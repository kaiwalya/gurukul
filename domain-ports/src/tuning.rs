//! Pitch as a helix; tuning as a cylinder of lines it wraps; 440 Hz a tiny
//! rotation of that cylinder.
//!
//! [`pitch.rs`](super::pitch) is a straight line: `log2(Hz)`, no wrap. That is
//! the right picture for one pitch, but a *tuning* is circular — a pattern
//! that repeats every octave, forever. To see it, wind the log2 line around a
//! cylinder so **one turn = one octave**, and the line becomes a helix.
//! Climbing in pitch is then a rotation around the barrel: keep raising the
//! pitch and you sweep around, and the moment you have doubled the frequency
//! you have come back to the same angle — one floor higher up the helix. So
//! every pitch splits into *which floor* (the octave, the height) and *where
//! around the barrel* (the angle within the octave).
//!
//! Now suppose a tuning fork is faulty and rings at 450 Hz where it should
//! ring at 440. Every note built from it slides up in lock-step: its octaves
//! move 900, 1800, 3600. On a *linear* Hz axis those shifts are all different
//! sizes — but on the log2 helix they are the **same rotation**, because each
//! is the same *ratio* (450/440), and equal ratios are equal angles. The whole
//! tuning just turns by one small angle, rigidly; the spacing *between* its
//! notes is untouched. That angle is a **mantissa** — the within-octave,
//! octave-free part of a pitch — which is why this file stores
//! [`PitchLog2Interval`]s reduced by [`fract`](PitchLog2Interval::fract) and
//! mints no new type: an angle on the cylinder *is* the mantissa of a pitch
//! difference. Each note of the tuning is then a **line drawn parallel to the
//! axis** — a **groove** cut down the cylinder (the two words are used
//! interchangeably) — at one fixed angle, running from −∞ to +∞: every time
//! that line crosses the helix is the same note, one octave away. A tuning is
//! thus not a list of frequencies but a list of **angles** — its lines are anonymous
//! here; what *names* them is a separate concern, and the geometry never knows
//! it.
//!
//! This is also what "A = 440 Hz" really says. Of that number, almost
//! everything is just powers of two — 440, 880, 220 are the *same note* on
//! different floors, and the helix already accounts for the floors. The one
//! piece of information that is *not* recoverable from doubling is the small
//! **octave-free rotation**: where, around the barrel, that note sits. Strip
//! the octave away (`log2(440)`, keep only the `fract`) and what remains is
//! the sole true content of "440" — a single angle, the global rotation of the
//! whole bundle. That is all a reference pitch contributes, and all this file
//! keeps of it.
//!
//! - [`TuningIntervals`] is the rigid shape: the bundle of line-angles. Two
//!   instruments on the same tuning system share it exactly, whatever their
//!   reference pitch.
//! - [`TuningAbsolute`] places that shape by adding the one global `rotation` —
//!   the octave-free residue of the reference pitch.
//! - [`TuningRotated`] re-bases that bundle by an integer cursor — the same
//!   cylinder read from a different line as root. Both implement the [`Tuning`]
//!   trait, so downstream code reads either through one contract.
//!
//! **Octaves are deliberately absent here.** A line spans every octave, so
//! "which octave" is not a question this file answers — it is reattached later,
//! by the layer above ([`scale`](super::scale)), which carries an integer
//! octave (the register) and so picks both a line and a floor. Here we keep
//! only the angles: the shape, and how far it has turned.
//! The one absolute anchor is [`ORIGIN`], the point `log2(Hz) = 0` (1 Hz),
//! from which an angle becomes a concrete point on the barrel.

use crate::pitch::{PitchLog2, PitchLog2Interval};

