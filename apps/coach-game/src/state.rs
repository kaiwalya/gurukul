//! App-wide state and player-facing settings resources.
//!
//! The state machine is intentionally flat (MainMenu / Settings / InGame /
//! Paused). Settings doesn't need a sub-state for the audio tab yet —
//! there's only one tab. Promote to a SubStates when a second tab appears.

use bevy::prelude::*;
use domain_ports::audio_devices::{DeviceId, InputDevice};
use domain_ports::pitch::PitchLog2;
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

#[derive(States, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[states(scoped_entities)]
pub enum AppState {
    #[default]
    MainMenu,
    Settings,
    InGame,
    /// Esc-pause overlay drawn on top of (but mutually exclusive with)
    /// InGame. Entering Paused stops the AppCoach session; resuming
    /// starts a fresh one. See `HasPausedSession` for the menu-label
    /// gate that lets MainMenu show "Continue".
    Paused,
}

/// True while there's a paused session the player can resume. Set by
/// `OnEnter(Paused)`, cleared when the player Quits-to-Main from the
/// pause overlay (which truly ends the run). Drives the main menu's
/// Continue/Free Practice label.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct HasPausedSession(pub bool);

/// True when the Pause screen's Resume action must be disabled because the
/// session was interrupted by the OS (phone call, Siri, etc.) and the
/// interruption has not yet ended with `should_resume: true`.
///
/// Set to `true` by `drain_events` on `AudioInterruption { Began }`.
/// Cleared to `false` on `AudioInterruption { Ended { should_resume: true } }`.
/// When the user presses Escape to pause (not an OS interruption) this is
/// `false` — Resume is enabled.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ResumeLocked(pub bool);

/// User-chosen input device, persists across menu navigation. `None`
/// means "use OS default" — what AppCoach's `AudioConfig::device_id`
/// also treats as default.
#[derive(Resource, Default, Debug, Clone)]
pub struct SelectedDevice(pub Option<DeviceId>);

/// The most recent device list from `CoachEvent::AudioDevicesListed`. The
/// Settings → Audio screen populates it on enter (via `AudioListDevices`)
/// and renders one row per device.
#[derive(Resource, Default)]
pub struct KnownDevices(pub Vec<InputDevice>);

/// The most recent scale catalogue from [`CoachEvent::MusicScalesListed`].
/// Populated by [`drain_events`](crate::coach::drain_events) in response
/// to [`Command::MusicListScales`] — the read side of the CQRS split, same
/// pattern as [`KnownDevices`]. Empty until the first `MusicListScales` reply
/// arrives (honest absence: the picker waits for `KnownScales` to be
/// non-empty before rendering rows).
#[derive(Resource, Default)]
pub struct KnownScales(pub Vec<ScaleIntervals>);

/// Calibration layer for the musical UI. Settings the user sets once
/// and forgets — the harmonium-maker's choices, not the singer's.
/// Per-song choices (tonic, scale) live in the separate `SongTonality`
/// resource.
///
/// Why split this out: the dial's tick marks and the needle math all
/// derive from this. Keeping it as one resource means a future Settings
/// UI has one struct to edit.
///
/// This struct is deliberately **vocabulary-free**: it holds the raw
/// musical inputs (`Hz`, a [`TuningKind`]), never a note-naming scheme.
/// Naming the dial face or the tonic is a separate, deferred label
/// layer — see `docs/MUSIC_MODEL.md`. Until it ships, the head shows
/// the *math view* (degrees / keys / Hz), not invented note names.
#[derive(Resource, Debug, Clone)]
pub struct AppSettings {
    /// Calibration anchor in Hz — the frequency the tuning root key
    /// maps to. 440.0 is standard concert pitch; 432, 442, 415 are the
    /// common alternatives. Only the Hz is stored; no note name is
    /// attached (that's the deferred label layer's job).
    pub reference_hz: f32,

    /// How the 12 slot positions on the octave circle are computed.
    /// This is the port's [`TuningKind`] stored directly — the head
    /// invents no parallel enum. 12-TET is evenly spaced; Hindustani
    /// Just uses 5-limit ratios.
    pub tuning_kind: TuningKind,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            reference_hz: 440.0,
            tuning_kind: TuningKind::TwelveTet,
        }
    }
}

