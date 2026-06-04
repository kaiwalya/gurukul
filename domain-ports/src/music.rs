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
//! # The affine model: point vs vector
//!
//! The single source of bugs here is that several different "which note"
//! indices look identical (all small integers). The type system keeps
//! them apart by encoding the **affine structure** of pitch:
//!
//! There are **two affine point/vector spaces** plus a terminal Hz output:
//!
//! - **Scale space** (Roman numerals): [`ScaleNote`] is a **point** —
//!   Sa=0, Re=1, Ga=2. Distances here count *notes* (Sa→Re = 1). This is
//!   the gauge-free "which degree" axis; its origin is Sa. (We don't model
//!   a scale-space *vector* yet — nothing subtracts two `ScaleNote`s
//!   today; add one when a caller needs it.)
//! - **Key space** (semitones): [`InstrumentKey`] is a **point** — a
//!   position on the keyboard line, the thing the player and singer
//!   *speak* ("Safed-1", "Kaali-1"). [`KeyInterval`] is its **vector** — a
//!   signed *distance* in keys. `InstrumentKey − InstrumentKey =
//!   KeyInterval`. Here Sa→Re = 2 (two semitones).
//! - **Hz** is the terminal output ([`tuning_view::hz`]) — a plain `f32`,
//!   not a point type. Its "interval" is a multiplicative ratio.
//!
//! The two spaces meet on [`Tonality`]: a scale is a list of **key-widths**
//! (`[KeyInterval; N]`, e.g. `[2,2,1,2,2,2,1]` semitones) — how many *keys*
//! each note-step spans. [`Tonality::key_of`] turns a [`ScaleNote`] into an
//! [`InstrumentKey`] by summing the first *d* widths from the tonic. That
//! sum is the scale-space → key-space conversion (1 note ⇒ 2 keys for the
//! first Bilawal step).
//!
//! - **Slot space** is an index into a [`Tuning`]'s `slots` array, where
//!   `slots[0]` is the tuning's root. What the tuning *math* works in.
//!   The bridge from an [`InstrumentKey`] to a slot is [`tuning_view::slot_of`].
//!
//! The affine algebra is encoded as operator impls, and the gauge law
//! (below) is encoded as the operators that *don't* exist:
//!
//! ```text
//! InstrumentKey    − InstrumentKey    = KeyInterval     // the only path to Hz; gauge cancels
//! InstrumentKey    + KeyInterval = InstrumentKey        // place a degree on the keyboard
//! KeyInterval ± KeyInterval = KeyInterval     // compose distances
//! InstrumentKey    + InstrumentKey    → DOES NOT COMPILE   // adding two gauges is nonsense
//! ```
//!
//! # The gauge law (why the math has the shape it does)
//!
//! Think of these as **coordinate frames on one underlying
//! log-frequency line** (the physics analogy is exact — frames + gauge):
//!
//! - **Key space** ([`InstrumentKey::offset`]) is *affine*: positions and their
//!   differences are meaningful, but the **origin is a gauge** — a pure
//!   labelling choice. "offset 0 = C" is a convention, not a fact. Shift
//!   *every* offset (keys, roots, tonics) by the same constant and **no
//!   observable changes** — we verified this: moving the A=440 root from
//!   offset 9 to 21 (and the tonic 0→12) left the resolved Hz table
//!   byte-for-byte identical.
//! - **Scale space** ([`ScaleNote`]) is *also* affine — its gauge is Sa
//!   itself. `ScaleNote(2)` (Ga) means a position *2 notes above the
//!   tonic*; note 0 is wherever Sa sits.
//! - **Hz space** is the only frame with a true physical anchor
//!   (`root_note_hz`), and it is **not** merely affine: 0 Hz (silence) is
//!   a real zero you don't get to pick, and *ratios* (×2 = octave) carry
//!   meaning. Frequencies and frequency ratios are the invariants. The
//!   affine→Hz map is exponential (`2^(delta/N)`); everything additive
//!   above is the log-domain of this multiplicative truth.
//!
//! **The law: no logic may depend on a gauge.** Concretely: nothing may
//! depend on an *absolute* [`InstrumentKey`] — only on **differences**
//! ([`KeyInterval`]s), because a difference is what survives a gauge shift
//! (`(a+c) − (b+c) = a − b`). The type system enforces this: an
//! [`InstrumentKey`] is just an `offset` with no arithmetic of its own;
//! the only way to read an octave or fold to a slot is to first subtract
//! to a [`KeyInterval`] and then divide by the tuning's period `N` (in
//! [`tuning_view`]). So the octave-wrap bug (deriving the octave from two
//! separately gauge-dependent quantities that don't cancel) becomes
//! **unrepresentable** — you cannot reach the octave without first
//! subtracting to the invariant `delta`.
//!
//! **Why this is structural, not a style note.** The transform
//! `key → Hz` is *forced* to factor through `delta = key − root`:
//! subtract to the invariant on the first line, then compute only with
//! invariants. See [`tuning_view::hz`]: slot and octave are *both*
//! projections of the same `delta`, which is what keeps them consistent.
//!
//! **The one licensed gauge pick.** A bare integer becomes an
//! [`InstrumentKey`]
//! at exactly one door: the named constructor [`harmonium_key`]
//! (C-origin). Naming that constructor *is* choosing the gauge — a
//! `piano_origin` would be a different valid chart on the same line. The
//! discipline: pick one chart for the whole system and never mix, since
//! the cancellation only works when both operands share a gauge.
//!
//! # FFI surface
//!
//! [`InstrumentKey`], [`TuningKind`], [`Tonality`], and [`TuningSpec`] are flat
//! and `Copy` — they cross the `AppCoach` command boundary. [`Tuning`]
//! is coach-internal (it owns a `Vec<f32>`) and never crosses; it is
//! *built* coach-side from a [`TuningSpec`].