/// The point where the cylinder's angles meet the pitch helix: `log2(Hz) = 0`,
/// i.e. 1 Hz.
///
/// A tuning's lines are pure angles — lines on the cylinder, with no
/// absolute pitch of their own. `ORIGIN` is the one place this file commits to
/// a concrete pitch: the zero a frequency is measured against when it enters
/// the cylinder. Subtracting it turns an absolute Hz into an angle —
/// `(PitchLog2::from_hz(f) - ORIGIN).fract()` — which is how a reference
/// frequency becomes the [`TuningAbsolute`] rotation. (Unlike
/// [`pitch.rs`](super::pitch), which has no privileged point, the tuning layer
/// *has* chosen where it meets the helix, so it may name it.)
pub const ORIGIN: PitchLog2 = PitchLog2(0.0);

/// The cap on a [`TuningIntervals`]' gap array. A fixed cap keeps the whole
/// tuning chain flat and `Copy` (so it crosses the command boundary directly,
/// like the old `music.rs` `Tonality`); 32 covers the system's largest tuning
/// (22-shruti) with room for finer microtonal grids.
pub const MAX_TUNING_SLOTS: usize = 32;

/// The rigid shape of a tuning: the **gaps between successive lines** on the
/// cylinder, each a mantissa in `[0, 1)`, carried as a [`PitchLog2Interval`].
///
/// Storing gaps (not absolute angles) is why a line "knows where the previous
/// one is" — each step is measured from its predecessor, and walking the gaps
/// from the first line traces the whole bundle. Because one full turn is one
/// octave, the gaps **sum to 1**: the walk closes the octave and lands back on
/// the first line, one floor up.
///
/// This is gauge-free and reference-free: it is *only* the pattern, the spacing
/// between notes. Two instruments tuned to the same system (12-TET, Hindustani
/// Just) hold identical `TuningIntervals` no matter what their reference pitch
/// is — that difference lives entirely in the `Tuning` rotation, not here.
///
/// **Flat and `Copy`.** Storage is a fixed `[PitchLog2Interval; MAX_TUNING_SLOTS]`
/// with an explicit `len`, not a `Vec` — the slot count `N` (12 for 12-TET and
/// Just, 22 for shruti) is the `len`, and slots `[len..]` are padding. This is
/// what lets the whole tuning chain (`TuningAbsolute`, `TuningRotated`, and the
/// `Scale` above) be `Copy` and cross the command boundary without a heap
/// allocation.
///
/// **Padding is `0.0`, not NaN.** A zero gap is inert in the two operations
/// that matter: it adds nothing to the sum-to-1 check ([`well_formed`]), and it
/// compares equal under the derived `PartialEq` (NaN would poison both — a
/// forgotten `[..len]` would silently sum to NaN and every equality would go
/// false). So a stray read of the padding is finite-and-wrong, never silently
/// catastrophic. `len` is the real terminator; `0.0` is not overloaded as one
/// (unlike a scale's 0-terminated widths, a tuning gap genuinely *could* be a
/// small value, so the length is explicit).
///
/// [`well_formed`]: TuningIntervals::well_formed
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TuningIntervals {
    /// The gap from each line to the next (prev-relative), in slot order, the
    /// first `len` entries each a mantissa in `[0, 1)` and the rest `0.0`
    /// padding. The valid gaps sum to 1 (one octave). Mantissas by construction
    /// — [`new`](TuningIntervals::new) folds every input through
    /// [`fract`](PitchLog2Interval::fract).
    rotations: [PitchLog2Interval; MAX_TUNING_SLOTS],
    /// The slot count `N` — how many of `rotations` are valid gaps. Slots
    /// `[len..]` are `0.0` padding.
    len: usize,
}

impl TuningIntervals {
    /// Build the shape from raw gaps, folding each to its mantissa (`[0, 1)`)
    /// so the stored values are gaps by construction. Does **not** check the
    /// sum-to-1 well-formedness — that is [`well_formed`](TuningIntervals::well_formed),
    /// run at the join where it matters.
    ///
    /// Debug-panics if more than [`MAX_TUNING_SLOTS`] gaps are supplied (a
    /// programming error: no real tuning exceeds the cap). Excess gaps past the
    /// cap are dropped in release.
    pub fn new(rotations: impl IntoIterator<Item = PitchLog2Interval>) -> TuningIntervals {
        let mut buf = [PitchLog2Interval(0.0); MAX_TUNING_SLOTS];
        let mut len = 0;
        for gap in rotations {
            debug_assert!(
                len < MAX_TUNING_SLOTS,
                "tuning has more than {MAX_TUNING_SLOTS} slots"
            );
            if len >= MAX_TUNING_SLOTS {
                break;
            }
            buf[len] = gap.fract();
            len += 1;
        }
        TuningIntervals {
            rotations: buf,
            len,
        }
    }

