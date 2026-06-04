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
//!   Sa=0, Re=1, Ga=2 — and [`ScaleNoteInterval`] is its **vector**, a
//!   signed distance counted in *notes*. `ScaleNote − ScaleNote =
//!   ScaleNoteInterval`; here Sa→Re = 1 (one note). This is the
//!   gauge-free "which degree" axis; its origin is Sa.
//! - **Key space** (semitones): [`InstrumentKey`] is a **point** — a
//!   position on the keyboard line, the thing the player and singer
//!   *speak* ("Safed-1", "Kaali-1"). [`InstrumentKeyInterval`] is its **vector** — a
//!   signed *distance* in keys. `InstrumentKey − InstrumentKey =
//!   InstrumentKeyInterval`. Here Sa→Re = 2 (two semitones).
//! - **Hz** is the terminal output ([`tuning_view::hz`]) — a plain `f32`,
//!   not a point type. Its "interval" is a multiplicative ratio.
//! - **Slot space** is an index into a [`Tuning`]'s `slots` array, where
//!   `slots[0]` is the tuning's root. What the tuning *math* works in.
//!   The bridge from an [`InstrumentKey`] to its Hz is [`tuning_view::hz`].
//!
//! The two affine spaces are deliberately *parallel but distinct*: a
//! [`ScaleNoteInterval`] (a note-count) and an [`InstrumentKeyInterval`]
//! (a key-count) are different types, so a degree-distance can never be
//! silently spent as a key-distance. They meet on exactly one type,
//! [`Tonality`]: a scale is a list of **key-widths**
//! (`[InstrumentKeyInterval; N]`, e.g. `[2,2,1,2,2,2,1]` semitones) — how
//! many *keys* each note-step spans. [`Tonality::key_of`] turns a
//! [`ScaleNote`] into an [`InstrumentKey`] by summing the first *d* widths
//! from the tonic. That sum is the scale-space → key-space conversion
//! (1 note ⇒ 2 keys for the first Bilawal step).
//!
//! The affine algebra is encoded as operator impls, and the gauge law
//! (below) is encoded as the operators that *don't* exist:
//!
//! ```text
//! InstrumentKey    − InstrumentKey    = InstrumentKeyInterval     // the only path to Hz; gauge cancels
//! InstrumentKey    + InstrumentKeyInterval = InstrumentKey        // place a degree on the keyboard
//! InstrumentKeyInterval ± InstrumentKeyInterval = InstrumentKeyInterval     // compose distances
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
//! ([`InstrumentKeyInterval`]s), because a difference is what survives a gauge shift
//! (`(a+c) − (b+c) = a − b`). The type system enforces this: an
//! [`InstrumentKey`] is just an `offset` with no arithmetic of its own;
//! the only way to read an octave or fold to a slot is to first subtract
//! to an [`InstrumentKeyInterval`] and then divide by the tuning's period `N` (in
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
/// to get an [`InstrumentKeyInterval`], or move a key by an `InstrumentKeyInterval`.
///
/// **`offset` is an `f32`, not an integer.** A key is a position on a
/// *continuous* line, not a discrete index: a sliding voice (meend /
/// glissando) sits *between* the keys, e.g. offset `1.4`. Whole-number
/// offsets are the keyboard's fixed keys; fractional offsets are the
/// continuum the voice glides through. (Scale space stays discrete — see
/// [`ScaleNote`] — because a degree-count is combinatorial, not physical.)
/// Because the field is `f32`, this type is `PartialEq` but **not** `Eq`
/// (no total order on floats); nothing in the system hashes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstrumentKey {
    pub offset: f32,
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
///
/// **`f32`, mirroring [`InstrumentKey`].** A distance between two
/// continuous positions is itself continuous: the slide produces
/// fractional intervals (1.4 keys above Sa). Whole-number intervals are
/// the only ones any *authored* scale uses (widths are `2.0`, `1.0`, …);
/// the fraction appears only when a live, sliding position is involved.
/// `PartialEq`, not `Eq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstrumentKeyInterval(pub f32);

