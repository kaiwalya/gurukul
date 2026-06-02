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

/// Calibration + naming layer for the musical UI. Settings the user
/// sets once and forgets — the harmonium-maker's choices, not the
/// singer's. Per-song choices (tonic, scale) live in the separate
/// `SongTonality` resource.
///
/// Why split this out: the dial's tick marks, the needle math, and the
/// labels rendered on the dial all derive from this. Keeping it as one
/// resource means a future Settings UI has one struct to edit.
#[derive(Resource, Debug, Clone)]
pub struct AppSettings {
    /// Calibration anchor in Hz. By convention this is "A=" — i.e. the
    /// frequency the Western note A maps to. 440.0 is standard concert
    /// pitch; 432, 442, 415 are the common alternatives. The note name
    /// ("A") is presentation, derived from [`NoteSystem`] at render
    /// time; only the Hz is stored.
    pub reference_hz: f32,

    /// How the 12 slot positions on the octave circle are computed.
    /// 12-TET is evenly spaced; Hindustani Just uses 5-limit ratios.
    /// Tuning system is independent of note system: a Sargam user can
    /// still sing in 12-TET, and a Western user can experiment with
    /// Just intonation.
    pub tuning_system: TuningSystem,

    /// Vocabulary used for note labels on the dial and for displaying
    /// the reference frequency. Western renders the reference as
    /// "A = 440 Hz"; Sargam users typically don't anchor labels to a
    /// fixed Hz (Sa = whatever the tonic is today), so the display
    /// collapses to "440 Hz".
    pub note_system: NoteSystem,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            reference_hz: 440.0,
            tuning_system: TuningSystem::TwelveTET,
            note_system: NoteSystem::SargamLatin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuningSystem {
    TwelveTET,
    HindustaniJust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSystem {
    Western,
    SargamLatin,
    SargamDevanagari,
}

impl NoteSystem {
    /// Absolute label for the song's tonic at a given chromatic slot.
    /// This is **Job B** in `docs/MUSIC_MODEL.md` — naming the tonic
    /// in HUDs and pickers, not labelling the dial face. Out-of-range
    /// slots return "?".
    ///
    /// - **Western** uses absolute pitch names (C, C♯, D, ...). The
    ///   same vocabulary also doubles as the dial label ring (Job A)
    ///   when the label ring is rendered, because Western's
    ///   absolute-Hz lock mode happens to make Job A and Job B share
    ///   a table.
    /// - **Sargam** (both Latin and Devanagari) uses **harmonium key
    ///   positions**: Safed (white) and Kaali (black), numbered
    ///   within each colour across an octave. A Hindustani singer
    ///   announces "Kaali-1 Bilawal" the way a Western singer
    ///   announces "C♯ Major" — both name the absolute key the tonic
    ///   sits on, plus the scale. The dial face (Job A) renders
    ///   Sa/Re/Ga/... separately, locked to the tonic; that is **not**
    ///   this table.
    pub fn tonic_label(self, slot: u8) -> &'static str {
        const WESTERN: [&str; 12] = [
            "C", "C♯", "D", "E♭", "E", "F", "F♯", "G", "A♭", "A", "B♭", "B",
        ];
        // Harmonium key positions: Safed = white, Kaali = black. The
        // numbering counts within each colour as you walk up from
        // Safed-1 (= C):
        //   C  C♯  D  E♭ E  F  F♯ G  A♭ A  B♭ B
        //   S1 K1  S2 K2 S3 S4 K3 S5 K4 S6 K5 S7
        // Shared between SargamLatin and SargamDevanagari because the
        // names are loanwords with no widely-used Devanagari form for
        // the harmonium-position vocabulary; numerals stay Arabic.
        const HARMONIUM: [&str; 12] = [
            "Safed-1", "Kaali-1", "Safed-2", "Kaali-2", "Safed-3", "Safed-4", "Kaali-3", "Safed-5",
            "Kaali-4", "Safed-6", "Kaali-5", "Safed-7",
        ];
        let table = match self {
            NoteSystem::Western => &WESTERN,
            NoteSystem::SargamLatin | NoteSystem::SargamDevanagari => &HARMONIUM,
        };
        table.get(slot as usize).copied().unwrap_or("?")
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
        // Bilawal thaat (Sa Re Ga Ma Pa Dha Ni) on the first white key
        // — same intervals as the Western major scale (2+2+1+2+2+2+1).
        Self(Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]))
    }
}

