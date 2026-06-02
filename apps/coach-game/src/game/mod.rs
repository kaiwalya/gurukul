//! InGame state: open a session with the selected device, stream
//! features. Features print to stdout (see [`log_features`]) and feed
//! the note dial overlay (see [`dial`]).

pub mod dial;
pub mod hud;

use crate::coach::Coach;
use crate::state::{AppSettings, AppState, HasPausedSession, SelectedDevice, SongTonality};
use bevy::prelude::*;
use domain_ports::app_coach::{AudioConfig, Command, FeatureSnapshot};

#[derive(Resource, Default)]
pub struct LastFeatureTs(u64);

pub fn on_enter(
    coach: NonSend<Coach>,
    selected: Res<SelectedDevice>,
    settings: Res<AppSettings>,
    tonality: Res<SongTonality>,
    mut has_paused: ResMut<HasPausedSession>,
) {
    // Configure the musical frame of reference *before* starting audio,
    // so the coach holds the tuning + tonality the moment a session is
    // live. The two are decoupled (configure causes no state change),
    // but configuring first means the reference is never momentarily
    // absent while Running.
    coach.0.send_command(Command::ConfigureSession {
        tuning: settings.tuning_spec(),
        tonality: tonality.0,
    });
    coach.0.send_command(Command::StartSession(AudioConfig {
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
