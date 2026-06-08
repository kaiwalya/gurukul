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

use crate::pitch::{PitchLog2, PitchLog2Interval};
use crate::tuning::{Tuning, TuningRotated, ORIGIN};

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

    /// The tuning slots that are degrees, in ascending order: the set-bit
    /// positions of the mask. Bilawal on 12-TET is `[0, 2, 4, 5, 7, 9, 11]`.
    /// This is the Sa-relative semitone (more precisely, *ordinal slot*) of
    /// each degree — the "deg" reading a math view shows, independent of where
    /// Sa sits on the helix.
    pub fn degree_slots(self) -> Vec<u32> {
        (0..u32::BITS)
            .filter(|&i| self.mask & (1 << i) != 0)
            .collect()
    }

    /// The tuning **slot** the `degree`-th tooth lands in: the index of the
    /// `degree`-th set bit, counting from Sa = degree 0 at bit 0. Bilawal's
    /// degree 3 (Ma) is slot 5 on 12-TET — the fourth set bit of
    /// `{0, 2, 4, 5, …}`. This is the ordinal scale degree → ordinal tuning slot
    /// map, the bridge from the tooth count to the groove index
    /// [`TuningIntervals::cumulative_rotation_to`] reads.
    ///
    /// Degrees past the last fold by octave: `degree == note_count()` returns
    /// slot `N` (Sa one octave up), `note_count() + 1` the next degree a floor
    /// higher, and so on — the same carry-the-octave behavior as the layers
    /// below, so a scale degree may climb without bound.
    ///
    /// [`TuningIntervals::cumulative_rotation_to`]: crate::tuning::TuningIntervals::cumulative_rotation_to
    pub fn slot_of_degree(self, degree: usize, n: u32) -> u32 {
        let count = self.note_count();
        let (octaves, rem) = (degree / count, degree % count);
        let in_octave = (0..n)
            .filter(|&i| self.mask & (1 << i) != 0)
            .nth(rem)
            .expect("rem < note_count so the rem-th set bit exists within N");
        in_octave + octaves as u32 * n
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
#[derive(Debug, Clone, Copy, PartialEq)]
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

    /// The concrete pitch of scale degree `i` (Sa = 0, Re = 1, …), resolved
    /// onto the helix. The degree picks a tuning slot
    /// ([`ScaleIntervals::slot_of_degree`]), the slot picks a cumulative
    /// rotation up from the root ([`TuningIntervals::cumulative_rotation_to`]),
    /// and that angle is added to **Sa**.
    ///
    /// `pitch_at(0)` *is* Sa, the tonic: degree 0 maps to slot 0, whose
    /// cumulative rotation is zero, so it reduces to Sa itself — both motions
    /// of the tonic resolved onto the helix (`ORIGIN + rotation + octave`, the
    /// single point that re-attaches the octave the tuning layer drops). Every
    /// other degree is a cumulative rotation up from there.
    ///
    /// Octave-carrying: degrees at or past the note count climb floors (degree
    /// `n` is Sa one octave up), because both the slot map and the cumulative
    /// rotation carry octaves rather than wrapping. So this resolves any degree,
    /// not only the ones in the base octave.
    ///
    /// [`ScaleIntervals::slot_of_degree`]: ScaleIntervals::slot_of_degree
    /// [`TuningIntervals::cumulative_rotation_to`]: crate::tuning::TuningIntervals::cumulative_rotation_to
    /// [`ORIGIN`]: crate::tuning::ORIGIN
    pub fn pitch_at(&self, i: usize) -> PitchLog2 {
        let n = self.tuning.len() as u32;
        let slot = self.intervals.slot_of_degree(i, n);
        let angle = self
            .tuning
            .intervals()
            .cumulative_rotation_to(slot as usize);
        let sa = ORIGIN + self.tuning.rotation() + PitchLog2Interval::octaves(self.octave);
        sa + angle
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

    /// Each degree maps to the slot of its set bit, and degrees past the count
    /// carry the octave: Bilawal degree 7 is Sa one floor up (slot 12).
    #[test]
    fn slot_of_degree_walks_set_bits_and_carries_octaves() {
        let iv = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        // Sa Re Ga Ma Pa Dha Ni at slots 0,2,4,5,7,9,11.
        let slots: Vec<u32> = (0..7).map(|d| iv.slot_of_degree(d, 12)).collect();
        assert_eq!(slots, [0, 2, 4, 5, 7, 9, 11]);
        // Degree 7 folds: Sa one octave up.
        assert_eq!(iv.slot_of_degree(7, 12), 12);
        // Degree 8 is Re one octave up.
        assert_eq!(iv.slot_of_degree(8, 12), 14);
    }

    use crate::tuning::{Tuning, TuningKind};

    /// On 12-TET the resolver reproduces equal-temperament frequencies. The
    /// rotation carries only the *pitch class* of 440 (its octave-free residue);
    /// the floor is the integer `octave`, anchored at ORIGIN = 1 Hz. So Sa lands
    /// on 440 when `octave = floor(log2 440) = 8`, and from there the Bilawal
    /// degrees are 440 × 2^(k/12) for k in the slots, Sa one octave up is 880.
    #[test]
    fn pitch_at_reproduces_twelve_tet_frequencies() {
        let rotation = (PitchLog2::from_hz(440.0) - crate::tuning::ORIGIN).fract();
        let tuning =
            crate::tuning::TuningAbsolute::new(TuningKind::TwelveTet.intervals(), rotation);
        let bilawal = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        let octave = (440f32.log2()).floor() as i32; // 8
        let scale = Scale::new(bilawal, tuning.shift_up(0), octave);

        // pitch_at(0) is Sa, the tonic.
        assert!((scale.pitch_at(0).to_hz() - 440.0).abs() < 1e-2);
        for (degree, slot) in [(0, 0), (1, 2), (2, 4), (3, 5)] {
            let want = 440.0 * 2f32.powf(slot as f32 / 12.0);
            assert!((scale.pitch_at(degree).to_hz() - want).abs() < 1e-2);
        }
        // Degree 7 is Sa one octave up: 880 Hz.
        assert!((scale.pitch_at(7).to_hz() - 880.0).abs() < 1e-2);
    }

    /// The integer octave is a clean register jump: the same scale at octave −1
    /// halves every resolved frequency.
    #[test]
    fn octave_halves_the_register() {
        let rotation = (PitchLog2::from_hz(440.0) - crate::tuning::ORIGIN).fract();
        let tuning =
            crate::tuning::TuningAbsolute::new(TuningKind::TwelveTet.intervals(), rotation);
        let iv = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        let high = Scale::new(iv, tuning.shift_up(0), 0);
        let low = Scale::new(iv, tuning.shift_up(0), -1);
        assert!((high.pitch_at(0).to_hz() / low.pitch_at(0).to_hz() - 2.0).abs() < 1e-3);
    }
}
