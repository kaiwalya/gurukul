//! A scale as the **outer cylinder** that drops into a tuning's grooves.
//!
//! [`tuning.rs`](super::tuning) is a cylinder cut with grooves — `N` lines
//! parallel to the axis, one per slot (12 for 12-TET, the twelve equal
//! semitones; 22 for the shruti grid, a finer uneven division). A *scale* is a
//! second cylinder that fits over it, with teeth that drop into **only some** of
//! those grooves. The seven-note Bilawal scale, widths `[2, 2, 1, 2, 2, 2, 1]`,
//! is seven teeth landing in seven of the twelve 12-TET grooves; the gaps are
//! how many grooves each tooth skips. The same scale can drop in at any of the
//! tuning's rotations (12 for Bilawal on 12-TET), which is why "which groove is
//! **Sa**" (Sa = the tonic, the scale's root degree — the note the rest are
//! counted from) is a property of the *tuning's* rotation, not of the tooth
//! pattern.
//!
//! That split is the whole module:
//!
//! - [`ScaleIntervals`] is the **tooth pattern** — a `u32` bitmask, near-const,
//!   pure combinatorics. Bit `i` set ⇒ tuning slot `i` (counting up from Sa) is
//!   a scale degree. It is reference-free, tuning-free, rotation-free: just
//!   which grooves the teeth land in, with no commitment to *where* on the
//!   keyboard Sa sits. Catalogue entries live here.
//! - [`Scale`] is the placed object — a tooth pattern dropped onto a concrete
//!   tuning at a concrete register. It owns the [`ScaleIntervals`], the
//!   [`TuningRotated`] (which carries the *small rotation* — which groove is
//!   Sa), and an integer `octave` (the *big jump* — which floor on the helix,
//!   the male/female register the cylinder deliberately forgets).
//!
//! **No `f32` here.** A tooth-width is a whole count of grooves, a rotation is
//! an integer cursor, an octave is an integer floor. The continuous slide lives
//! in [`pitch.rs`](super::pitch) and the uneven groove spacing in
//! [`tuning.rs`](super::tuning); the scale layer is purely the integer
//! selection on top.

use crate::tuning::TuningRotated;

/// The tooth pattern: which of a tuning's grooves are scale degrees, as a
/// `u32` bitmask. **Bit 0 is Sa** (the tonic groove, always set); bit `i` set
/// means slot `i` counting up from Sa is a degree. Bilawal on a 12-slot tuning
/// is `{0, 2, 4, 5, 7, 9, 11}`.
///
/// Reference-free and tuning-free — like [`TuningIntervals`] one layer down, it
/// is *only* the pattern. It does not know which tuning it will drop into, where
/// Sa lands, or any frequency. Those arrive only when it is placed in a
/// [`Scale`]. Near-const: catalogue entries are fixed bit patterns with utility
/// readings ([`widths`](ScaleIntervals::widths),
/// [`note_count`](ScaleIntervals::note_count)) layered over them.
///
/// **Bit `i` is the tuning's *ordinal* line `i`** — the `i`-th groove up from
/// Sa, not `i` semitones and not `i`/N of an octave. That this index is ordinal
/// (which groove), not metric (what angle), is what lets an integer scale slide
/// over an *uneven* tuning: the definition lives on the [`Tuning`] trait
/// ("The line index is ordinal, not metric"), and the bitmask just counts in
/// it. On 12-TET ordinal and metric coincide; on 22-shruti they do not, and the
/// mask still means "these grooves," whatever uneven angles they resolve to.
///
/// [`TuningIntervals`]: crate::tuning::TuningIntervals
/// [`Tuning`]: crate::tuning::Tuning
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleIntervals {
    /// Bit `i` set ⇒ slot `i` (from Sa = bit 0) is a degree. The 32-bit width
    /// covers any tuning the system ships (12-TET, 22-shruti) with room over.
    mask: u32,
}

impl ScaleIntervals {
    /// Build from a raw bitmask. Bit 0 (Sa) is forced set — a scale always
    /// contains its own tonic, so a mask with bit 0 clear is a caller error,
    /// and forcing it keeps every `ScaleIntervals` self-consistent.
    pub fn from_mask(mask: u32) -> ScaleIntervals {
        ScaleIntervals { mask: mask | 1 }
    }

    /// Build from tooth-widths — the gaps between successive degrees, walking up
    /// from Sa. `[2, 2, 1, 2, 2, 2, 1]` for Bilawal: Sa, then +2 grooves to Re,
    /// +2 to Ga, +1 to Ma, and so on. The final width closes the octave (steps
    /// from the last degree back onto Sa one floor up) and is *not* a set bit —
    /// it only advances the running slot so the widths sum to the tuning's slot
    /// count `N`.
    pub fn from_widths(widths: &[u32]) -> ScaleIntervals {
        let mut mask = 1u32; // Sa
        let mut slot = 0u32;
        // Every width but the last sets a degree; the last only closes the octave.
        for &w in widths.iter().take(widths.len().saturating_sub(1)) {
            slot += w;
            debug_assert!(slot < 32, "scale degree at slot {slot} exceeds u32 mask");
            mask |= 1 << slot;
        }
        ScaleIntervals { mask }
    }

