//! AppCoach wiring + cross-state event drain.
//!
//! The Coach handle is constructed once at startup and lives as a
//! `NonSend` resource for the entire app lifetime — it's `!Send` on
//! macOS (CoreAudio CaptureSession). Session lifecycle (Start/Stop) is
//! driven by state transitions; this module only owns construction
//! and the always-on event drain.

use crate::state::{KnownDevices, KnownScales};
use bevy::prelude::*;
use domain_ports::app_coach::{AppCoach, AppCoachDeps, CoachEvent, MusicInfo};
use domain_ports::clock::Clock;
use domain_ports::pitch::PitchLog2;
use domain_ports::telemetry::Telemetry;
use domain_ports::tuning::Tuning;
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

/// The game-side live features: the port's `FeatureSnapshot` with `f0`
/// already lifted out of raw Hz into a [`PitchLog2`] (`None` = unvoiced,
/// retiring the port's `f0_hz == 0.0` sentinel). The Hz→pitch conversion
/// happens **once**, at the poll seam in [`drain_events`], so no game
/// system below ever touches a raw frequency — they read a `PitchLog2`
/// and feed it straight into the scale geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Features {
    /// The detected pitch, or `None` when the frame is unvoiced (silence,
    /// breath, noise — the port's `f0_hz <= 0.0`).
    pub pitch: Option<PitchLog2>,
    /// YIN periodicity confidence, `0.0..=1.0` (carried through verbatim).
    pub confidence: f32,
    /// Onset detector output (positive on attack).
    pub onset: f32,
    /// Breath / aspiration energy estimate.
    pub breath: f32,
    /// Vibrato rate in Hz over the recent window.
    pub vibrato_rate: f32,
    /// Vibrato depth in semitones.
    pub vibrato_depth: f32,
    /// Snapshot timestamp in ms (for de-duping repeats between polls).
    pub t_ms: u64,
}

/// The head-side **read model** for the latest live [`Features`] (`pitch`,
/// onset, breath, vibrato). Polled from the coach every tick by
/// [`drain_events`] and republished here so UI systems read a plain
/// `Res<LatestFeatures>` instead of holding the [`Coach`] handle.
///
/// Unlike [`MusicInfoRes`] this is a high-rate poll (no per-sample
/// event), so it refreshes every frame rather than on an event.
#[derive(Resource, Default)]
pub struct LatestFeatures(pub Option<Features>);

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
            CoachEvent::SessionConfigured { scale } => {
                // Compact one-liner: log Sa's pitch, the slot count, and the
                // in-scale degrees rather than dumping the whole `Scale`.
                info!(
                    "session configured: Sa={:.0}Hz on {}-slot grid, degrees {:?}",
                    scale.pitch_at(0).to_hz(),
                    scale.tuning().len(),
                    scale.intervals().degree_slots(),
                );
                music.0 = coach.0.music_info();
            }
        }
    }
    // Live features have no per-sample event — poll each tick. This is the
    // one seam where the game lifts f0 out of raw Hz: the port's snapshot
    // carries `f0_hz` (DSP's native unit), and we convert to PitchLog2 here
    // so no system below ever sees a frequency. The `f0_hz <= 0.0`
    // unvoiced sentinel becomes `pitch: None`.
    features.0 = coach.0.latest_features().map(|s| Features {
        pitch: (s.f0_hz > 0.0).then(|| PitchLog2::from_hz(s.f0_hz)),
        confidence: s.confidence,
        onset: s.onset,
        breath: s.breath,
        vibrato_rate: s.vibrato_rate,
        vibrato_depth: s.vibrato_depth,
        t_ms: s.t_ms,
    });
}