use std::ops::{Add, Neg, Sub};

/// The cap on a [`Tonality`]'s `widths` array (one key-width per
/// note-step). A fixed cap keeps `Tonality` flat and `Copy`; 32 covers any
/// 12- or 22-slot scale with room to spare.
pub const MAX_SCALE_NOTES: usize = 32;

/// A position on the keyboard line — a **point** in the affine model.
///
/// Just a position: a single `offset` on the whole multi-octave line.
/// Name-free and `Copy` (so it crosses the FFI boundary): the *name*
/// ("Safed-1" vs "C") is derived head-side by the note system, never
/// stored here.
///
/// **No period.** A point doesn't carry the keyboard's keys-per-octave —
/// that's a fact about the *tuning*, not the point, and it's only needed
/// when you fold a position into the repeating slot pattern. So folding
/// lives in [`tuning_view`] (against the tuning's slot count `N`), and a
/// bare `InstrumentKey` is meaningless until paired with a [`Tuning`] —
/// which is honest, since you can't get its Hz or slot without one.
///
/// Being a **point**, an `InstrumentKey` exposes *no* arithmetic of its
/// own. The only operations are the affine ones below: subtract two keys
/// to get a [`KeyInterval`], or move a key by a `KeyInterval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentKey {
    pub offset: u8,
}

/// A signed distance between two [`InstrumentKey`]s, in keys — a
/// **vector** in keyboard space. The gauge-*invariant* quantity: it
/// survives a shift of the origin (`(a+c) − (b+c) = a − b`), so it is the
/// only kind of value the tuning math is allowed to branch on.
///
/// Just a number of keys — like the point, it carries no period. Folding
/// a delta into an octave needs a period (`N`), and that comes from the
/// [`Tuning`] at the point of use ([`tuning_view`]), not from the
/// interval itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInterval(pub i32);

impl Add for KeyInterval {
    type Output = KeyInterval;
    fn add(self, rhs: KeyInterval) -> KeyInterval {
        KeyInterval(self.0 + rhs.0)
    }
}

