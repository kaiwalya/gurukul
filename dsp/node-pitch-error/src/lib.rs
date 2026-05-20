use engine::{Node, NodeRegistry, PortSpec, PortType};
use std::collections::HashMap;

/// Computes signed pitch error in cents between an estimated and reference F0.
///
/// Inputs:
///   `f0_estimated` (Feature) — detector output; 0.0 sentinel = unvoiced.
///   `f0_reference` (Feature) — ground-truth F0; 0.0 sentinel = unvoiced.
///
/// Outputs:
///   `error_cents` (Feature) — `1200 * log2(f0_estimated / f0_reference)`. Emits 0.0
///       when either input is unvoiced (≤ 0). ZOH across the block.
///   `voiced` (Control) — 1.0 when both inputs are > 0 this block, else 0.0. ZOH.
pub struct PitchError;

impl Node for PitchError {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        // inputs[0] = f0_estimated, inputs[1] = f0_reference
        // outputs[0] = error_cents, outputs[1] = voiced
        let f0_est = inputs
            .first()
            .and_then(|s| s.last().copied())
            .unwrap_or(0.0);
        let f0_ref = inputs.get(1).and_then(|s| s.last().copied()).unwrap_or(0.0);

        let (error_cents, voiced) = if f0_est > 0.0 && f0_ref > 0.0 {
            let cents = 1200.0 * (f0_est / f0_ref).log2();
            (cents, 1.0f32)
        } else {
            (0.0f32, 0.0f32)
        };

        if let Some(out) = outputs.first_mut() {
            out[..nframes].fill(error_cents);
        }
        if let Some(out) = outputs.get_mut(1) {
            out[..nframes].fill(voiced);
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "PitchError",
        vec![
            PortSpec {
                name: "f0_estimated",
                ty: PortType::Feature,
            },
            PortSpec {
                name: "f0_reference",
                ty: PortType::Feature,
            },
        ],
        vec![
            PortSpec {
                name: "error_cents",
                ty: PortType::Feature,
            },
            PortSpec {
                name: "voiced",
                ty: PortType::Control,
            },
        ],
        vec![],
        Box::new(|_params: &HashMap<String, f64>| Box::new(PitchError) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_block(f0_est: f32, f0_ref: f32) -> (f32, f32) {
        let mut node = PitchError;
        node.prepare("test", 48000, 512);

        let est_buf = vec![f0_est; 512];
        let ref_buf = vec![f0_ref; 512];
        let mut cents_buf = vec![0.0f32; 512];
        let mut voiced_buf = vec![0.0f32; 512];

        node.process(
            &[&est_buf, &ref_buf],
            &mut [&mut cents_buf, &mut voiced_buf],
            512,
        );

        (cents_buf[511], voiced_buf[511])
    }

    #[test]
    fn equal_inputs_zero_cents_voiced() {
        let (cents, voiced) = run_block(440.0, 440.0);
        assert!(
            (cents).abs() < 1e-4,
            "equal inputs: expected 0 cents, got {cents}"
        );
        assert_eq!(voiced, 1.0);
    }

    #[test]
    fn octave_up_1200_cents() {
        let (cents, voiced) = run_block(880.0, 440.0);
        assert!(
            (cents - 1200.0).abs() < 1e-3,
            "octave up: expected 1200 cents, got {cents}"
        );
        assert_eq!(voiced, 1.0);
    }

    #[test]
    fn octave_down_minus_1200_cents() {
        let (cents, voiced) = run_block(220.0, 440.0);
        assert!(
            (cents + 1200.0).abs() < 1e-3,
            "octave down: expected -1200 cents, got {cents}"
        );
        assert_eq!(voiced, 1.0);
    }

    #[test]
    fn unvoiced_when_either_input_zero() {
        let (cents0, voiced0) = run_block(0.0, 440.0);
        assert_eq!(cents0, 0.0, "est=0: expected 0 cents");
        assert_eq!(voiced0, 0.0, "est=0: expected unvoiced");

        let (cents1, voiced1) = run_block(440.0, 0.0);
        assert_eq!(cents1, 0.0, "ref=0: expected 0 cents");
        assert_eq!(voiced1, 0.0, "ref=0: expected unvoiced");
    }

    #[test]
    fn ten_cent_offset_sanity() {
        let f0_ref = 440.0f32;
        let f0_est = 440.0f32 * 2.0f32.powf(10.0 / 1200.0);
        let (cents, voiced) = run_block(f0_est, f0_ref);
        assert!(
            (cents - 10.0).abs() < 0.01,
            "10-cent offset: expected ~10 cents, got {cents}"
        );
        assert_eq!(voiced, 1.0);
    }
}
