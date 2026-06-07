//! Pitch on a log2 line.
//!
//! A pitch is a frequency — a value on a line, in Hz. But musically the line
//! is the wrong one. Octaves are special: 220→440 Hz *feels* like the same
//! step as 440→880 Hz, even though one spans 220 Hz and the other 440. What
//! the ear hears as "the same step" is an equal *ratio* (×2), not an equal
//! difference. Hz spaces ratios unevenly, so on the Hz line the octave keeps
//! growing.
//!
//! Take `log2(Hz)` and the unevenness disappears: equal ratios become equal
//! distances. ×2 in Hz is +1 in log2, so every octave is the same width and
//! octaves become plain linear increments instead of doublings. This is
//! **`PitchLog2` space** — every point freely converts back to Hz (store
//! `10`, the real pitch is `2^10 = 1024 Hz`).
//!
//! One tension remains: pitch is **linear** (higher is forever higher) and yet
//! **circular** (an octave up is "the same note"). Both are true at once if the
//! line is wound into a **helix** — one full turn per octave. Then any interval
//! splits into how many turns (whole octaves) and how far around the last turn
//! (where within the octave), which is just the integer/fractional divmod of
//! its one log2 number. This module stays on the bare line and never needs the
//! helix to do its arithmetic; it surfaces only where an interval is read as an
//! [`angle`](PitchLog2Interval::angle), the radians around that winding.
//!
//! Two types live on this line:
//!
//! - [`PitchLog2`] — a **point**: one `log2(Hz)` value, a pitch. It has no
//!   privileged zero — log2 fixes where `1 Hz` lands (`0`), but "which
//!   octave you're in" is only meaningful *relative to a reference you pick*.
//!   So a point exposes no octave or within-octave reading of its own; you
//!   get those by subtracting a reference first.
//! - [`PitchLog2Interval`] — a **difference**: one point minus another. This
//!   is the thing that has a well-defined octave count, because subtracting
//!   cancels the unchosen zero (`(a + c) − (b + c) = a − b`). The same one
//!   number reads in any unit — log2 steps, Δ-Hz, or radians
//!   ([`angle`](PitchLog2Interval::angle), one turn per octave) — and the
//!   octave/within-octave split is the same divmod in every one.
//!
//! **The only path from a point to "which octave / where in the octave" is
//! through an interval.** Subtract a reference to get a difference, then read
//! the difference's [`full_octaves`](PitchLog2Interval::full_octaves) and
//! [`mantissa`](PitchLog2Interval::mantissa). Point-minus-point first, read
//! second: nothing here may depend on an absolute pitch, only on a difference
//! from a chosen zero.
//!
//! The arithmetic enforces it. A point can be *differenced* (point − point →
//! interval) or *moved* (point ± interval → point), and nothing else. Adding
//! two points is the nonsense of adding two zeros, so the impl simply does
//! not exist and `pitch + pitch` will not compile.

use std::ops::{Add, Neg, Sub};

/// A point on the log2 line: `log2(Hz)`.
///
/// One real number is a whole pitch. It carries no octave reading of its own
/// — that's reference-relative, so you get it by subtracting a reference to a
/// [`PitchLog2Interval`] (see the module docs). The only arithmetic a point
/// allows is differencing with another point and moving by an interval;
/// `PitchLog2 + PitchLog2` is deliberately not implemented.
///
/// `f32`, so `PartialEq`/`PartialOrd` but not `Eq`/`Ord`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct PitchLog2(pub f32);

/// A signed difference between two points on the log2 line.
///
/// Unlike a point, a difference has no unchosen zero left in it, so it splits
/// cleanly into whole octaves and a leftover. This is the type that moves a
/// point along the line, and the type whose `full_octaves`/`mantissa` read
/// off "how many octaves" and "where within one".
///
/// `f32`, so `PartialEq`/`PartialOrd` but not `Eq`/`Ord`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct PitchLog2Interval(pub f32);

impl PitchLog2 {
    /// The point for a frequency: `log2(hz)`.
    ///
    /// This is the fixed chart between Hz and the helix (1 Hz ↦ 0), not a
    /// chosen origin — so it carries no unchosen zero of the kind a point's
    /// octave reading would depend on (see the module docs).
    pub fn from_hz(hz: f32) -> PitchLog2 {
        PitchLog2(hz.log2())
    }

    /// The frequency of this point: `exp2(self)`. Inverse of [`from_hz`].
    ///
    /// [`from_hz`]: PitchLog2::from_hz
    pub fn to_hz(self) -> f32 {
        self.0.exp2()
    }
}

impl PitchLog2Interval {
    /// An interval of `n` whole octaves. One octave is `+1.0` in log2, so
    /// this is the named way to write the octave move — `pitch +
    /// PitchLog2Interval::octaves(2)` instead of a bare `2.0` whose meaning
    /// the reader has to recall.
    pub fn octaves(n: i32) -> PitchLog2Interval {
        PitchLog2Interval(n as f32)
    }

    /// How many whole octaves this difference spans — the **integer part** of
    /// the log2 value. Since +1 in log2 is ×2 in Hz, each whole step is one
    /// octave, and this counts them. Pairs with [`mantissa`], which is the
    /// fractional remainder.
    ///
    /// Signed: a difference below the reference reads negative octaves. The
    /// split is Euclidean, so it agrees with [`mantissa`] staying in `[0, 1)`
    /// — a difference of `−0.3` is octave `−1` with mantissa `0.7`, never
    /// octave `0` with mantissa `−0.3`.
    ///
    /// [`mantissa`]: PitchLog2Interval::mantissa
    pub fn full_octaves(self) -> i32 {
        self.0.div_euclid(1.0) as i32
    }