impl Sub for KeyInterval {
    type Output = KeyInterval;
    fn sub(self, rhs: KeyInterval) -> KeyInterval {
        KeyInterval(self.0 - rhs.0)
    }
}

impl Neg for KeyInterval {
    type Output = KeyInterval;
    fn neg(self) -> KeyInterval {
        KeyInterval(-self.0)
    }
}

/// `InstrumentKey − InstrumentKey = KeyInterval` — the difference of two
/// positions. **The only path from points to the invariant**: every Hz
/// computation starts here, which is why the gauge cancels structurally.
impl Sub for InstrumentKey {
    type Output = KeyInterval;
    fn sub(self, rhs: InstrumentKey) -> KeyInterval {
        KeyInterval(self.offset as i32 - rhs.offset as i32)
    }
}

/// `InstrumentKey + KeyInterval = InstrumentKey` — move from a position by a
/// distance. Used to place a scale note on the keyboard
/// (`tonic + Σ widths`). Debug-asserts the result stays on the
/// representable line (non-negative, fits `u8`).
///
/// Note the *absence* of `impl Add for InstrumentKey` (point + point):
/// adding two gauges is nonsense, so it deliberately does not compile.
impl Add<KeyInterval> for InstrumentKey {
    type Output = InstrumentKey;
    fn add(self, rhs: KeyInterval) -> InstrumentKey {
        let sum = self.offset as i32 + rhs.0;
        debug_assert!(
            (0..=u8::MAX as i32).contains(&sum),
            "key + interval = {sum} is off the representable keyboard line"
        );
        InstrumentKey { offset: sum as u8 }
    }
}

/// A key on a 12-key keyboard (harmonium / piano) at the given offset —
/// **the one licensed gauge pick**: choosing this constructor (vs a
/// hypothetical `piano_origin`) is choosing the C-origin chart.
///
/// The line is **C-origin**: offset 0 is C, counting up in semitones
/// (C=0, C♯=1, … A=9, B=11), with the octave in the high bits (`offset /
/// 12`). So `harmonium_key(0)` is Safed-1 / C in octave 0;
/// `harmonium_key(9)` is the A in octave 0; the A a tuner anchors "A=440"
/// on is conventionally placed in octave 1, `harmonium_key(21)` (21 = 9 +
/// 12), so song tonics an octave below it land in the singing register
/// rather than the lowest octave.
///
/// (The "12" is the harmonium's keys-per-octave, but it lives in the
/// *name* of this constructor and in the tuning's slot count — not as a
/// field on the key. A key is just an offset; see [`InstrumentKey`].)
pub fn harmonium_key(offset: u8) -> InstrumentKey {
    InstrumentKey { offset }
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

/// A position in **scale space** — a **point**, counted in *notes* from
/// the tonic. Sa is `ScaleNote(0)`, Re is `ScaleNote(1)`, Ga is
/// `ScaleNote(2)`, and so on (Western Roman numerals: I, II, III…).
///
/// This is the gauge-free "which degree" axis. Distances here count
/// **notes**, not keys: Sa→Re is *one* `ScaleNote` apart, even though it
/// is *two* [`KeyInterval`] keys apart on the keyboard. The note→key
/// conversion is the scale's shape, applied by [`Tonality::key_of`].
///
/// `offset` is a `u8` note index; like [`InstrumentKey`] it is just a
/// position and carries no period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleNote {
    pub offset: u8,
}

