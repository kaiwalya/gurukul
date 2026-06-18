//! Auto-return-to-menu when a `--replay-audio` WAV finishes.
//!
//! Only active on the `--replay-audio` path. The live-mic and headless-test
//! paths never receive these systems.
//!
//! Mechanism: `detect_wav_end` runs `.after(coach::drain_events)` and reads
//! `FeatureDrainCount` (the number of feature hops drained this frame).
//! Once the WAV has been seen flowing (≥1 hop since entering InGame) and then
//! goes silent for `WAV_END_THRESHOLD_SECS`, it sets `NextState(MainMenu)`.
//! `OnExit(InGame)` already sends `Command::AudioStopSession`, so no extra cleanup
//! is needed.

use crate::coach::FeatureDrainCount;
use crate::state::AppState;
use bevy::prelude::*;

/// Sustained silence required before declaring the WAV finished (wall time).
pub const WAV_END_THRESHOLD_SECS: f32 = 1.0;

/// Transient state for the WAV-end detector. Inserted only on the
/// `--replay-audio` path and reset on every `OnEnter(InGame)` so
/// re-entering the game after an auto-return works correctly.
#[derive(Resource, Default)]
pub struct ReplayAudioEnd {
    /// True once at least one feature hop has been drained in this session.
    /// Gates out pre-roll silence so we don't immediately return to menu
    /// before the WAV has started producing audio.
    pub seen_audio: bool,
    /// Accumulated wall-clock seconds during which drain_count was zero,
    /// after `seen_audio` became true.
    pub silent_secs: f32,
}

/// Reset the detector on every `OnEnter(InGame)` so a user who re-enters
/// the game after an auto-return (or after a Pause/Resume) doesn't bounce
/// straight back to the menu.
pub fn reset_detector(mut end: ResMut<ReplayAudioEnd>) {
    *end = ReplayAudioEnd::default();
}

/// Detect WAV exhaustion and transition to `MainMenu`.
///
/// Must run `.after(coach::drain_events)` so `FeatureDrainCount` reflects
/// this frame's drain, and `.run_if(in_state(AppState::InGame))` so
/// accumulated silence never bleeds into the Paused state.
pub fn detect_wav_end(
    count: Res<FeatureDrainCount>,
    time: Res<Time>,
    mut end: ResMut<ReplayAudioEnd>,
    mut next: ResMut<NextState<AppState>>,
) {
    if count.0 > 0 {
        end.seen_audio = true;
        end.silent_secs = 0.0;
    } else if end.seen_audio {
        end.silent_secs += time.delta_secs();
        if end.silent_secs >= WAV_END_THRESHOLD_SECS {
            next.set(AppState::MainMenu);
        }
    }
    // If !seen_audio && count == 0: pre-roll silence — do nothing.
}