impl AppSettings {
    /// The flat tuning inputs the coach needs to build a
    /// [`Tuning`](domain_ports::music::Tuning), derived from the user's
    /// calibration settings.
    ///
    /// Convention: `reference_hz` is the "A=" anchor, so the tuning root
    /// is the A key (`harmonium_key(9)`) and `root_note_hz` is
    /// `reference_hz` directly — A sits at slot 0.
    pub fn tuning_spec(&self) -> TuningSpec {
        TuningSpec {
            root_note_hz: self.reference_hz,
            kind: match self.tuning_system {
                TuningSystem::TwelveTET => TuningKind::TwelveTet,
                TuningSystem::HindustaniJust => TuningKind::HindustaniJust,
            },
            root: harmonium_key(9),
        }
    }
}

/// Look up a display name for a step-vector scale in the user's note
/// system. Sargam users see Indian thaat names ("Bilawal"); Western
/// users see Western mode names ("Major"). Unknown vectors fall back
/// to the literal step list — obvious-placeholder rendering, so we
/// notice when a new scale lands without a label.
///
/// This is a stub for the real scale catalogue. Three entries today
/// (Bilawal/Major, Yaman/Lydian, Bhairav/double-harmonic) cover what
/// the HUD will plausibly show before the catalogue ships.
pub fn scale_name(steps: &[u8], note_system: NoteSystem) -> String {
    let western = matches!(note_system, NoteSystem::Western);
    match steps {
        [2, 2, 1, 2, 2, 2, 1] => if western { "Major" } else { "Bilawal" }.to_string(),
        [2, 2, 2, 1, 2, 2, 1] => if western { "Lydian" } else { "Kalyan" }.to_string(),
        [1, 3, 1, 2, 1, 3, 1] => if western {
            "Double harmonic"
        } else {
            "Bhairav"
        }
        .to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_settings_default_matches_design() {
        let s = AppSettings::default();
        assert_eq!(s.reference_hz, 440.0);
        assert_eq!(s.tuning_system, TuningSystem::TwelveTET);
        assert_eq!(s.note_system, NoteSystem::SargamLatin);
    }

    #[test]
    fn song_tonality_default_is_bilawal_on_safed_1() {
        let SongTonality(t) = SongTonality::default();
        assert_eq!(t.tonic, harmonium_key(0));
        assert_eq!(t.steps(), &[2, 2, 1, 2, 2, 2, 1]);
        // Well-formed against a 12-slot tuning.
        assert!(t.well_formed(12));
    }

    #[test]
    fn tuning_spec_pegs_root_at_a_with_reference_hz() {
        // Default settings: A=440, 12-TET. The tuning root is the A key
        // (offset 9) and root_note_hz is the reference directly.
        let spec = AppSettings::default().tuning_spec();
        assert_eq!(spec.root, harmonium_key(9));
        assert_eq!(spec.root_note_hz, 440.0);
        assert_eq!(spec.kind, TuningKind::TwelveTet);
    }

    #[test]
    fn western_tonic_label_is_absolute_pitch_name() {
        // Western names the tonic by its absolute chromatic pitch.
        assert_eq!(NoteSystem::Western.tonic_label(0), "C");
        assert_eq!(NoteSystem::Western.tonic_label(1), "C♯");
        assert_eq!(NoteSystem::Western.tonic_label(2), "D");
        assert_eq!(NoteSystem::Western.tonic_label(11), "B");
    }

    #[test]
    fn sargam_tonic_label_is_harmonium_position() {
        // Sargam announces the tonic by harmonium key position
        // (Safed = white, Kaali = black), not as "Sa" — Sa is always
        // Sa, so it tells the singer nothing. The specific case in
        // the docs: a tonic on key 1 reads "Kaali-1".
        assert_eq!(NoteSystem::SargamLatin.tonic_label(0), "Safed-1");
        assert_eq!(NoteSystem::SargamLatin.tonic_label(1), "Kaali-1");
        assert_eq!(NoteSystem::SargamLatin.tonic_label(2), "Safed-2");
        assert_eq!(NoteSystem::SargamLatin.tonic_label(3), "Kaali-2");
        assert_eq!(NoteSystem::SargamLatin.tonic_label(11), "Safed-7");
        // Devanagari shares the harmonium-position table.
        assert_eq!(NoteSystem::SargamDevanagari.tonic_label(1), "Kaali-1");
    }

    #[test]
    fn tonic_label_out_of_range_returns_placeholder() {
        assert_eq!(NoteSystem::Western.tonic_label(12), "?");
        assert_eq!(NoteSystem::SargamLatin.tonic_label(255), "?");
    }
}