/// The song's musical frame, head-side: a fully-placed [`Scale`] (tooth
/// pattern + rotated tuning carrying Sa + integer octave). The head holds
/// the same value it sends the coach via
/// [`Command::MusicConfigureSession`](domain_ports::app_coach::Command), and the
/// dial reads it to paint the scale ring.
///
/// The dial's *geometry* (how many slots, where they sit) lives in the
/// `Scale`'s tuning; the mask says *which slots are in-scale*, and the
/// tuning's rotation + octave say *where Sa is*. Today it defaults to
/// Bilawal on C — there's no per-song picker yet. When songs land, this is
/// what they write to.
#[derive(Resource, Debug, Clone, Copy)]
pub struct SongTonality(pub Scale);

impl Default for SongTonality {
    fn default() -> Self {
        // Bilawal thaat (Sa Re Ga Ma Pa Dha Ni) — same intervals as the
        // Western major scale (2+2+1+2+2+2+1). Built on the default A=440
        // 12-TET calibration, with Sa on C one octave below the reference.
        Self(AppSettings::default().song_scale(
            ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
            SA_ON_C_SHIFT,
            SA_ON_C_OCTAVE,
        ))
    }
}

/// Semitones from the A=440 reference line up to C: A→C is +3 on the
/// 12-TET circle, so Sa-on-C is `shift_up(3)` of the reference tuning.
pub const SA_ON_C_SHIFT: usize = 3;

/// The helix floor C sits on, one octave below A=440 (C ≈ 262 Hz, so
/// `floor(log2 262) = 8`). ORIGIN is 1 Hz, so the register lives in this
/// integer octave, not in the rotation (which keeps only the pitch class).
pub const SA_ON_C_OCTAVE: i32 = 8;

impl AppSettings {
    /// The reference-anchored tuning: this calibration's [`TuningKind`]
    /// shape rotated so its slot 0 is the `reference_hz` pitch class (the
    /// "A=" line). The rotation carries only the reference's octave-free
    /// residue; the octave a song sits in is the `Scale`'s integer floor.
    ///
    /// This is the bare cylinder a song then re-bases (`shift_up` to put Sa
    /// on its tonic) and places at a register — see [`song_scale`].
    ///
    /// [`song_scale`]: AppSettings::song_scale
    pub fn tuning_absolute(&self) -> TuningAbsolute {
        TuningAbsolute::at_reference(
            self.tuning_kind.intervals(),
            PitchLog2::from_hz(self.reference_hz),
        )
    }

    /// Place a scale on this calibration: the tooth pattern `intervals`
    /// dropped onto the reference tuning re-based so Sa is `sa_shift` slots
    /// above the reference line, at register `octave` (helix floor of Sa).
    pub fn song_scale(&self, intervals: ScaleIntervals, sa_shift: usize, octave: i32) -> Scale {
        Scale::new(intervals, self.tuning_absolute().shift_up(sa_shift), octave)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_settings_default_matches_design() {
        let s = AppSettings::default();
        assert_eq!(s.reference_hz, 440.0);
        assert_eq!(s.tuning_kind, TuningKind::TwelveTet);
    }

    #[test]
    fn song_tonality_default_is_bilawal_on_c() {
        let SongTonality(scale) = SongTonality::default();
        // Bilawal degrees on the 12-slot grid.
        assert_eq!(scale.intervals().degree_slots(), [0, 2, 4, 5, 7, 9, 11]);
        // 12-TET tuning underneath.
        assert_eq!(scale.tuning().len(), 12);
        // Sa resolves to C ≈ 262 Hz (one octave below the A=440 reference).
        let sa = scale.pitch_at(0).to_hz();
        assert!((sa - 261.63).abs() < 1.0, "Sa should be middle C, got {sa}");
    }

    #[test]
    fn tuning_absolute_pegs_slot_zero_at_reference_hz() {
        // Default settings: A=440, 12-TET. Slot 0 of the reference tuning
        // is the A=440 pitch class.
        let t = AppSettings::default().tuning_absolute();
        assert_eq!(t.len(), 12);
        // Resolving 440 Hz lands on slot 0.
        let (slot, _oct) = t.resolve(PitchLog2::from_hz(440.0));
        assert_eq!(slot, 0, "the reference Hz must sit on slot 0");
    }
}
