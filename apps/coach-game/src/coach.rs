//! AppCoach wiring + cross-state event drain.
//!
//! The Coach handle is constructed once at startup and lives as a
//! `NonSend` resource for the entire app lifetime — it's `!Send` on
//! macOS (CoreAudio CaptureSession). Session lifecycle (Start/Stop) is
//! driven by state transitions; this module only owns construction
//! and the always-on event drain.

use crate::state::{KnownDevices, KnownScales};
use bevy::prelude::*;
use domain_ports::app_coach::{AppCoach, AppCoachDeps, CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::sync::Arc;

pub struct Coach(pub Box<dyn AppCoach>);

/// The head-side **read model** for the coach's musical frame of
/// reference (tuning + tonality). Written *only* by [`drain_events`] in
/// response to a [`CoachEvent::SessionConfigured`]; read by every UI
/// system (HUD, dial) via `Res<MusicInfoRes>`.
///
/// This is the read side of the CQRS split: UI never holds the
/// [`Coach`] handle to *read* config — writes go out as `Command`s, the
/// truth comes back as an event that refreshes this resource. `None`
/// until the first session is configured (honest absence, like the HUD's
/// `—` placeholder).
#[derive(Resource, Default)]
pub struct MusicInfoRes(pub Option<MusicInfo>);

/// The head-side **read model** for the latest live feature snapshot
/// (`f0`, onset, breath, vibrato). Polled from the coach every tick by
/// [`drain_events`] and republished here so UI systems read a plain
/// `Res<LatestFeatures>` instead of holding the [`Coach`] handle.
///
/// Unlike [`MusicInfoRes`] this is a high-rate poll (no per-sample
/// event), so it refreshes every frame rather than on an event.
#[derive(Resource, Default)]
pub struct LatestFeatures(pub Option<FeatureSnapshot>);

pub fn spawn_coach(world: &mut World) {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));
    let audio_devices = Arc::new(adapter_audio_cpal::new_devices());
    let audio_capture = Arc::new(adapter_audio_cpal::new_capture(Arc::clone(&clock)));

    let coach = adapter_app_coach::new(AppCoachDeps {
        clock,
        telemetry,
        audio_devices,
        audio_capture,
        host_version: env!("CARGO_PKG_VERSION"),
    });
    world.insert_non_send_resource(Coach(Box::new(coach)));
}

/// On `AppExit`, synchronously tear down the coach so its control
/// plane thread and any open audio stream end before the process
/// returns from `main`. Without this, hitting Quit (or closing the
/// window) leaves the renderer gone but the AppCoach background
/// thread still running — terminal never returns to the prompt.
///
/// 2-second timeout matches what a clean teardown takes plus some
/// slack; on timeout the coach detaches and logs via telemetry.
pub fn shutdown_on_exit(mut exits: MessageReader<bevy::app::AppExit>, coach: NonSend<Coach>) {
    if exits.read().next().is_some() {
        let result = coach.0.shutdown(std::time::Duration::from_secs(2));
        info!("coach shutdown: {result:?}");
    }
}

/// Always-on read-side sync: the **single** system that holds the
/// [`Coach`] handle to *read* from it. Drains events and polls features,
/// republishing both into resources the UI reads. Confining all reads
/// here keeps the `!Send` handle off every render system (which can then
/// run as ordinary `Res` readers).
///
/// - `DevicesListed` → [`KnownDevices`] (Settings → Audio screen).
/// - `SessionConfigured` → refresh [`MusicInfoRes`] from `music_info()`
///   (the read side of the config CQRS round-trip).
/// - lifecycle / errors / drops → logs.
/// - every tick → poll `latest_features()` into [`LatestFeatures`].
pub fn drain_events(
    coach: NonSend<Coach>,
    mut known: ResMut<KnownDevices>,
    mut scales: ResMut<KnownScales>,
    mut music: ResMut<MusicInfoRes>,
    mut features: ResMut<LatestFeatures>,
) {
    let mut events = Vec::new();
    coach.0.poll_events(&mut events);
    for ev in events {
        match ev {
            CoachEvent::DevicesListed { devices } => {
                known.0 = devices;
            }
            CoachEvent::ScalesListed { shapes } => {
                scales.0 = shapes;
            }
            CoachEvent::SessionStateChanged { new_state } => {
                info!("session state: {new_state:?}");
            }
            CoachEvent::SessionError { kind, reason } => {
                error!("session error: {kind:?} — {reason}");
            }
            CoachEvent::EventsDropped { count } => {
                warn!("events dropped: {count}");
            }
            CoachEvent::DefaultInputChanged { .. } => {}
            // The musical frame was (re)configured. Pull the fresh
            // snapshot and republish it for the UI to read.
            CoachEvent::SessionConfigured { tuning, tonality } => {
                // Compact one-liner: the `Tonality` Debug dumps all 32 width
                // slots (mostly 0-terminator padding), so format the active
                // ones by hand instead.
                let widths = tonality
                    .widths()
                    .iter()
                    .map(|w| format!("{:.0}", w.0))
                    .collect::<Vec<_>>()
                    .join(" ");
                info!(
                    "session configured: {:?} {:.0}Hz root={:.0} / tonic={:.0} [{}]",
                    tuning.kind,
                    tuning.root_note_hz,
                    tuning.root.offset,
                    tonality.tonic.offset,
                    widths,
                );
                music.0 = coach.0.music_info();
            }
        }
    }
    // Live features have no per-sample event — poll each tick.
    features.0 = coach.0.latest_features();
}
