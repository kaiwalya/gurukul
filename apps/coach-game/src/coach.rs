//! AppCoach wiring + cross-state event drain.
//!
//! The Coach handle is constructed once at startup and lives as a
//! `NonSend` resource for the entire app lifetime — it's `!Send` on
//! macOS (CoreAudio CaptureSession). Session lifecycle (Start/Stop) is
//! driven by state transitions; this module only owns construction
//! and the always-on event drain.

use crate::state::KnownDevices;
use bevy::prelude::*;
use domain_ports::app_coach::{AppCoach, AppCoachDeps, CoachEvent};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::sync::Arc;

pub struct Coach(pub Box<dyn AppCoach>);

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

/// Always-on event drain. Splits `DevicesListed` into the
/// `KnownDevices` resource so the Settings → Audio screen can render
/// it; surfaces lifecycle / errors / drops as logs.
pub fn drain_events(coach: NonSend<Coach>, mut known: ResMut<KnownDevices>) {
    let mut events = Vec::new();
    coach.0.poll_events(&mut events);
    for ev in events {
        match ev {
            CoachEvent::DevicesListed { devices } => {
                known.0 = devices;
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
            // The musical frame was (re)configured. The HUD reads the
            // published `music_info` snapshot directly each frame, so
            // there's nothing to fold here — just trace it.
            CoachEvent::SessionConfigured { tuning, tonality } => {
                info!("session configured: {tuning:?} / {tonality:?}");
            }
        }
    }
}
