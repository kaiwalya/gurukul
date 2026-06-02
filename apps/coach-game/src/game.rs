//! InGame state: open a session with the selected device, stream
//! features. No rendering yet — features print to stdout, same as the
//! pre-menu scaffold.

use crate::coach::Coach;
use crate::state::{AppState, HasPausedSession, SelectedDevice};
use bevy::prelude::*;
use domain_ports::app_coach::{Command, FeatureSnapshot, SessionConfig};

#[derive(Resource, Default)]
pub struct LastFeatureTs(u64);

pub fn on_enter(
    coach: NonSend<Coach>,
    selected: Res<SelectedDevice>,
    mut has_paused: ResMut<HasPausedSession>,
) {
    coach.0.send_command(Command::StartSession(SessionConfig {
        device_id: selected.0.clone(),
        sample_rate: None,
        buffer_frames: None,
    }));
    // Whether we got here from MainMenu (New Game / Continue) or from
    // Paused (Resume), a session is now live and there's no separate
    // paused-session to keep around.
    has_paused.0 = false;
}

pub fn on_exit(coach: NonSend<Coach>, mut last: ResMut<LastFeatureTs>) {
    coach.0.send_command(Command::StopSession);
    last.0 = 0;
}

/// Esc in InGame → Paused (stops session via OnEnter(Paused)). Marks
/// `HasPausedSession` so the main menu can offer Continue.
pub fn handle_esc_in_game(
    keys: Res<ButtonInput<KeyCode>>,
    mut next: ResMut<NextState<AppState>>,
    mut has_paused: ResMut<HasPausedSession>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        has_paused.0 = true;
        next.set(AppState::Paused);
    }
}

/// Esc in Paused → InGame (starts a fresh session via OnEnter(InGame)).
pub fn handle_esc_paused(keys: Res<ButtonInput<KeyCode>>, mut next: ResMut<NextState<AppState>>) {
    if keys.just_pressed(KeyCode::Escape) {
        next.set(AppState::InGame);
    }
}

pub fn log_features(coach: NonSend<Coach>, mut last: ResMut<LastFeatureTs>) {
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