    /// The valid gaps — the first `len`, excluding the `0.0` padding.
    fn gaps(&self) -> &[PitchLog2Interval] {
        &self.rotations[..self.len]
    }

    /// Whether the gaps sum to one octave (within `eps`). Gaps are irrational
    /// `f32`s (`log2` of ratios), so this needs a tolerance — unlike an exact
    /// integer check. A shape that fails this does not close the octave.
    pub fn well_formed(&self, eps: f32) -> bool {
        let sum: f32 = self.gaps().iter().map(|g| g.0).sum();
        (sum - 1.0).abs() <= eps
    }

    /// The number of lines, `N`.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no lines. (Present so `len` reads as a real length;
    /// an empty tuning is not meaningful, but the type does not forbid it.)
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The gap at line `i`, read **cyclically** (`i mod N`). A pure array read of a
/// stored mantissa — no arithmetic touches the value, so the same line read at
/// any rotation returns the bit-identical `f32`. Module-private: it is the
/// re-basing primitive a [`TuningRotated`] reads through, not public surface.
fn gap_at(shape: &TuningIntervals, i: usize) -> PitchLog2Interval {
    shape.rotations[i % shape.len]
}

/// The angle from line 0 to line `i`: the sum of the first `i` gaps, **re-summed
/// fresh** from the immutable array. Unfolded — `angle_to(N)` is one whole
/// octave. Module-private: the prefix-sum a [`TuningRotated`] derives its
/// rotation from, recomputed each call so no shift accumulates error.
fn angle_to(shape: &TuningIntervals, i: usize) -> PitchLog2Interval {
    (0..i)
        .map(|k| gap_at(shape, k))
        .fold(PitchLog2Interval(0.0), |acc, g| acc + g)
}

/// A readable cylinder: the rigid [`TuningIntervals`] shape and the one global
/// rotation that turns the whole bundle (what "A = 440" vs "A = 441" sets),
/// read **from some chosen starting line**.
///
/// **This is the whole object — a cylinder of line-angles and how far it has
/// turned.** It carries no pitch. Crossing the lines with the pitch helix (at
/// [`ORIGIN`]) to read out frequencies is a separate, optional concern,
/// reconciled later by the [`scale`](super::scale) layer, which supplies the
/// octave the cylinder deliberately omits.
///
/// A bare [`TuningAbsolute`] reads from line 0; a [`TuningRotated`] reads from
/// a re-based line. To anything downstream they are interchangeable — that is
/// the point of the trait. Both answers are derived from the immutable gaps on
/// each call, so re-basing (shifting) never compounds floating-point error.
///
/// # The line index is ordinal, not metric
///
/// Lines are addressed by **ordinal position** from this view's root: line 0 is
/// the root, line 1 is the next groove up, line `i` is the `i`-th groove —
/// counted by *how many grooves up*, never by angle or Hz. The step from line
/// `i` to line `i + 1` is always "one groove," even though the *angle* it spans
/// (the gap) may be uneven: on 22-shruti, line 3 is the fourth groove up,
/// sitting at some uneven angle, **not** at `3/22` of an octave. This is the index the
/// internal `gap_at`/`angle_to` and [`shift_up`](Tuning::shift_up) all count in.
///
/// This ordinal index is the public ruler the layers above slide along — a
/// scale's [`ScaleIntervals`] bit `i` means "tuning line `i`," the `i`-th
/// groove, by exactly this counting. That a scale can drop onto the tuning at
/// all rests on this: the index is ordinal (which groove), so the integer scale
/// pattern composes with the uneven tuning without ever depending on the
/// grooves' real spacing. The unevenness lives only in the angles the gaps
/// resolve to, never in the index.
///
/// [`ScaleIntervals`]: crate::scale::ScaleIntervals
pub trait Tuning {
    /// The rigid shape as read from this view's starting line — the
    /// line-angles, reference-independent. A bare [`TuningAbsolute`] returns
    /// its gaps as authored; a [`TuningRotated`] returns the same gaps walked
    /// from its re-based start (a cyclic permutation — the gap *values* are
    /// never recomputed).
    fn intervals(&self) -> TuningIntervals;

