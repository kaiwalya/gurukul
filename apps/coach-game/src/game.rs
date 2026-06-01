//! InGame state: open a session with the selected device, stream
//! features. No rendering yet — features print to stdout, same as the
//! pre-menu scaffold.

use crate::coach::Coach;
use crate::state::SelectedDevice;
use bevy::prelude::*;
use domain_ports::app_coach::{Command, FeatureSnapshot, SessionConfig};

#[derive(Resource, Default)]
pub struct LastFeatureTs(u64);

pub fn on_enter(coach: NonSend<Coach>, selected: Res<SelectedDevice>) {
    coach.0.send_command(Command::StartSession(SessionConfig {
        device_id: selected.0.clone(),
        sample_rate: None,
        buffer_frames: None,
    }));
}

pub fn on_exit(coach: NonSend<Coach>, mut last: ResMut<LastFeatureTs>) {
    coach.0.send_command(Command::StopSession);
    last.0 = 0;
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
