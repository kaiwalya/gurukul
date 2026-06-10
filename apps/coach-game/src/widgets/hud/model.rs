//! HUD model: the pure `MusicInfo` → display-row projection.
//!
//! The only music-aware layer of the slice. Today it produces one row:
//! the scale's tooth-widths against the tuning's slot count — the gaps
//! between successive degrees, closing the octave (`2 2 1 2 2 2 1` for
//! Bilawal). Plain Rust, no Bevy.

use domain_ports::app_coach::MusicInfo;
use domain_ports::tuning::Tuning;

/// Build the int row from a snapshot: the scale's tooth-widths against the
/// tuning's slot count, joined by spaces (`int 2 2 1 2 2 2 1` for Bilawal).
pub fn int_row(info: &MusicInfo) -> String {
    let n = info.scale.tuning().len() as u32;
    format!(
        "int {}",
        info.scale
            .intervals()
            .widths(n)
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::app_coach::MusicInfo;
    use domain_ports::pitch::PitchLog2;
    use domain_ports::scale::{Scale, ScaleIntervals};
    use domain_ports::tuning::{TuningAbsolute, TuningKind};

    fn bilawal_a440() -> MusicInfo {
        let tuning = TuningAbsolute::at_reference(
            TuningKind::TwelveTet.intervals(),
            PitchLog2::from_hz(440.0),
        );
        let intervals = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        // Sa on C, one octave below the A=440 reference.
        MusicInfo {
            scale: Scale::new(intervals, tuning.shift_up(3), 8),
        }
    }

    #[test]
    fn intervals_are_the_tooth_widths() {
        // Sa-relative tooth-widths, closing the octave (sum to 12).
        let int = int_row(&bilawal_a440());
        assert_eq!(int, "int 2 2 1 2 2 2 1");
    }
}
