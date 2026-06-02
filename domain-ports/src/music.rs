//! The musical model: the value types that describe *how a session is
//! tuned* and *what the singer is singing* — the frame of reference the
//! coach judges pitch against.
//!
//! This module is **not** a port (no trait, no adapter). It is the
//! shared vocabulary the [`AppCoach`](crate::app_coach::AppCoach) port
//! carries across its boundary (via
//! [`Command::ConfigureSession`](crate::app_coach::Command)) and that
//! the coach holds internally. `docs/MUSIC_MODEL.md` is the canonical
//! statement of the model and the reasoning behind the split; read it
//! before changing anything here.
//!
//! # State vs View
//!
//! The types below are the **state layer** — dumb memory. They hold
//! bytes (offsets, a tonic, an interval list) and know how to be *born*
//! (constructors) and *validated*, but not how to be *read* as a number
//! line. The **view layer** — [`tuning_view`] — imposes the number-line
//! interpretation (keyboard↔slot↔Hz) on top.
//!
//! # Two coordinate spaces
//!
//! The single source of bugs here is that two different "which note"
//! indices look identical (both small integers). Keep them apart:
//!
//! - **Keyboard space** — a physical key of the instrument. What the
//!   player and singer *speak*: "Safed-1", "Kaali-1". Modelled by
//!   [`InstrumentKey`].
//! - **Slot space** — an index into a [`Tuning`]'s computed `slots`
//!   array, where `slots[0]` is the tuning's root note. What the tuning
//!   *math* works in. The bridge between the two is
//!   [`tuning_view::slot_of`].
//!
//! # FFI surface
//!
//! [`InstrumentKey`], [`TuningKind`], [`Tonality`], and [`TuningSpec`]
//! are flat and `Copy` — they cross the `AppCoach` command boundary.
//! [`Tuning`] is coach-internal (it owns a `Vec<f32>`) and never
//! crosses; it is *built* coach-side from a [`TuningSpec`].

/// The number of `scale_intervals` slots in a [`Tonality`]. A fixed cap
/// keeps `Tonality` flat and `Copy`; 32 covers any 12- or 22-slot scale
/// with room to spare.
pub const MAX_SCALE_INTERVALS: usize = 32;

/// A physical key of an instrument's keyboard.
///
/// Name-free and `Copy` (so it crosses the FFI boundary): the *name*
/// ("Safed-1" vs "C") is derived head-side from `offset` by the note
/// system, never stored here.
///
/// `offset` is a position on the **whole multi-octave line**;
/// `octave_size` (the keyboard's key count per octave — 12
/// harmonium/piano, 22 shruti) splits it. The within-octave position
/// and the octave are the two halves of one `divmod` — see [`fold`] and
/// [`octave`]. Carrying `octave_size` keeps the modular arithmetic
/// self-contained and makes a 12-key set impossible to mix silently
/// with a 22-key one.
///
/// [`fold`]: InstrumentKey::fold
/// [`octave`]: InstrumentKey::octave
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentKey {
    pub offset: u8,
    pub octave_size: u8,
}

impl InstrumentKey {
    /// Position within one octave: `0..octave_size`. The "fold to one
    /// octave" operation — `offset.rem_euclid(octave_size)`.
    pub fn fold(self) -> u8 {
        self.offset.rem_euclid(self.octave_size)
    }

    /// Which octave this key sits in — the part [`fold`](Self::fold)
    /// discards. `offset.div_euclid(octave_size)`.
    pub fn octave(self) -> i32 {
        (self.offset as i32).div_euclid(self.octave_size as i32)
    }
}

/// A key on a 12-key keyboard (harmonium / piano) at the given offset.
/// Stamps `octave_size = 12`. `harmonium_key(0)` is Safed-1 / C;
/// `harmonium_key(9)` is the A a tuner anchors "A=440" on.
pub fn harmonium_key(offset: u8) -> InstrumentKey {
    InstrumentKey {
        offset,
        octave_size: 12,
    }
}