    /// How far this view's root line has been turned: a mantissa in `[0, 1)`.
    /// This is the single absolute fact a reference pitch contributes; changing
    /// the reference moves only this, never [`intervals`](Tuning::intervals).
    /// For a re-based view the `fract` is the snap that keeps the root in the
    /// base octave no matter how far it has shifted.
    fn rotation(&self) -> PitchLog2Interval;

    /// The number of lines, `N`.
    fn len(&self) -> usize {
        self.intervals().len()
    }

    /// Whether there are no lines.
    fn is_empty(&self) -> bool {
        self.intervals().is_empty()
    }

    /// Re-base the cylinder `k` lines **up**: the line `k` steps above the
    /// current root becomes the new root. Returns a fresh [`TuningRotated`] —
    /// nothing mutates. Because the cursor advances by integer `k mod N`,
    /// shifting in steps and shifting all at once land on the bit-identical
    /// cursor (`shift_up(1)` done `N` times is the identity), so the no-op
    /// closes exactly and no error accumulates across shifts.
    fn shift_up(&self, k: usize) -> TuningRotated;

    /// Re-base the cylinder `k` lines **down** — the inverse of
    /// [`shift_up`](Tuning::shift_up). `shift_down(k)` equals `shift_up(N − k)`:
    /// down is up the other way around the ring.
    fn shift_down(&self, k: usize) -> TuningRotated;
}

/// The owned, authored cylinder: a rigid [`TuningIntervals`] shape plus the one
/// global rotation, read from line 0. The root of all readings — a
/// [`TuningRotated`] is just this re-based by an integer cursor.
///
/// Flat and `Copy` (its [`TuningIntervals`] is a fixed array), so it crosses the
/// command boundary directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TuningAbsolute {
    /// The rigid shape — the line-angles, reference-independent.
    intervals: TuningIntervals,
    /// How far the whole bundle has been turned: a mantissa in `[0, 1)`. This
    /// is the single absolute fact a reference pitch contributes; changing the
    /// reference moves only this, never [`intervals`](TuningAbsolute::intervals).
    rotation: PitchLog2Interval,
}

impl TuningAbsolute {
    /// Place a shape by its global rotation. The rotation is folded to a
    /// mantissa ([`fract`](PitchLog2Interval::fract)), since turning the bundle
    /// by a whole octave is no turn at all.
    pub fn new(intervals: TuningIntervals, rotation: PitchLog2Interval) -> TuningAbsolute {
        TuningAbsolute {
            intervals,
            rotation: rotation.fract(),
        }
    }

    /// Build from the absolute Hz of each line, **root first**, plus the Hz of
    /// the line that closes the octave (the root, one floor up) so the final
    /// gap can be measured. The root's own angle on the cylinder becomes the
    /// global rotation; the shape is the gaps between successive lines.
    ///
    /// **Floating-point discipline.** Each line's angle is measured *once*,
    /// directly from the root (`log2(f[i]) − log2(f[0])`) — never chained
    /// line-to-line, so no rounding accumulates across lines. The stored gaps
    /// are then plain differences of those independent angles (`angle[i+1] −
    /// angle[i]`), **not** re-`fract`ed per gap: fracting each gap would let a
    /// hair-negative step wrap to ≈1.0 and would break the sum. Because the
    /// gaps are successive differences of one angle sequence, their sum
    /// *telescopes* to `angle[last] − angle[0]`, landing on one octave to
    /// machine precision — which is what makes
    /// [`well_formed`](TuningIntervals::well_formed) pass with a tight `eps`.
    ///
    /// Pass `n + 1` frequencies for `n` lines (the last closes the octave).
    pub fn from_frequencies(hz: impl IntoIterator<Item = f32>) -> TuningAbsolute {
        let pitches: Vec<PitchLog2> = hz.into_iter().map(PitchLog2::from_hz).collect();
        let Some(&root) = pitches.first() else {
            return TuningAbsolute::new(
                TuningIntervals::new(std::iter::empty()),
                PitchLog2Interval(0.0),
            );
        };
        // Each line's angle, measured once from the root — no chaining.
        let angles: Vec<PitchLog2Interval> = pitches.iter().map(|&p| p - root).collect();
        // Gaps are differences of those angles; the sum telescopes exactly.
        let gaps = angles.windows(2).map(|w| w[1] - w[0]);
        TuningAbsolute::new(TuningIntervals::new(gaps), (root - ORIGIN).fract())
    }
}