impl Add for InstrumentKeyInterval {
    type Output = InstrumentKeyInterval;
    fn add(self, rhs: InstrumentKeyInterval) -> InstrumentKeyInterval {
        InstrumentKeyInterval(self.0 + rhs.0)
    }
}

impl Sub for InstrumentKeyInterval {
    type Output = InstrumentKeyInterval;
    fn sub(self, rhs: InstrumentKeyInterval) -> InstrumentKeyInterval {
        InstrumentKeyInterval(self.0 - rhs.0)
    }
}

impl Neg for InstrumentKeyInterval {
    type Output = InstrumentKeyInterval;
    fn neg(self) -> InstrumentKeyInterval {
        InstrumentKeyInterval(-self.0)
    }
}

/// `InstrumentKey − InstrumentKey = InstrumentKeyInterval` — the difference of two
/// positions. **The only path from points to the invariant**: every Hz
/// computation starts here, which is why the gauge cancels structurally.
impl Sub for InstrumentKey {
    type Output = InstrumentKeyInterval;
    fn sub(self, rhs: InstrumentKey) -> InstrumentKeyInterval {
        InstrumentKeyInterval(self.offset - rhs.offset)
    }
}

/// `InstrumentKey + InstrumentKeyInterval = InstrumentKey` — move from a position by a
/// distance. Used to place a scale note on the keyboard
/// (`tonic + Σ widths`). Debug-asserts the result stays on the
/// representable line (non-negative, fits `u8`).
///
/// Note the *absence* of `impl Add for InstrumentKey` (point + point):
/// adding two gauges is nonsense, so it deliberately does not compile.
impl Add<InstrumentKeyInterval> for InstrumentKey {
    type Output = InstrumentKey;
    fn add(self, rhs: InstrumentKeyInterval) -> InstrumentKey {
        let sum = self.offset + rhs.0;
        debug_assert!(
            sum.is_finite() && sum >= 0.0,
            "key + interval = {sum} is off the representable keyboard line"
        );
        InstrumentKey { offset: sum }
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
///
/// `offset` is an `f32` ([`InstrumentKey`] is continuous), but every
/// caller today names a *whole* key (`0.0`, `12.0`, `21.0`) — the fixed
/// keyboard positions. Fractional keys come from the live slide, not from
/// a named constructor.
pub fn harmonium_key(offset: f32) -> InstrumentKey {
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
/// **notes**, not keys: Sa→Re is *one* [`ScaleNoteInterval`] apart, even
/// though it is *two* [`InstrumentKeyInterval`] keys apart on the
/// keyboard. The note→key conversion is the scale's shape, applied by
/// [`Tonality::key_of`].
///
/// `offset` is a `u8` note index; like [`InstrumentKey`] it is just a
/// position and carries no period. The scale's period (notes per octave)
/// lives on the scale, as [`Tonality::note_count`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleNote {
    pub offset: u8,
}

/// A signed distance between two [`ScaleNote`]s, counted in **notes** — a
/// **vector** in scale space. The exact mirror of [`InstrumentKeyInterval`]
/// one space over: `ScaleNote − ScaleNote = ScaleNoteInterval`, and a
/// note plus a note-distance is another note.
///
/// Counted in notes, *not* keys: Sa→Pa is `ScaleNoteInterval(4)` (four
/// notes up the scale), which the scale's widths resolve to seven keys.
/// Kept distinct from [`InstrumentKeyInterval`] precisely so the two can't
/// be confused — a note-count is not a key-count, and the only bridge
/// between them is [`Tonality::key_of`].
///
/// Like the key-space vector it carries no period; folding a note-distance
/// into one octave needs the scale's `note_count`, applied at the point of
/// use, not stored on the vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleNoteInterval(pub i8);

impl Add for ScaleNoteInterval {
    type Output = ScaleNoteInterval;
    fn add(self, rhs: ScaleNoteInterval) -> ScaleNoteInterval {
        ScaleNoteInterval(self.0 + rhs.0)
    }
}

impl Sub for ScaleNoteInterval {
    type Output = ScaleNoteInterval;
    fn sub(self, rhs: ScaleNoteInterval) -> ScaleNoteInterval {
        ScaleNoteInterval(self.0 - rhs.0)
    }
}

impl Neg for ScaleNoteInterval {
    type Output = ScaleNoteInterval;
    fn neg(self) -> ScaleNoteInterval {
        ScaleNoteInterval(-self.0)
    }
}

/// `ScaleNote − ScaleNote = ScaleNoteInterval` — the difference of two
/// scale positions, in notes. The scale-space mirror of
/// `InstrumentKey − InstrumentKey`; its gauge (Sa) cancels the same way.
impl Sub for ScaleNote {
    type Output = ScaleNoteInterval;
    fn sub(self, rhs: ScaleNote) -> ScaleNoteInterval {
        ScaleNoteInterval(self.offset as i8 - rhs.offset as i8)
    }
}

/// `ScaleNote + ScaleNoteInterval = ScaleNote` — move from a scale
/// position by a note-distance. As with [`InstrumentKey`], there is no
/// `ScaleNote + ScaleNote`: adding two gauges is nonsense and deliberately
/// does not compile. Debug-asserts the result stays non-negative and fits
/// `u8`.
impl Add<ScaleNoteInterval> for ScaleNote {
    type Output = ScaleNote;
    fn add(self, rhs: ScaleNoteInterval) -> ScaleNote {
        let sum = self.offset as i16 + rhs.0 as i16;
        debug_assert!(
            (0..=u8::MAX as i16).contains(&sum),
            "note + interval = {sum} is off the representable scale line"
        );
        ScaleNote { offset: sum as u8 }
    }
}

/// The singer's per-song choice: which physical key is home (Sa), and
/// the shape of the scale planted on it. Flat and `Copy` — this is the
/// payload that crosses the `AppCoach` command boundary.
///
/// `PartialEq`, not `Eq`: it holds `f32` widths and an `f32`-offset tonic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tonality {
    /// The [`InstrumentKey`] the singer calls home (Sa) — a **point**, the
    /// scale-space origin. The same space as a [`Tuning`]'s `root`,
    /// distinct role: this is the *song* root (the second of the "two
    /// roots"). What the singer says: "Kaali-1".
    pub tonic: InstrumentKey,

    /// The scale's shape as **[`InstrumentKeyInterval`] key-widths between successive
    /// notes**, walking up from the tonic. `[2,2,1,2,2,2,1]` for
    /// Bilawal/Major — each entry is how many *keys* (semitones) that
    /// note-step spans. These are *gaps*, not notes: the tonic (Sa) is
    /// implicit at the start, `widths[0]` is the Sa→Re key-width (2), and
    /// so on. This array *is* the scale-space → key-space conversion table.
    ///
    /// Fixed-capacity ([`MAX_SCALE_NOTES`]) and **0-terminated**: read
    /// widths until the first `InstrumentKeyInterval(0.0)`. A `0` width is never
    /// musically valid (two scale notes on one key), so the sentinel is
    /// self-validating. (`-0.0 == 0.0` in Rust, so a signed zero still
    /// terminates.)
    ///
    /// **Invariant: every width is a whole number** (`2.0`, `1.0`, …).
    /// The element type is `InstrumentKeyInterval` (`f32`) only so it shares
    /// the affine vocabulary with the continuous key line; a scale's
    /// note-steps are always an integral number of keys. The fractional
    /// freedom of [`InstrumentKeyInterval`] is for the *live slide*, never
    /// for an authored scale. Consumers that need an integer index (the
    /// dial's slot mask) round, relying on this invariant.
    pub widths: [InstrumentKeyInterval; MAX_SCALE_NOTES],
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
    pub fn new(tonic: InstrumentKey, widths: &[f32]) -> Tonality {
        debug_assert!(
            widths.len() <= MAX_SCALE_NOTES,
            "scale has {} widths, cap is {MAX_SCALE_NOTES}",
            widths.len()
        );
        debug_assert!(
            widths.iter().all(|&w| w != 0.0),
            "scale widths must be non-zero (0 is the terminator)"
        );
        let mut buf = [InstrumentKeyInterval(0.0); MAX_SCALE_NOTES];
        let n = widths.len().min(MAX_SCALE_NOTES);
        for (slot, &w) in buf[..n].iter_mut().zip(&widths[..n]) {
            *slot = InstrumentKeyInterval(w);
        }
        Tonality { tonic, widths: buf }
    }

    /// The scale key-widths up to (not including) the `InstrumentKeyInterval(0.0)`
    /// terminator.
    pub fn widths(&self) -> &[InstrumentKeyInterval] {
        let end = self
            .widths
            .iter()
            .position(|&w| w == InstrumentKeyInterval(0.0))
            .unwrap_or(MAX_SCALE_NOTES);
        &self.widths[..end]
    }

    /// Notes per octave `n` — the **scale-space period**, the exact
    /// analogue of a [`Tuning`]'s slot count `N` one space over. 7 for a
    /// heptatonic scale like Bilawal. This is the modulus any scale-space
    /// fold divides by (`note.rem_euclid(n)`), just as `slots.len()` is the
    /// key-space modulus.
    ///
    /// It equals `widths().len()` because the widths walk *around* the
    /// octave: there is one width per step, and the final step closes back
    /// onto Sa, so the step count and the distinct-note count coincide.
    /// (A display that wants the octave-Sa as a separate eighth entry adds
    /// its own `+1`; that is not the period.)
    pub fn note_count(&self) -> usize {
        self.widths().len()
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
        let from_sa = widths[..d]
            .iter()
            .fold(InstrumentKeyInterval(0.0), |acc, &w| acc + w);
        self.tonic + from_sa
    }

    /// Whether walking the key-widths from the tonic traverses exactly one
    /// octave and lands back on the tonic — i.e. the widths (to the
    /// terminator) sum to the tuning's slot count `n`. `n` comes from the
    /// *tuning*, so this is checked at the `Tuning × Tonality` join, not
    /// at construction.
    /// The widths are whole numbers (the scale invariant) and small, so
    /// they sum *exactly* in `f32` (no rounding below 2^24) — an exact
    /// `== n` compare is correct here, no epsilon needed.
    pub fn well_formed(&self, n: u8) -> bool {
        self.widths().iter().map(|w| w.0).sum::<f32>() == n as f32
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
/// on. **Coach-internal** — owns its slot arrays, never crosses the FFI
/// boundary. Built from a [`TuningSpec`] via [`Tuning::new`].
///
/// **Opaque.** Its fields are private; the *only* code that sees inside is
/// [`tuning_view`] (a child module, so it has access). Everyone else holds
/// a `Tuning` and calls `tuning_view::hz` / `Tuning::n` — they never learn
/// whether the slots are stored linear, log, or both. That freedom is the
/// point: the struct can keep **two** parallel representations and the view
/// picks whichever is cheaper for the job, with no outside coupling.
///
/// It keeps both `slots_linear` (Hz) and `slots_log2` (their base-2 logs).
/// They cannot drift: both are written *once*, together, in [`Tuning::new`]
/// and never again (private fields + no setter), so storing the
/// derived-from-the-other shadow doesn't reintroduce a second source of
/// truth — there is exactly one constructor and it fills both.
#[derive(Debug, Clone, PartialEq)]
pub struct Tuning {
    /// **One octave** of slot frequencies (linear Hz) in **slot space**:
    /// `slots_linear[0] == root_note_hz`, the rest fan upward by the kind's
    /// pattern for exactly `N` slots. `len() == N`. The pattern repeats
    /// every octave, so a key an octave up is `× 2`; the array never stores
    /// more than one octave (see [`tuning_view::hz`]).
    slots_linear: Vec<f32>,
    /// `log2` of each entry of `slots_linear`, frozen alongside it. The
    /// pitch-linear view of the same slots: an octave is `+1.0` here, and a
    /// fractional key between two slots is a plain weighted average of two
    /// of these (then one `exp2`) — which is how [`tuning_view::hz`]
    /// interpolates the slide. `exp2(slots_log2[i]) == slots_linear[i]`.
    slots_log2: Vec<f32>,
    /// The [`InstrumentKey`] that slot 0 sits on — the key the instrument was
    /// tuned from. The bridge between the two spaces (see
    /// [`tuning_view::hz`]).
    root: InstrumentKey,
}

impl Tuning {
    /// Run the kind's shape and freeze it — both the linear slots and their
    /// `log2` — alongside the root key. `slots_linear[0] == root_note_hz`,
    /// no re-pegging. The two slot arrays are filled here together and
    /// nowhere else, so they stay in lock-step by construction.
    pub fn new(spec: TuningSpec) -> Tuning {
        let slots_linear = spec.kind.shape(spec.root_note_hz);
        let slots_log2 = slots_linear.iter().map(|hz| hz.log2()).collect();
        Tuning {
            slots_linear,
            slots_log2,
            root: spec.root,
        }
    }

    /// The slot count `N`. Representation-neutral (both arrays are length
    /// `N`), so it stays public — the adapter reads it for the
    /// `well_formed` check.
    pub fn n(&self) -> usize {
        self.slots_linear.len()
    }
}

/// The **view layer** for a [`Tuning`]: the Hz↔keyboard number-line map.
/// The only place real frequencies and keyboard↔slot arithmetic live.
/// Pure functions — state in, number out; they read a `Tuning`, never
/// own it.
pub mod tuning_view {
    use super::{InstrumentKey, InstrumentKeyInterval, Tuning};

    /// Signed [`InstrumentKeyInterval`] from the tuning's root to `key`. The one
    /// quantity [`hz`] derives from — computing slot and octave from the
    /// *same* delta is what keeps them consistent (split origins were the
    /// cause of the octave-wrap bug). Just the `InstrumentKey −
    /// InstrumentKey` subtraction; the period that turns this delta into a
    /// slot + octave is the tuning's `N`, applied in [`hz`] below — **this
    /// is the one place that knows the period**, which is why a bare
    /// key/interval doesn't carry one.
    fn delta(t: &Tuning, key: InstrumentKey) -> InstrumentKeyInterval {
        key - t.root
    }

    /// Where a frequency sits **within one octave**, as a fraction in
    /// `[0, 1)`: 0 is the octave start (a power-of-two multiple of the
    /// reference), 0.5 is a tritone up, approaching 1 wraps back to 0.
    ///
    /// This is the log-frequency fold — `log2(hz / ref)` mod 1 — the
    /// model-side truth about "position around the octave circle",
    /// independent of how anything draws it. A view that paints a dial
    /// turns this into an angle by multiplying by its circle's full turn;
    /// the fold itself (which octaves wrap onto each other) lives here, not
    /// in the view. Tuning-independent: it places *any* Hz, whether a
    /// slot's target frequency or a live, sliding voice between slots.
    ///
    /// `ref_hz` is the frequency that maps to position 0 (the octave
    /// anchor). Non-positive `hz` is a caller bug; returns 0.
    pub fn octave_position(hz: f32, ref_hz: f32) -> f32 {
        if hz <= 0.0 {
            return 0.0;
        }
        (hz / ref_hz).log2().rem_euclid(1.0)
    }

    /// The `log2` of a **whole** key's frequency, `delta` keys from the
    /// root. `slots_log2` holds one octave (`slots_log2[0] ==
    /// log2(root_note_hz)`); a key `delta` keys away reads
    /// `slots_log2[delta mod N] + (delta div N)` — the slot's log-frequency
    /// plus one `+1.0` per octave. Slot and octave come from the *same*
    /// `delta` and period `N`, so a key below the root lands an octave down
    /// (negative `div_euclid`), never wrapping up into the root's octave.
    /// This is the integer-key projection [`hz`] brackets the slide between.
    fn slot_log2(t: &Tuning, delta: i32) -> f32 {
        let n = t.slots_log2.len() as i32;
        t.slots_log2[delta.rem_euclid(n) as usize] + delta.div_euclid(n) as f32
    }

    /// Hz of any key, at any octave, **relative to the root** — including a
    /// *fractional* key (the slide), interpolated between the two whole
    /// keys bracketing it.
    ///
    /// The interpolation is a plain **average in pitch**: take the
    /// log-frequency of the slot at or below the key (`lo`) and of the next
    /// slot up (`hi`), lerp them by the fractional part, and exponentiate
    /// once. Equal steps in `offset` are then equal steps in *cents*, which
    /// is how the ear hears a glide — a straight average of the two
    /// frequencies would read sharp. Working from `slots_log2` makes this
    /// the literal weighted average of two stored numbers (no `log` at call
    /// time); the octave wrap (slot 11 → slot 0 an octave up) falls out of
    /// [`slot_log2`] for free.
    ///
    /// A **whole** key has `frac == 0`, so `hz` reduces to
    /// `exp2(slot_log2(d))` — exactly the old `slots[d mod N] × 2^(d div N)`,
    /// unchanged.
    pub fn hz(t: &Tuning, key: InstrumentKey) -> f32 {
        let d = delta(t, key).0;
        let floor = d.floor();
        let frac = d - floor;
        let lo = slot_log2(t, floor as i32);
        if frac == 0.0 {
            return lo.exp2();
        }
        let hi = slot_log2(t, floor as i32 + 1);
        (lo + (hi - lo) * frac).exp2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Affine algebra: point/vector operators -----------------------

    #[test]
    fn key_minus_key_is_interval() {
        let d = harmonium_key(14.0) - harmonium_key(21.0);
        assert_eq!(d, InstrumentKeyInterval(-7.0));
    }

    #[test]
    fn key_plus_interval_moves_the_point() {
        let p = harmonium_key(12.0) + InstrumentKeyInterval(9.0);
        assert_eq!(p, harmonium_key(21.0));
    }

    #[test]
    fn intervals_compose() {
        let a = InstrumentKeyInterval(4.0);
        let b = InstrumentKeyInterval(3.0);
        assert_eq!((a + b).0, 7.0);
        assert_eq!((a - b).0, 1.0);
        assert_eq!((-a).0, -4.0);
    }

    #[test]
    fn fractional_key_is_a_distance_in_keys() {
        // The slide: a key 1.4 above another is InstrumentKeyInterval(1.4).
        // Whole-number keys still subtract to whole intervals.
        let d = harmonium_key(13.4) - harmonium_key(12.0);
        assert!((d.0 - 1.4).abs() < 1e-6);
        let back = harmonium_key(12.0) + d;
        assert!((back.offset - 13.4).abs() < 1e-6);
    }

    #[test]
    fn scale_note_re_is_one_note_two_keys() {
        // The whole point of two spaces: Sa→Re is *one* ScaleNote apart
        // but *two* KeyIntervals apart, and the Tonality bridges them.
        let bilawal = Tonality::new(harmonium_key(0.0), &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0]);
        let re = bilawal.key_of(ScaleNote { offset: 1 });
        assert_eq!(re - bilawal.tonic, InstrumentKeyInterval(2.0));
    }

    #[test]
    fn scale_note_minus_scale_note_counts_in_notes() {
        // Sa→Pa is four *notes* apart in scale space (Sa Re Ga Ma Pa),
        // even though it is seven keys apart on the keyboard.
        let sa = ScaleNote { offset: 0 };
        let pa = ScaleNote { offset: 4 };
        assert_eq!(pa - sa, ScaleNoteInterval(4));
        assert_eq!(sa - pa, ScaleNoteInterval(-4));
    }

    #[test]
    fn scale_note_plus_interval_moves_the_point() {
        let p = ScaleNote { offset: 0 } + ScaleNoteInterval(4);
        assert_eq!(p, ScaleNote { offset: 4 });
    }

    #[test]
    fn scale_note_intervals_compose() {
        let a = ScaleNoteInterval(3);
        let b = ScaleNoteInterval(2);
        assert_eq!((a + b).0, 5);
        assert_eq!((a - b).0, 1);
        assert_eq!((-a).0, -3);
    }

    #[test]
    fn note_count_is_the_scale_space_period() {
        // Heptatonic: 7 notes per octave (= 7 widths, since the last
        // width closes back onto Sa).
        let bilawal = Tonality::new(harmonium_key(0.0), &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0]);
        assert_eq!(bilawal.note_count(), 7);
        // A pentatonic scale folds at 5.
        let pentatonic = Tonality::new(harmonium_key(0.0), &[2.0, 2.0, 3.0, 2.0, 3.0]);
        assert_eq!(pentatonic.note_count(), 5);
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
        let t = Tonality::new(harmonium_key(0.0), &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0]);
        assert_eq!(
            t.widths(),
            &[
                InstrumentKeyInterval(2.0),
                InstrumentKeyInterval(2.0),
                InstrumentKeyInterval(1.0),
                InstrumentKeyInterval(2.0),
                InstrumentKeyInterval(2.0),
                InstrumentKeyInterval(2.0),
                InstrumentKeyInterval(1.0)
            ]
        );
        // Everything past the 7 widths is the 0 terminator.
        assert_eq!(t.widths[7], InstrumentKeyInterval(0.0));
    }

    #[test]
    fn key_of_walks_the_scale_from_the_tonic() {
        // Bilawal on C octave 1 (offset 12): scale notes land on
        // 12 14 16 17 19 21 23, then the octave Sa at 24.
        let t = Tonality::new(harmonium_key(12.0), &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0]);
        let line: Vec<f32> = (0..=7)
            .map(|d| t.key_of(ScaleNote { offset: d }).offset)
            .collect();
        assert_eq!(line, vec![12.0, 14.0, 16.0, 17.0, 19.0, 21.0, 23.0, 24.0]);
    }