/// How the slots of a tuning are spaced. Each kind declares its own
/// slot count `N` (via the length of [`shape`](TuningKind::shape)'s
/// output) and the rule for placing the slots.
///
/// `Copy` and flat: a `TuningKind` is part of the FFI-crossing
/// [`TuningSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuningKind {
    /// 12-tone equal temperament. `N = 12`, each slot `×2^(1/12)` from
    /// the last. Rotationally symmetric.
    TwelveTet,
    /// Hindustani Just intonation. `N = 12`, 5-limit ratios fanning from
    /// the root. Not symmetric — the `root` of the tuning is
    /// load-bearing here.
    HindustaniJust,
}

impl TuningKind {
    /// The `N` slot frequencies of this kind's fixed pattern, built
    /// **upward from the root note** `root_hz` at slot 0. `slots[0] ==
    /// root_hz` by construction; the rest fan up. Length `== N`.
    ///
    /// This is offset-free: it knows nothing about *which key* the root
    /// note sits on — that is the [`Tuning`]'s `root`, supplied
    /// separately. It only stamps frequencies onto slots.
    pub fn shape(self, root_hz: f32) -> Vec<f32> {
        match self {
            TuningKind::TwelveTet => (0..12)
                .map(|i| root_hz * 2f32.powf(i as f32 / 12.0))
                .collect(),
            TuningKind::HindustaniJust => {
                // 5-limit ratios, Sa at 1/1. Komal Ni at 9/5 (the
                // Hindustani convention). Order matches the dial's slot
                // order: Sa, komal Re, Re, komal Ga, Ga, Ma, tivra Ma,
                // Pa, komal Dha, Dha, komal Ni, Ni.
                const RATIOS: [(f32, f32); 12] = [
                    (1.0, 1.0),
                    (16.0, 15.0),
                    (9.0, 8.0),
                    (6.0, 5.0),
                    (5.0, 4.0),
                    (4.0, 3.0),
                    (45.0, 32.0),
                    (3.0, 2.0),
                    (8.0, 5.0),
                    (5.0, 3.0),
                    (9.0, 5.0),
                    (15.0, 8.0),
                ];
                RATIOS.iter().map(|(n, d)| root_hz * n / d).collect()
            }
        }
    }
}

/// The singer's per-song choice: which physical key is home (Sa), and
/// the shape of the scale planted on it. Flat and `Copy` — this is the
/// payload that crosses the `AppCoach` command boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tonality {
    /// The physical key the singer calls home (Sa). **Keyboard space**
    /// — the same space as a [`Tuning`]'s `root`, distinct role. This is
    /// the *song* root, the second of the "two roots" (the first being
    /// the key the instrument was tuned from). What the singer says:
    /// "Kaali-1".
    pub tonic: InstrumentKey,

    /// The scale's shape as **intervals between successive notes** (in
    /// slot units), walking up from the tonic. `[2,2,1,2,2,2,1]` for
    /// Bilawal/Major. These are *gaps*, not notes: the tonic (Sa) is
    /// implicit at the start, `scale_intervals[0]` is the Sa→Re step,
    /// and so on.
    ///
    /// Fixed-capacity ([`MAX_SCALE_INTERVALS`]) and **0-terminated**:
    /// read intervals until the first `0`. A `0` interval is never
    /// musically valid (it would put two scale notes on one slot), so
    /// the sentinel is self-validating.
    pub scale_intervals: [u8; MAX_SCALE_INTERVALS],
}

impl Tonality {
    /// Build a `Tonality` from a tonic key and a slice of scale
    /// intervals (gaps, walking up from the tonic). Panics in debug if
    /// `steps` is longer than [`MAX_SCALE_INTERVALS`] or contains an
    /// interior `0` — both are programming errors at the only caller
    /// today (code, not a picker).
    ///
    /// Note: this does **not** check the sum-to-`N` invariant — that
    /// needs `N` from the *tuning*, which a `Tonality` alone doesn't
    /// have. See [`well_formed`](Self::well_formed), checked at the join
    /// with a [`Tuning`].
    pub fn new(tonic: InstrumentKey, steps: &[u8]) -> Tonality {
        debug_assert!(
            steps.len() <= MAX_SCALE_INTERVALS,
            "scale has {} steps, cap is {MAX_SCALE_INTERVALS}",
            steps.len()
        );
        debug_assert!(
            steps.iter().all(|&s| s != 0),
            "scale intervals must be non-zero (0 is the terminator)"
        );
        let mut scale_intervals = [0u8; MAX_SCALE_INTERVALS];
        let n = steps.len().min(MAX_SCALE_INTERVALS);
        scale_intervals[..n].copy_from_slice(&steps[..n]);
        Tonality {
            tonic,
            scale_intervals,
        }
    }

