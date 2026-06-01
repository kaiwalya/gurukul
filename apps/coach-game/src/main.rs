//! coach-game: Bevy host for the singing-coach.
//!
//! First-pixel parity bar: open the mic via AppCoach, log each fresh
//! feature snapshot to stdout from a Bevy system. No rendering yet —
//! just prove the AppCoach handle survives inside Bevy's schedule and
//! features arrive at frame rate.

use bevy::prelude::*;
use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, CoachEvent, Command, FeatureSnapshot, SessionConfig,
};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::sync::Arc;
use std::time::Duration;

/// AppCoach is `!Send` (CoreAudio CaptureSession is `!Send` on macOS),
/// so it lives as a `NonSend` resource pinned to Bevy's main thread.
/// `Box<dyn AppCoach>` lets us own the concrete impl behind the trait.
struct Coach(Box<dyn AppCoach>);

/// Last published feature snapshot timestamp. Used to detect fresh
/// snapshots between frames — the data plane publishes at ~85Hz while
/// Bevy ticks at 60Hz+, so we'll see most snapshots but want to skip
/// repeats in the log.
#[derive(Resource, Default)]
struct LastFeatureTs(u64);

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .init_resource::<LastFeatureTs>()
        .add_systems(Startup, (spawn_coach, start_session).chain())
        .add_systems(Update, (drain_events, log_features))
        .run();
}

fn spawn_coach(world: &mut World) {
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

fn start_session(coach: NonSend<Coach>) {
    coach.0.send_command(Command::StartSession(SessionConfig {
        device_id: None,
        sample_rate: None,
        buffer_frames: None,
    }));
}

fn drain_events(coach: NonSend<Coach>) {
    let mut events = Vec::new();
    coach.0.poll_events(&mut events);
    for ev in events {
        match ev {
            CoachEvent::SessionStateChanged { new_state } => {
                info!("session state: {new_state:?}");
            }
            CoachEvent::SessionError { kind, reason } => {
                error!("session error: {kind:?} — {reason}");
            }
            CoachEvent::EventsDropped { count } => {
                warn!("events dropped: {count}");
            }
            CoachEvent::DevicesListed { .. } | CoachEvent::DefaultInputChanged { .. } => {}
        }
    }
}

fn log_features(coach: NonSend<Coach>, mut last: ResMut<LastFeatureTs>) {
    let Some(FeatureSnapshot {
        f0_hz,
        onset,
        breath,
        vibrato_rate,
        vibrato_depth,
        t_ms,
    }) = coach.0.latest_features()
    else {
        return;
    };
    if t_ms == last.0 {
        return;
    }
    last.0 = t_ms;
    let f0_str = if f0_hz > 0.0 {
        format!("{f0_hz:7.2} Hz")
    } else {
        "    --    ".to_string()
    };
    let onset_marker = if onset > 0.0 { "•" } else { " " };
    info!(
        "t={t_ms:>8}ms  f0 {f0_str}  br {breath:>4.2}  vib {vibrato_rate:>4.1}Hz/{vibrato_depth:>4.2}st  {onset_marker}"
    );
}

/// Bevy doesn't have a single "on app exit" hook that's both `NonSend`
/// and reliable across platforms; relying on `Drop` of the `Coach`
/// resource handles teardown. `CoachImpl::Drop` calls `shutdown(ZERO)`
/// when the head forgot to do so — sufficient for the first-pixel pass.
#[allow(dead_code)]
fn _shutdown_note() {
    let _ = SHUTDOWN_TIMEOUT;
}