/// The singer's per-song choice: which physical key is home (Sa), and
/// the shape of the scale planted on it. Flat and `Copy` — this is the
/// payload that crosses the `AppCoach` command boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tonality {
    /// The [`InstrumentKey`] the singer calls home (Sa) — a **point**, the
    /// scale-space origin. The same space as a [`Tuning`]'s `root`,
    /// distinct role: this is the *song* root (the second of the "two
    /// roots"). What the singer says: "Kaali-1".
    pub tonic: InstrumentKey,

    /// The scale's shape as **[`KeyInterval`] key-widths between successive
    /// notes**, walking up from the tonic. `[2,2,1,2,2,2,1]` for
    /// Bilawal/Major — each entry is how many *keys* (semitones) that
    /// note-step spans. These are *gaps*, not notes: the tonic (Sa) is
    /// implicit at the start, `widths[0]` is the Sa→Re key-width (2), and
    /// so on. This array *is* the scale-space → key-space conversion table.
    ///
    /// Fixed-capacity ([`MAX_SCALE_NOTES`]) and **0-terminated**: read
    /// widths until the first `KeyInterval(0)`. A `0` width is never
    /// musically valid (two scale notes on one key), so the sentinel is
    /// self-validating.
    pub widths: [KeyInterval; MAX_SCALE_NOTES],
}

impl Tonality {
    /// Build a `Tonality` from a tonic pitch and a slice of scale
    /// key-widths (gaps in keys, walking up from the tonic). Panics in
    /// debug if `widths` is longer than [`MAX_SCALE_NOTES`] or contains an
    /// interior `0` — both are programming errors at the only caller today
    /// (code, not a picker).
    ///
    /// Note: this does **not** check the sum-to-`N` invariant — that
    /// needs `N` from the *tuning*, which a `Tonality` alone doesn't
    /// have. See [`well_formed`](Self::well_formed), checked at the join
    /// with a [`Tuning`].
    pub fn new(tonic: InstrumentKey, widths: &[i32]) -> Tonality {
        debug_assert!(
            widths.len() <= MAX_SCALE_NOTES,
            "scale has {} widths, cap is {MAX_SCALE_NOTES}",
            widths.len()
        );
        debug_assert!(
            widths.iter().all(|&w| w != 0),
            "scale widths must be non-zero (0 is the terminator)"
        );
        let mut buf = [KeyInterval(0); MAX_SCALE_NOTES];
        let n = widths.len().min(MAX_SCALE_NOTES);
        for (slot, &w) in buf[..n].iter_mut().zip(&widths[..n]) {
            *slot = KeyInterval(w);
        }
        Tonality { tonic, widths: buf }
    }

    /// The scale key-widths up to (not including) the `KeyInterval(0)`
    /// terminator.
    pub fn widths(&self) -> &[KeyInterval] {
        let end = self
            .widths
            .iter()
            .position(|&w| w == KeyInterval(0))
            .unwrap_or(MAX_SCALE_NOTES);
        &self.widths[..end]
    }

    /// The [`InstrumentKey`] of [`ScaleNote`] `note` (Sa = `ScaleNote(0)`).
    /// Sums the first `note.offset` key-widths up from the tonic:
    /// `tonic + Σ widths[0..note]`. This is the scale-space → key-space
    /// map — it both converts notes to keys (1 note ⇒ its key-width) and
    /// performs the `+tonic` injection that places the result on the
    /// keyboard. `ScaleNote(0)` has an empty sum, recovering the tonic.
    ///
    /// Debug-panics if `note` exceeds the number of widths to the
    /// terminator.
    pub fn key_of(&self, note: ScaleNote) -> InstrumentKey {
        let widths = self.widths();
        let d = note.offset as usize;
        debug_assert!(
            d <= widths.len(),
            "scale note {d} out of range (scale has {} widths)",
            widths.len()
        );
        let from_sa = widths[..d].iter().fold(KeyInterval(0), |acc, &w| acc + w);
        self.tonic + from_sa
    }