    /// The scale intervals up to (not including) the `0` terminator.
    pub fn steps(&self) -> &[u8] {
        let end = self
            .scale_intervals
            .iter()
            .position(|&s| s == 0)
            .unwrap_or(MAX_SCALE_INTERVALS);
        &self.scale_intervals[..end]
    }

    /// Whether walking the intervals from the tonic traverses exactly
    /// one octave and lands back on the tonic — i.e. the steps (to the
    /// terminator) sum to the tuning's slot count `n`. `n` comes from
    /// the *tuning*, so this is checked at the `Tuning × Tonality` join,
    /// not at construction.
    pub fn well_formed(&self, n: u8) -> bool {
        self.steps().iter().map(|&s| s as u16).sum::<u16>() == n as u16
    }
}

/// The flat, `Copy` inputs the head hands the coach to build a
/// [`Tuning`]. Crosses the FFI boundary inside
/// [`Command::ConfigureSession`](crate::app_coach::Command); the coach
/// runs [`Tuning::new`] on receipt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TuningSpec {
    /// The anchor frequency placed at slot 0. The "A=" in "A=440 Hz" is
    /// a *name* the head supplies separately (via `root`); only the Hz
    /// is carried here.
    pub root_note_hz: f32,
    /// How to space the slots.
    pub kind: TuningKind,
    /// The physical key slot 0 sits on — the key the instrument was
    /// tuned from. **Keyboard space.** Bridges keyboard↔slot.
    pub root: InstrumentKey,
}

/// A frozen tuning: `N` slot frequencies plus the physical key slot 0
/// sits on. **Coach-internal** — owns a `Vec<f32>`, never crosses the
/// FFI boundary. Built from a [`TuningSpec`] via [`Tuning::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct Tuning {
    /// **One octave** of slot frequencies in **slot space**: `slots[0]
    /// == root_note_hz`, the rest fan upward by the kind's pattern for
    /// exactly `N` slots. `slots.len() == N`. The pattern repeats every
    /// octave, so a key an octave up is `slots[..] × 2`; the array never
    /// stores more than one octave (see [`tuning_view::hz`]).
    pub slots: Vec<f32>,
    /// The physical key (**keyboard space**) that slot 0 sits on — the
    /// key the instrument was tuned from. The bridge between the two
    /// spaces (see [`tuning_view::slot_of`]).
    pub root: InstrumentKey,
}

impl Tuning {
    /// Run the kind's shape and freeze it alongside the root key.
    /// `slots[0] == root_note_hz` — no re-pegging.
    pub fn new(spec: TuningSpec) -> Tuning {
        Tuning {
            slots: spec.kind.shape(spec.root_note_hz),
            root: spec.root,
        }
    }

    /// The slot count `N`.
    pub fn n(&self) -> usize {
        self.slots.len()
    }
}

/// The **view layer** for a [`Tuning`]: the Hz↔Instrument number-line
/// map. The only place real frequencies and keyboard↔slot arithmetic
/// live. Pure functions — state in, number out; they read a `Tuning`,
/// never own it.
pub mod tuning_view {
    use super::{InstrumentKey, Tuning};

    /// Keyboard space → slot space. The one bridge between the two
    /// spaces. Folds to one octave (the slot pattern repeats every
    /// octave), so the result is always `0..N`.
    pub fn slot_of(t: &Tuning, key: InstrumentKey) -> usize {
        let n = t.slots.len() as i32;
        ((key.offset as i32 - t.root.offset as i32).rem_euclid(n)) as usize
    }