impl Tuning for TuningAbsolute {
    fn intervals(&self) -> TuningIntervals {
        self.intervals
    }

    fn rotation(&self) -> PitchLog2Interval {
        self.rotation
    }

    fn shift_up(&self, k: usize) -> TuningRotated {
        TuningRotated::new(*self, k)
    }

    fn shift_down(&self, k: usize) -> TuningRotated {
        let n = self.intervals.len();
        TuningRotated::new(*self, n - k % n)
    }
}

/// A re-based reading of a [`TuningAbsolute`]: the same authored cylinder, but
/// with line `start` taken as the root. Owns its [`TuningAbsolute`] (a shift
/// copies it — the cylinder is a flat `Copy` value of a few hundred bytes, and
/// shifts are rare configuration events, not a hot path) so the view is a
/// self-contained value, free of lifetimes.
///
/// The whole state is one integer cursor. Both the re-based shape and the
/// re-based rotation are derived from the immutable gaps on each read, so any
/// number of shifts lands on the same answer as one combined shift with no
/// floating-point drift.
///
/// Flat and `Copy`, so it crosses the command boundary directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TuningRotated {
    /// The authored cylinder, owned. Never mutated; a shift produces a new
    /// `TuningRotated` over a copy.
    tuning: TuningAbsolute,
    /// Which line of [`tuning`](TuningRotated::tuning) is this view's root,
    /// in `[0, N)`. Folded `mod N` at construction, so `start` and `start + N`
    /// are the same view.
    start: usize,
}

impl TuningRotated {
    /// Re-base `tuning` so line `start` (taken `mod N`) is the root.
    pub fn new(tuning: TuningAbsolute, start: usize) -> TuningRotated {
        let n = tuning.intervals.len();
        TuningRotated {
            start: if n == 0 { 0 } else { start % n },
            tuning,
        }
    }

    /// Which authored line is this view's root, in `[0, N)`.
    pub fn start(&self) -> usize {
        self.start
    }
}

impl Tuning for TuningRotated {
    /// The authored gaps walked from `start`: a cyclic permutation. The gap
    /// *values* are read straight from the authored array (no arithmetic), so
    /// re-basing never perturbs a gap.
    fn intervals(&self) -> TuningIntervals {
        let authored = &self.tuning.intervals;
        let n = authored.len();
        TuningIntervals::new((0..n).map(|i| gap_at(authored, self.start + i)))
    }

    /// The authored rotation plus the angle walked to `start`, snapped to the
    /// base octave. Re-summed fresh from the gaps each call — never carried
    /// across shifts, so no error accumulates.
    fn rotation(&self) -> PitchLog2Interval {
        let walked = angle_to(&self.tuning.intervals, self.start);
        (self.tuning.rotation + walked).fract()
    }

    fn shift_up(&self, k: usize) -> TuningRotated {
        // Re-base the *authored* cylinder, not self — keeps the cursor a single
        // integer add against the original, never a chain of views.
        TuningRotated::new(self.tuning, self.start + k)
    }

