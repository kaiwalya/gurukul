//! Scale-picker model: pure row labels and selected-scale calculation.
//!
//! The only music-aware layer of the slice. It turns the known-scale
//! catalogue into display labels (tooth-widths) and computes the new
//! [`Scale`] when a row is selected — keeping the current Sa rotation +
//! register, swapping only the tooth pattern. Plain Rust, no Bevy.

use domain_ports::scale::{Scale, ScaleIntervals};

/// Build the label for a shape: its tooth-widths against the tuning's slot
/// count `n`, joined by spaces (`"2 2 1 2 2 2 1"` for Bilawal) — the same
/// vocabulary the HUD's int row shows.
pub fn shape_label(shape: ScaleIntervals, n: u32) -> String {
    shape
        .widths(n)
        .iter()
        .map(|w| w.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Labels for every shape in the catalogue, in catalogue order, against the
/// active tuning's slot count `n`. The row at index `i` selects
/// `scales[i]`.
pub fn row_labels(scales: &[ScaleIntervals], n: u32) -> Vec<String> {
    scales.iter().map(|shape| shape_label(*shape, n)).collect()
}

/// Select a new shape: rebuild the [`Scale`] keeping the current Sa
/// (rotated tuning) and register, swapping only the tooth pattern.
pub fn select_scale(current: &Scale, intervals: ScaleIntervals) -> Scale {
    Scale::new(intervals, *current.tuning(), current.octave())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::pitch::PitchLog2;
    use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

    fn absolute() -> TuningAbsolute {
        TuningAbsolute::at_reference(TuningKind::TwelveTet.intervals(), PitchLog2::from_hz(440.0))
    }

    #[test]
    fn row_labels_render_tooth_widths() {
        let bilawal = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        let sparse = ScaleIntervals::from_mask(1);
        let labels = row_labels(&[bilawal, sparse], 12);
        assert_eq!(labels[0], "2 2 1 2 2 2 1");
        // A single-degree mask closes the octave in one 12-wide tooth.
        assert_eq!(labels[1], "12");
    }

    #[test]
    fn select_scale_preserves_sa_and_register() {
        let absolute = absolute();
        // Current: Bilawal, Sa shifted 5 slots up, register 8.
        let current = Scale::new(
            ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
            absolute.shift_up(5),
            8,
        );
        let new_shape = ScaleIntervals::from_widths(&[2, 1, 2, 2, 1, 2, 2]);
        let selected = select_scale(&current, new_shape);
        assert_eq!(selected.octave(), current.octave(), "register preserved");
        assert_eq!(selected.tuning(), current.tuning(), "Sa rotation preserved");
        assert_eq!(
            selected.intervals().widths(12),
            new_shape.widths(12),
            "tooth pattern swapped to the selected shape"
        );
    }
}