    /// The raw bitmask.
    pub fn mask(self) -> u32 {
        self.mask
    }

    /// The number of degrees — the set-bit count (popcount). 7 for a heptatonic
    /// scale like Bilawal. This is the scale's note count `n`, the period scale
    /// degrees fold against.
    pub fn note_count(self) -> usize {
        self.mask.count_ones() as usize
    }

    /// The tooth-widths: gaps between successive set bits, with a final width
    /// that closes the octave up to slot count `n`. Inverse of
    /// [`from_widths`](ScaleIntervals::from_widths) — `from_widths(&iv.widths(n))`
    /// round-trips. `n` is the tuning's slot count (12, 22), needed because the
    /// closing width (last degree → Sa one octave up) is measured against it and
    /// is not recoverable from the mask alone.
    pub fn widths(self, n: u32) -> Vec<u32> {
        let degrees: Vec<u32> = (0..n).filter(|&i| self.mask & (1 << i) != 0).collect();
        let mut widths = Vec::with_capacity(degrees.len());
        for pair in degrees.windows(2) {
            widths.push(pair[1] - pair[0]);
        }
        // Closing width: last degree up to Sa one octave on (slot n).
        if let Some(&last) = degrees.last() {
            widths.push(n - last);
        }
        widths
    }
}

/// A scale placed on a concrete tuning at a concrete register: the tooth pattern
/// dropped into the grooves, with both motions of the tonic resolved.
///
/// The tonic is two independent integer moves, and each lives where it belongs:
///
/// - the **small rotation** — which groove is Sa — lives inside the owned
///   [`TuningRotated`], whose integer cursor *is* "which of the N lines is the
///   root." The scale does not store it separately; re-rooting Sa is re-basing
///   the tuning.
/// - the **big jump** — which octave (male vs female register) — is the integer
///   `octave` here, the helix floor count [`tuning.rs`](super::tuning)
///   deliberately drops. Signed: a low register sits below the reference.
///
/// `ScaleIntervals` carries no `f32`, `TuningRotated`'s rotation is an integer
/// cursor, and `octave` is an integer floor — so the whole tonic placement is
/// clicky and integral, never a continuous slide.
#[derive(Debug, Clone, PartialEq)]
pub struct Scale {
    /// The tooth pattern — which grooves are degrees.
    intervals: ScaleIntervals,
    /// The tuning the teeth drop into, already re-based so its root line is Sa.
    /// Owns the small rotation.
    tuning: TuningRotated,
    /// Which octave (helix floor) Sa sits on — the register jump. Signed.
    octave: i32,
}

impl Scale {
    /// Place a tooth pattern on a rotated tuning at a register.
    pub fn new(intervals: ScaleIntervals, tuning: TuningRotated, octave: i32) -> Scale {
        Scale {
            intervals,
            tuning,
            octave,
        }
    }

    /// The tooth pattern.
    pub fn intervals(&self) -> ScaleIntervals {
        self.intervals
    }

    /// The rotated tuning the teeth drop into (carries the small rotation).
    pub fn tuning(&self) -> &TuningRotated {
        &self.tuning
    }

    /// Which octave (helix floor) Sa sits on.
    pub fn octave(&self) -> i32 {
        self.octave
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bilawal's widths and its mask are two faces of the same pattern, and
    /// round-trip through each other on a 12-slot tuning.
    #[test]
    fn bilawal_widths_and_mask_round_trip() {
        let widths = [2, 2, 1, 2, 2, 2, 1];
        let iv = ScaleIntervals::from_widths(&widths);
        // Degrees: Sa, Re, Ga, Ma, Pa, Dha, Ni at slots 0,2,4,5,7,9,11.
        assert_eq!(iv.mask(), 0b1010_1011_0101);
        assert_eq!(iv.note_count(), 7);
        assert_eq!(iv.widths(12), widths);
    }

    /// Sa (bit 0) is always present, even from a mask that omits it.
    #[test]
    fn sa_is_always_set() {
        let iv = ScaleIntervals::from_mask(0b1010_1011_0100); // bit 0 clear
        assert_eq!(iv.mask() & 1, 1);
    }

    /// A pentatonic pattern folds at five degrees and closes the octave.
    #[test]
    fn pentatonic_closes_the_octave() {
        // Sa Re Ga Pa Dha: widths 2,2,3,2,3 sum to 12.
        let iv = ScaleIntervals::from_widths(&[2, 2, 3, 2, 3]);
        assert_eq!(iv.note_count(), 5);
        assert_eq!(iv.widths(12).iter().sum::<u32>(), 12);
    }
}