    #[test]
    fn well_formed_true_when_widths_sum_to_n() {
        let bilawal = Tonality::new(harmonium_key(0.0), &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0]);
        assert!(bilawal.well_formed(12));
    }

    #[test]
    fn well_formed_false_when_sum_short_or_over() {
        let short = Tonality::new(harmonium_key(0.0), &[2.0, 2.0, 1.0]); // sums to 5
        assert!(!short.well_formed(12));
        let over = Tonality::new(
            harmonium_key(0.0),
            &[2.0, 2.0, 1.0, 2.0, 2.0, 2.0, 1.0, 2.0],
        ); // 14
        assert!(!over.well_formed(12));
    }

    // --- Tuning + tuning_view (worked example from MUSIC_MODEL.md) ----

    #[test]
    fn sa_on_d_of_an_a_tuned_harmonium() {
        // Harmonium tuned from A in octave 1 (offset 21), 12-TET, A=440.
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 440.0,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(21.0),
        });
        // Singer puts Sa on D in octave 1 (offset 14).
        let sa = harmonium_key(14.0);
        // Key → Hz: D sits *below* A in the same octave, so the interval
        // is negative (delta = -7 → octave -1): 440 × 2^(5/12) × 2^-1 ≈
        // 293.7 Hz. The slot and the octave come from the same delta, so a
        // key below the root lands an octave down rather than wrapping up
        // above A.
        let hz = tuning_view::hz(&tuning, sa);
        assert!(
            (hz - 293.66).abs() < 0.1,
            "Sa should be ~293.7 Hz, got {hz}"
        );
    }

    #[test]
    fn octave_position_folds_to_unit_interval() {
        let r = 261.625_56; // C
                            // The reference itself sits at 0.
        assert!(tuning_view::octave_position(r, r).abs() < 1e-5);
        // An octave up folds back to 0.
        assert!(tuning_view::octave_position(r * 2.0, r).abs() < 1e-3);
        // A tritone (6 semitones) sits at the half-way point.
        let tritone = r * 2f32.powf(6.0 / 12.0);
        assert!((tuning_view::octave_position(tritone, r) - 0.5).abs() < 1e-3);
        // Non-positive Hz is a caller bug → 0.
        assert_eq!(tuning_view::octave_position(0.0, r), 0.0);
    }

    #[test]
    fn hz_an_octave_up_doubles() {
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 261.625_56,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(0.0),
        });
        let c4 = harmonium_key(0.0); // octave 0
        let c5 = harmonium_key(12.0); // octave 1, same fold
        assert!((tuning_view::hz(&tuning, c5) - 2.0 * tuning_view::hz(&tuning, c4)).abs() < 1e-2);
    }

    #[test]
    fn fractional_key_interpolates_in_cents_not_hz() {
        // The slide. 12-TET, C at slot 0. A key halfway between slot 0 and
        // slot 1 must sit at the *cents* midpoint (a quarter-tone, +50
        // cents = ×2^(1/24)), NOT the arithmetic mean of the two
        // frequencies — which would read sharp.
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 261.625_56,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(0.0),
        });
        let lo = tuning_view::hz(&tuning, harmonium_key(0.0));
        let hi = tuning_view::hz(&tuning, harmonium_key(1.0));
        let mid = tuning_view::hz(&tuning, harmonium_key(0.5));

        let geometric = (lo * hi).sqrt(); // cents midpoint
        let arithmetic = (lo + hi) / 2.0; // sharp
        assert!(
            (mid - geometric).abs() < 1e-2,
            "got {mid}, want {geometric}"
        );
        assert!(
            (mid - arithmetic).abs() > 0.1,
            "must NOT be the arithmetic mean {arithmetic}"
        );
    }

    #[test]
    fn fractional_key_interpolates_across_the_octave_wrap() {
        // A key at 11.5 sits between slot 11 (B) and slot 0 an octave up
        // (C5). The wrap is handled by slot_log2's +octave, so the result
        // is the cents midpoint of B4 and C5 — strictly between them and
        // above B4.
        let tuning = Tuning::new(TuningSpec {
            root_note_hz: 261.625_56,
            kind: TuningKind::TwelveTet,
            root: harmonium_key(0.0),
        });
        let b4 = tuning_view::hz(&tuning, harmonium_key(11.0));
        let c5 = tuning_view::hz(&tuning, harmonium_key(12.0));
        let mid = tuning_view::hz(&tuning, harmonium_key(11.5));
        assert!(b4 < mid && mid < c5, "{b4} < {mid} < {c5}");
        assert!((mid - (b4 * c5).sqrt()).abs() < 1e-2);
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
            root: harmonium_key(9.0),
        });
        let line: Vec<i32> = [0.0f32, 2.0, 4.0, 5.0, 7.0, 9.0, 11.0]
            .iter()
            .map(|&d| tuning_view::hz(&tuning, harmonium_key(d)).round() as i32)
            .collect();
        // Strictly non-decreasing, and a C-major octave from C4.
        assert!(line.windows(2).all(|w| w[1] >= w[0]), "{line:?}");
        assert_eq!(line, vec![262, 294, 330, 349, 392, 440, 494]);
    }

    #[test]
    fn tuning_log2_slots_are_the_exp_inverse_of_linear() {
        // The two stored representations are filled together in `new` and
        // must stay in lock-step: exp2(slots_log2[i]) == slots_linear[i].
        // (Private fields — visible only because this test module is inside
        // the `music` module; the seal holds for everyone outside.)
        for kind in [TuningKind::TwelveTet, TuningKind::HindustaniJust] {
            let t = Tuning::new(TuningSpec {
                root_note_hz: 261.625_56,
                kind,
                root: harmonium_key(0.0),
            });
            assert_eq!(t.slots_linear.len(), t.slots_log2.len());
            for (lin, lg) in t.slots_linear.iter().zip(&t.slots_log2) {
                assert!((lg.exp2() - lin).abs() < 1e-2, "{kind:?}: {lg} vs {lin}");
            }
        }
    }

    // --- FFI surface: the crossing types are Copy ---------------------

    #[test]
    fn ffi_payload_types_are_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<InstrumentKey>();
        assert_copy::<InstrumentKeyInterval>();
        assert_copy::<ScaleNote>();
        assert_copy::<ScaleNoteInterval>();
        assert_copy::<TuningKind>();
        assert_copy::<Tonality>();
        assert_copy::<TuningSpec>();
    }
}
