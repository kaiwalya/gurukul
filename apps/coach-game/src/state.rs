//! App-wide state and player-facing settings resources.
//!
//! The state machine is intentionally flat (MainMenu / Settings / InGame /
//! Paused). Settings doesn't need a sub-state for the audio tab yet —
//! there's only one tab. Promote to a SubStates when a second tab appears.

use bevy::prelude::*;
use domain_ports::audio_devices::{DeviceId, InputDevice};
use domain_ports::music::{harmonium_key, Tonality, TuningKind, TuningSpec};

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
/// Continue/New Game label.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct HasPausedSession(pub bool);

/// User-chosen input device, persists across menu navigation. `None`
/// means "use OS default" — what AppCoach's `AudioConfig::device_id`
/// also treats as default.
#[derive(Resource, Default, Debug, Clone)]
pub struct SelectedDevice(pub Option<DeviceId>);

/// The most recent device list from `CoachEvent::DevicesListed`. The
/// Settings → Audio screen populates it on enter (via `ListDevices`)
/// and renders one row per device.
#[derive(Resource, Default)]
pub struct KnownDevices(pub Vec<InputDevice>);

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

/// The song's musical frame of reference, head-side. Wraps the port's
/// [`Tonality`] (the singer's Sa key + scale-interval shape) so it can
/// be a Bevy [`Resource`] — the head holds the same value it sends the
/// coach via [`Command::ConfigureSession`](domain_ports::app_coach::Command),
/// and the dial reads it to paint the scale ring.
///
/// The dial's *geometry* (how many slots, where they sit) comes from
/// [`AppSettings`]; this resource says *which slots are in-scale* and
/// *where Sa is* for the current song. Today it defaults to Bilawal on
/// Safed-1 (C) — there's no per-song picker yet. When songs land, this
/// is what they write to.
#[derive(Resource, Debug, Clone, Copy)]
pub struct SongTonality(pub Tonality);

impl Default for SongTonality {
    fn default() -> Self {
        // Bilawal thaat (Sa Re Ga Ma Pa Dha Ni) on C — same intervals
        // as the Western major scale (2+2+1+2+2+2+1). Sa sits on C in
        // octave 1 (`harmonium_key(12)`), one octave below the A=440
        // tuning root (`harmonium_key(21)`), so the scale resolves to
        // the middle register (C ≈ 262 Hz) rather than the lowest
        // octave on the line.
        Self(Tonality::new(harmonium_key(12), &[2, 2, 1, 2, 2, 2, 1]))
    }
}

impl AppSettings {
    /// The flat tuning inputs the coach needs to build a
    /// [`Tuning`](domain_ports::music::Tuning), derived from the user's
    /// calibration settings.
    ///
    /// Convention: `reference_hz` is the "A=" anchor, so the tuning root
    /// is the A key in octave 1 (`harmonium_key(21)`) and `root_note_hz`
    /// is `reference_hz` directly — A sits at slot 0. The root lives in
    /// octave 1 (not the lowest octave) so a song tonic an octave below
    /// it lands in the singing register rather than the cellar.
    pub fn tuning_spec(&self) -> TuningSpec {
        TuningSpec {
            root_note_hz: self.reference_hz,
            kind: self.tuning_kind,
            root: harmonium_key(21),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::music::KeyInterval;

    #[test]
    fn app_settings_default_matches_design() {
        let s = AppSettings::default();
        assert_eq!(s.reference_hz, 440.0);
        assert_eq!(s.tuning_kind, TuningKind::TwelveTet);
    }

    #[test]
    fn song_tonality_default_is_bilawal_on_c_octave_1() {
        let SongTonality(t) = SongTonality::default();
        // Sa on C in octave 1 — one octave below the A=440 root, so the
        // scale resolves to the middle register.
        assert_eq!(t.tonic, harmonium_key(12));
        assert_eq!(
            t.widths(),
            &[
                KeyInterval(2),
                KeyInterval(2),
                KeyInterval(1),
                KeyInterval(2),
                KeyInterval(2),
                KeyInterval(2),
                KeyInterval(1)
            ]
        );
        // Well-formed against a 12-slot tuning.
        assert!(t.well_formed(12));
    }

    #[test]
    fn tuning_spec_pegs_root_at_a_with_reference_hz() {
        // Default settings: A=440, 12-TET. The tuning root is the A key
        // in octave 1 (offset 21) and root_note_hz is the reference
        // directly.
        let spec = AppSettings::default().tuning_spec();
        assert_eq!(spec.root, harmonium_key(21));
        assert_eq!(spec.root_note_hz, 440.0);
        assert_eq!(spec.kind, TuningKind::TwelveTet);
    }
}