    /// Where within one octave this difference lands — the **fractional part**
    /// of the log2 value, always in `[0, 1)`. Adding a whole number to it
    /// jumps octaves (×2 in Hz each step) without moving this position, so two
    /// differences with the same mantissa are a whole number of octaves apart.
    /// It is the octave-independent leftover after [`full_octaves`].
    ///
    /// [`full_octaves`]: PitchLog2Interval::full_octaves
    pub fn mantissa(self) -> f32 {
        self.0.rem_euclid(1.0)
    }

    /// This difference with its **mantissa dropped** — snapped down to a whole
    /// number of octaves. Like `f32::floor`, but in pitch-space and staying a
    /// `PitchLog2Interval` so it composes. The whole-octaves twin of
    /// [`fract`]; `floor() + fract() == self`.
    ///
    /// Euclidean, matching [`full_octaves`]/[`mantissa`]: a negative difference
    /// floors *down* (`−0.3` → `−1.0`), not toward zero as `f32::floor` of a
    /// truncated value would — this is what keeps [`fract`] in `[0, 1)`.
    ///
    /// [`fract`]: PitchLog2Interval::fract
    /// [`full_octaves`]: PitchLog2Interval::full_octaves
    /// [`mantissa`]: PitchLog2Interval::mantissa
    pub fn floor(self) -> PitchLog2Interval {
        PitchLog2Interval(self.0.div_euclid(1.0))
    }

    /// This difference as an **angle in radians**, one octave per full turn
    /// (`self * 2π`). A projection, the radian twin of [`to_hz`] on a point: it
    /// reinterprets the one log2 number in a different unit, nothing more. It
    /// knows no tuning and no rotation — that lives in the tuning layer; this
    /// is purely the interval's own winding around the helix.
    ///
    /// **Unbounded on purpose.** Two octaves is `4π`, not `0` — octaves are
    /// kept, not wrapped. The integer-turn / leftover split is the same divmod
    /// as everywhere else, only scaled: `full_octaves() == (angle() /
    /// TAU).floor()`, and the dial's `[0, 2π)` reading is `fract().angle()`.
    /// Drop octaves *explicitly* with [`fract`] first; this method never does
    /// it for you.
    ///
    /// [`to_hz`]: PitchLog2::to_hz
    /// [`full_octaves`]: PitchLog2Interval::full_octaves
    /// [`fract`]: PitchLog2Interval::fract
    pub fn angle(self) -> f32 {
        self.0 * std::f32::consts::TAU
    }

    /// This difference with its **octaves dropped** — the within-octave
    /// remainder, kept as a `PitchLog2Interval` so it composes. The
    /// interval-valued twin of [`mantissa`] (same value, in `[0, 1)`); the
    /// fractional twin of [`floor`]; `floor() + fract() == self`.
    ///
    /// Euclidean, like `f32::fract` in spirit but always non-negative.
    ///
    /// [`mantissa`]: PitchLog2Interval::mantissa
    /// [`floor`]: PitchLog2Interval::floor
    pub fn fract(self) -> PitchLog2Interval {
        PitchLog2Interval(self.0.rem_euclid(1.0))
    }
}

// ── Affine algebra ───────────────────────────────────────────────────────
//
// A point can be differenced or moved; the nonsense operations are absent.
// `PitchLog2 + PitchLog2` has no impl, so it will not compile — adding two
// pitches (two unchosen zeros) is meaningless, and the missing impl is how
// the type system says so.

/// Point − point: the difference between two pitches.
impl Sub for PitchLog2 {
    type Output = PitchLog2Interval;
    fn sub(self, rhs: PitchLog2) -> PitchLog2Interval {
        PitchLog2Interval(self.0 - rhs.0)
    }
}

/// Point + interval: move a pitch along the helix.
impl Add<PitchLog2Interval> for PitchLog2 {
    type Output = PitchLog2;
    fn add(self, rhs: PitchLog2Interval) -> PitchLog2 {
        PitchLog2(self.0 + rhs.0)
    }
}

/// Point − interval: move a pitch the other way.
impl Sub<PitchLog2Interval> for PitchLog2 {
    type Output = PitchLog2;
    fn sub(self, rhs: PitchLog2Interval) -> PitchLog2 {
        PitchLog2(self.0 - rhs.0)
    }
}

/// Interval + interval: compose two moves.
impl Add for PitchLog2Interval {
    type Output = PitchLog2Interval;
    fn add(self, rhs: PitchLog2Interval) -> PitchLog2Interval {
        PitchLog2Interval(self.0 + rhs.0)
    }
}

/// Interval − interval: compose two moves.
impl Sub for PitchLog2Interval {
    type Output = PitchLog2Interval;
    fn sub(self, rhs: PitchLog2Interval) -> PitchLog2Interval {
        PitchLog2Interval(self.0 - rhs.0)
    }
}

/// Negate an interval: flip its direction.
impl Neg for PitchLog2Interval {
    type Output = PitchLog2Interval;
    fn neg(self) -> PitchLog2Interval {
        PitchLog2Interval(-self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn angle_winds_one_turn_per_octave() {
        assert!((PitchLog2Interval::octaves(2).angle() - 2.0 * TAU).abs() < 1e-6);
        assert!((PitchLog2Interval(-1.0).angle() + TAU).abs() < 1e-6);
    }

    #[test]
    fn angle_shares_the_octave_mantissa_divmod() {
        // angle() is the same divmod as full_octaves()/fract(), only scaled.
        let i = PitchLog2Interval(2.585); // two octaves and a fifth-ish
        assert_eq!((i.angle() / TAU).floor() as i32, i.full_octaves());
        assert!((i.fract().angle() - i.mantissa() * TAU).abs() < 1e-6);
    }
}