    /// Hz of any physical key, at any octave. `slots` holds one octave;
    /// the octave the key sits in is a power-of-two multiplier applied
    /// after the fold. The octave is measured *relative to the root's
    /// octave* — there is no requirement that the root sit in octave 0;
    /// this form is correct for any `root.octave()`.
    pub fn hz(t: &Tuning, key: InstrumentKey) -> f32 {
        let octave = key.octave() - t.root.octave();
        t.slots[slot_of(t, key)] * 2f32.powi(octave)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- InstrumentKey: fold / octave (the divmod split) --------------

    #[test]
    fn fold_within_octave_is_identity() {
        assert_eq!(harmonium_key(0).fold(), 0);
        assert_eq!(harmonium_key(11).fold(), 11);
    }

    #[test]
    fn fold_beyond_octave_wraps() {
        // 14 on a 12-key board = octave 1, position 2.
        let k = InstrumentKey {
            offset: 14,
            octave_size: 12,
        };
        assert_eq!(k.fold(), 2);
        assert_eq!(k.octave(), 1);
    }

    #[test]
    fn octave_zero_within_first_octave() {
        assert_eq!(harmonium_key(0).octave(), 0);
        assert_eq!(harmonium_key(11).octave(), 0);
    }

    // --- TuningKind::shape --------------------------------------------

    #[test]
    fn twelve_tet_shape_has_twelve_slots_root_at_zero() {
        let slots = TuningKind::TwelveTet.shape(261.625_56);
        assert_eq!(slots.len(), 12);
        assert!((slots[0] - 261.625_56).abs() < 1e-3);
        // Slot 9 above C is A=440.
        assert!((slots[9] - 440.0).abs() < 0.1, "got {}", slots[9]);
    }

    #[test]
    fn just_shape_pa_at_3_over_2() {
        let slots = TuningKind::HindustaniJust.shape(200.0);
        assert_eq!(slots.len(), 12);
        assert!((slots[0] - 200.0).abs() < 1e-3);
        assert!((slots[7] - 300.0).abs() < 1e-3, "Pa = 3/2 × 200 = 300");
    }

    #[test]
    fn just_shuddha_ga_flatter_than_12tet() {
        let just = TuningKind::HindustaniJust.shape(200.0)[4];
        let et = TuningKind::TwelveTet.shape(200.0)[4];
        assert!(
            just < et,
            "Just Ga (5/4) sits below 12-TET E: {just} < {et}"
        );
    }

    // --- Tonality -----------------------------------------------------

    #[test]
    fn tonality_new_terminates_and_reads_back_steps() {
        let t = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]);
        assert_eq!(t.steps(), &[2, 2, 1, 2, 2, 2, 1]);
        // Everything past the 7 steps is the 0 terminator.
        assert_eq!(t.scale_intervals[7], 0);
    }

    #[test]
    fn well_formed_true_when_steps_sum_to_n() {
        let bilawal = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]);
        assert!(bilawal.well_formed(12));
    }

    #[test]
    fn well_formed_false_when_sum_short_or_over() {
        let short = Tonality::new(harmonium_key(0), &[2, 2, 1]); // sums to 5
        assert!(!short.well_formed(12));
        let over = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1, 2]); // 14
        assert!(!over.well_formed(12));
    }

    // --- Tuning + tuning_view (worked example from MUSIC_MODEL.md) ----

    #[test]
    fn sa_on_d_of_an_a_tuned_harmonium() {
        // Harmonium tuned from A (root offset 9), 12-TET, A=440.
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 440.0,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(9),
        });
        // Singer puts Sa on D (offset 2).
        let sa = harmonium_key(2);
        // Keyboard → slot: (2 - 9).rem_euclid(12) = 5.
        assert_eq!(tuning_view::slot_of(&tuning, sa), 5);
        // Slot → Hz: 440 × 2^(5/12) ≈ 587.3 Hz (D5).
        let hz = tuning_view::hz(&tuning, sa);
        assert!(
            (hz - 587.33).abs() < 0.1,
            "Sa should be ~587.3 Hz, got {hz}"
        );
    }

    #[test]
    fn hz_an_octave_up_doubles() {
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 261.625_56,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(0),
        });
        let c4 = harmonium_key(0); // octave 0
        let c5 = InstrumentKey {
            offset: 12,
            octave_size: 12,
        }; // octave 1, same fold
        assert!((tuning_view::hz(&tuning, c5) - 2.0 * tuning_view::hz(&tuning, c4)).abs() < 1e-2);
    }

    // --- FFI surface: the crossing types are Copy ---------------------

    #[test]
    fn ffi_payload_types_are_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<InstrumentKey>();
        assert_copy::<TuningKind>();
        assert_copy::<Tonality>();
        assert_copy::<TuningSpec>();
    }
}