    /// Whether walking the key-widths from the tonic traverses exactly one
    /// octave and lands back on the tonic — i.e. the widths (to the
    /// terminator) sum to the tuning's slot count `n`. `n` comes from the
    /// *tuning*, so this is checked at the `Tuning × Tonality` join, not
    /// at construction.
    pub fn well_formed(&self, n: u8) -> bool {
        self.widths().iter().map(|w| w.0).sum::<i32>() == n as i32
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
    /// The [`InstrumentKey`] slot 0 sits on — the key the instrument was tuned
    /// from. Bridges keyboard↔slot.
    pub root: InstrumentKey,
}

/// A frozen tuning: `N` slot frequencies plus the [`InstrumentKey`] slot 0 sits
/// on. **Coach-internal** — owns a `Vec<f32>`, never crosses the FFI
/// boundary. Built from a [`TuningSpec`] via [`Tuning::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct Tuning {
    /// **One octave** of slot frequencies in **slot space**: `slots[0]
    /// == root_note_hz`, the rest fan upward by the kind's pattern for
    /// exactly `N` slots. `slots.len() == N`. The pattern repeats every
    /// octave, so a key an octave up is `slots[..] × 2`; the array never
    /// stores more than one octave (see [`tuning_view::hz`]).
    pub slots: Vec<f32>,
    /// The [`InstrumentKey`] that slot 0 sits on — the key the instrument was
    /// tuned from. The bridge between the two spaces (see
    /// [`tuning_view::slot_of`]).
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

/// The **view layer** for a [`Tuning`]: the Hz↔keyboard number-line map.
/// The only place real frequencies and keyboard↔slot arithmetic live.
/// Pure functions — state in, number out; they read a `Tuning`, never
/// own it.
pub mod tuning_view {
    use super::{InstrumentKey, KeyInterval, Tuning};

    /// Signed [`KeyInterval`] from the tuning's root to `key`. The one
    /// quantity both [`slot_of`] and [`hz`] derive from — computing slot
    /// and octave from the *same* delta is what keeps them consistent
    /// (split origins were the cause of the octave-wrap bug). Just the
    /// `InstrumentKey − InstrumentKey` subtraction; the period that turns
    /// this delta into a slot + octave is the tuning's `N`, applied in
    /// [`slot_of`] / [`hz`] below — **this is the one place that knows the
    /// period**, which is why a bare key/interval doesn't carry one.
    fn delta(t: &Tuning, key: InstrumentKey) -> KeyInterval {
        key - t.root
    }

    /// Keyboard space → slot space. Folds the root-relative interval to
    /// one octave (the slot pattern repeats every octave), so the result
    /// is always `0..N`. The period is the tuning's slot count `N`
    /// (`slots.len()`) — the sole authority on keys-per-octave. A key from
    /// a mismatched keyboard (e.g. a 22-shruti key against a 12-slot
    /// tuning) simply folds wrong here, at the one seam that has the `N`
    /// to judge it.
    pub fn slot_of(t: &Tuning, key: InstrumentKey) -> usize {
        let n = t.slots.len() as i32;
        delta(t, key).0.rem_euclid(n) as usize
    }

    /// Hz of any key, at any octave, **relative to the root**. `slots`
    /// holds one octave built up from the root note (`slots[0] ==
    /// root_note_hz`); a key `delta` keys from the root reads
    /// `slots[delta mod N] × 2^(delta div N)`. The slot and the octave
    /// multiplier come from the *same* `delta` and the *same* period `N`,
    /// so a key below the root lands an octave down (negative `div_euclid`)
    /// rather than wrapping up into the root's octave — walking a scale
    /// from a tonic below the root yields an ascending line, no post-hoc
    /// octave-lifting needed.
    pub fn hz(t: &Tuning, key: InstrumentKey) -> f32 {
        let n = t.slots.len() as i32;
        let d = delta(t, key).0;
        t.slots[d.rem_euclid(n) as usize] * 2f32.powi(d.div_euclid(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Affine algebra: point/vector operators -----------------------

    #[test]
    fn key_minus_key_is_interval() {
        let d = harmonium_key(14) - harmonium_key(21);
        assert_eq!(d, KeyInterval(-7));
    }

    #[test]
    fn key_plus_interval_moves_the_point() {
        let p = harmonium_key(12) + KeyInterval(9);
        assert_eq!(p, harmonium_key(21));
    }

    #[test]
    fn intervals_compose() {
        let a = KeyInterval(4);
        let b = KeyInterval(3);
        assert_eq!((a + b).0, 7);
        assert_eq!((a - b).0, 1);
        assert_eq!((-a).0, -4);
    }

    #[test]
    fn scale_note_re_is_one_note_two_keys() {
        // The whole point of two spaces: Sa→Re is *one* ScaleNote apart
        // but *two* KeyIntervals apart, and the Tonality bridges them.
        let bilawal = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]);
        let re = bilawal.key_of(ScaleNote { offset: 1 });
        assert_eq!(re - bilawal.tonic, KeyInterval(2));
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
    fn tonality_new_terminates_and_reads_back_widths() {
        let t = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]);
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
        // Everything past the 7 widths is the 0 terminator.
        assert_eq!(t.widths[7], KeyInterval(0));
    }