    fn shift_down(&self, k: usize) -> TuningRotated {
        let n = self.tuning.intervals.len();
        TuningRotated::new(self.tuning, self.start + (n - k % n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 12-TET from frequencies: gaps are all equal (1/12) and sum to 1.
    #[test]
    fn twelve_tet_gaps_are_even_and_close_the_octave() {
        // 13 frequencies = 12 lines + the closing octave (root doubled).
        let base = 261.6256_f32; // C4, arbitrary root
        let hz: Vec<f32> = (0..=12)
            .map(|i| base * 2f32.powf(i as f32 / 12.0))
            .collect();
        let t = TuningAbsolute::from_frequencies(hz);

        assert_eq!(t.len(), 12);
        assert!(t.intervals.well_formed(1e-5));
        for g in t.intervals.gaps() {
            assert!((g.0 - 1.0 / 12.0).abs() < 1e-5, "gap {} != 1/12", g.0);
        }
    }

    /// Just intonation: the gap from Sa (1/1) to Pa-relative steps reconstruct
    /// the true ratios, and the whole shape closes the octave.
    #[test]
    fn just_gaps_sum_to_one_octave() {
        // 5-limit ratios from the root, then the closing 2/1.
        let root = 440.0_f32;
        let ratios = [
            1.0,
            16.0 / 15.0,
            9.0 / 8.0,
            6.0 / 5.0,
            5.0 / 4.0,
            4.0 / 3.0,
            45.0 / 32.0,
            3.0 / 2.0,
            8.0 / 5.0,
            5.0 / 3.0,
            9.0 / 5.0,
            15.0 / 8.0,
            2.0,
        ];
        let hz: Vec<f32> = ratios.iter().map(|r| root * r).collect();
        let t = TuningAbsolute::from_frequencies(hz);

        assert_eq!(t.len(), 12);
        assert!(
            t.intervals.well_formed(1e-5),
            "Just shape must close the octave"
        );
    }

    /// Re-tuning the root (440 → 441) moves only the rotation, not the shape.
    #[test]
    fn reference_change_moves_rotation_not_shape() {
        let ratios = [
            1.0,
            9.0 / 8.0,
            5.0 / 4.0,
            4.0 / 3.0,
            3.0 / 2.0,
            5.0 / 3.0,
            15.0 / 8.0,
            2.0,
        ];
        let hz440: Vec<f32> = ratios.iter().map(|r| 440.0 * r).collect();
        let hz441: Vec<f32> = ratios.iter().map(|r| 441.0 * r).collect();

        let a = TuningAbsolute::from_frequencies(hz440);
        let b = TuningAbsolute::from_frequencies(hz441);

        // Gap-by-gap, not `==`: both gaps equal `log2(rᵢ₊₁/rᵢ)` algebraically,
        // but `log2(440·r)` and `log2(441·r)` round differently before the
        // subtract, so the shapes match only to float tolerance — not bitwise.
        // That is the claim: the reference cancels out of the shape.
        assert_eq!(a.intervals.len(), b.intervals.len());
        for (ga, gb) in a.intervals.rotations.iter().zip(&b.intervals.rotations) {
            assert!((ga.0 - gb.0).abs() < 1e-5, "shape is reference-free");
        }
        assert_ne!(a.rotation, b.rotation, "rotation carries the reference");
    }

    // ── Re-basing (shift) on a deliberately asymmetric tuning ────────────
    //
    // N = 3 with unequal gaps that sum to one octave, so every "the staircase
    // is uneven" claim shows up directly in the numbers (a single shift moves
    // the root by 0.5, two by 0.8 — never the even 1/3). base rotation 0.1.

    fn fake() -> TuningAbsolute {
        TuningAbsolute::new(
            TuningIntervals::new([
                PitchLog2Interval(0.5),
                PitchLog2Interval(0.3),
                PitchLog2Interval(0.2),
            ]),
            PitchLog2Interval(0.1),
        )
    }

    fn gaps(t: &impl Tuning) -> Vec<f32> {
        t.intervals().gaps().iter().map(|g| g.0).collect()
    }

    /// The keystone: `shift_up(1)` thrice equals `shift_up(3)` equals the
    /// unshifted view — and bitwise, because the cursor is integer `mod N`
    /// arithmetic, so the no-op closes exactly with no accumulated error.
    #[test]
    fn shifting_step_by_step_equals_all_at_once_and_closes() {
        let t = fake();
        let stepped = t.shift_up(1).shift_up(1).shift_up(1);
        let at_once = t.shift_up(3);
        assert_eq!(stepped.start(), at_once.start());
        assert_eq!(stepped.start(), 0, "three steps on N=3 returns to start");
        // Identical readings, exactly — no float drift across the chain.
        assert_eq!(gaps(&stepped), gaps(&t.shift_up(0)));
        assert_eq!(stepped.rotation(), t.shift_up(3).rotation());
    }

    /// Re-basing the gaps is a cyclic permutation: gap values are read straight
    /// from the authored array, never recomputed, so they match bit-for-bit.
    #[test]
    fn shift_permutes_gaps_without_recomputing_them() {
        let t = fake();
        // start = 0 → [0.5, 0.3, 0.2]; up 1 → [0.3, 0.2, 0.5]; up 2 → [0.2, 0.5, 0.3].
        assert_eq!(gaps(&t.shift_up(0)), vec![0.5, 0.3, 0.2]);
        assert_eq!(gaps(&t.shift_up(1)), vec![0.3, 0.2, 0.5]);
        assert_eq!(gaps(&t.shift_up(2)), vec![0.2, 0.5, 0.3]);
    }

    /// `shift_down(k)` is `shift_up(N − k)`: down is up the other way round.
    #[test]
    fn shift_down_is_shift_up_the_other_way() {
        let t = fake();
        assert_eq!(t.shift_down(1).start(), t.shift_up(2).start());
        assert_eq!(gaps(&t.shift_down(1)), gaps(&t.shift_up(2)));
        // Round-trip: up then down returns home.
        assert_eq!(t.shift_up(2).shift_down(2).start(), 0);
    }

    /// The rotation tracks the root along the *uneven* staircase, and the
    /// `fract` snaps it into the base octave on the wrap.
    #[test]
    fn rotation_walks_the_uneven_staircase_and_snaps() {
        let t = fake();
        // base 0.1; walk = cumulative gaps from start.
        assert!((t.shift_up(0).rotation().0 - 0.1).abs() < 1e-6); // 0.1 + 0.0
        assert!((t.shift_up(1).rotation().0 - 0.6).abs() < 1e-6); // 0.1 + 0.5
        assert!((t.shift_up(2).rotation().0 - 0.9).abs() < 1e-6); // 0.1 + 0.5 + 0.3
                                                                  // up 3 walks a full octave: (0.1 + 1.0).fract() == 0.1 — snapped back.
        assert!((t.shift_up(3).rotation().0 - 0.1).abs() < 1e-6);
    }

    /// The steps are genuinely unequal — shifting onto line 1 vs line 2 moves
    /// the root by different angles (0.5 vs 0.3), which an even tuning could
    /// never reproduce. This is the whole reason the rotation routes through
    /// the gaps and not through a flat `k/N`.
    #[test]
    fn single_shift_steps_are_uneven() {
        let t = fake();
        let step1 = t.shift_up(1).rotation().0 - t.shift_up(0).rotation().0; // 0.5
        let step2 = t.shift_up(2).rotation().0 - t.shift_up(1).rotation().0; // 0.3
        assert!((step1 - 0.5).abs() < 1e-6);
        assert!((step2 - 0.3).abs() < 1e-6);
        assert!(
            (step1 - step2).abs() > 0.1,
            "an uneven tuning has uneven steps"
        );
    }

    /// A bare `TuningAbsolute` reads identically to its zero-shift view — the
    /// trait makes the two interchangeable.
    #[test]
    fn absolute_is_the_zero_shift_view() {
        let t = fake();
        let zero = t.shift_up(0);
        assert_eq!(gaps(&t), gaps(&zero));
        assert_eq!(t.rotation(), zero.rotation());
        assert_eq!(t.len(), zero.len());
    }
}