    #[test]
    fn key_of_walks_the_scale_from_the_tonic() {
        // Bilawal on C octave 1 (offset 12): scale notes land on
        // 12 14 16 17 19 21 23, then the octave Sa at 24.
        let t = Tonality::new(harmonium_key(12), &[2, 2, 1, 2, 2, 2, 1]);
        let line: Vec<u8> = (0..=7)
            .map(|d| t.key_of(ScaleNote { offset: d }).offset)
            .collect();
        assert_eq!(line, vec![12, 14, 16, 17, 19, 21, 23, 24]);
    }

    #[test]
    fn well_formed_true_when_widths_sum_to_n() {
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
        // Harmonium tuned from A in octave 1 (offset 21), 12-TET, A=440.
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 440.0,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(21),
        });
        // Singer puts Sa on D in octave 1 (offset 14).
        let sa = harmonium_key(14);
        // Keyboard → slot: (14 - 21).rem_euclid(12) = 5.
        assert_eq!(tuning_view::slot_of(&tuning, sa), 5);
        // Slot → Hz: D sits *below* A in the same octave, so the
        // interval is negative (delta = -7 → octave -1): 440 × 2^(5/12)
        // × 2^-1 ≈ 293.7 Hz. The octave comes from the same delta as the
        // slot, so a key below the root lands an octave down rather than
        // wrapping up above A.
        let hz = tuning_view::hz(&tuning, sa);
        assert!(
            (hz - 293.66).abs() < 0.1,
            "Sa should be ~293.7 Hz, got {hz}"
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
        let c5 = harmonium_key(12); // octave 1, same fold
        assert!((tuning_view::hz(&tuning, c5) - 2.0 * tuning_view::hz(&tuning, c4)).abs() < 1e-2);
    }

    #[test]
    fn scale_below_root_ascends_naturally() {
        // 12-TET rooted at A=440. Walk Bilawal degrees from Sa on C
        // (key 0): 0 2 4 5 7 9 11 — all below A on the keyboard. Because
        // the octave comes from the same (negative) delta as the slot,
        // each key reads in octave -1 until it reaches A, so the line
        // climbs C4→B4 with no octave-lifting hack.
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 440.0,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(9),
        });
        let line: Vec<i32> = [0u8, 2, 4, 5, 7, 9, 11]
            .iter()
            .map(|&d| tuning_view::hz(&tuning, harmonium_key(d)).round() as i32)
            .collect();
        // Strictly non-decreasing, and a C-major octave from C4.
        assert!(line.windows(2).all(|w| w[1] >= w[0]), "{line:?}");
        assert_eq!(line, vec![262, 294, 330, 349, 392, 440, 494]);
    }

    // --- FFI surface: the crossing types are Copy ---------------------

    #[test]
    fn ffi_payload_types_are_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<InstrumentKey>();
        assert_copy::<KeyInterval>();
        assert_copy::<ScaleNote>();
        assert_copy::<TuningKind>();
        assert_copy::<Tonality>();
        assert_copy::<TuningSpec>();
    }
}
