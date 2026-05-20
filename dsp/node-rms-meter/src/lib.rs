use engine::{Node, NodeRegistry, PortSpec, PortType};
use std::collections::HashMap;

/// Per-block true RMS meter. Reads `audio_in`, publishes sqrt(mean(sample^2)) over each block
/// to `rms` (Control, zero-order-held across the block). No ballistics, no smoothing — this is
/// the raw numeric readout. For display-smoothed meters (VU, PPM, LUFS) add them as separate
/// node crates.
pub struct RmsMeter;

impl Node for RmsMeter {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        let input = match inputs.first() {
            Some(s) => s,
            None => return,
        };

        let n = nframes.max(1) as f64;
        let sum_sq: f64 = input[..nframes].iter().map(|&s| (s as f64).powi(2)).sum();
        let rms = (sum_sq / n).sqrt() as f32;

        if let Some(out) = outputs.get_mut(0) {
            out[..nframes].fill(rms);
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "RmsMeter",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "rms",
            ty: PortType::Control,
        }],
        vec![],
        Box::new(|_params: &HashMap<String, f64>| Box::new(RmsMeter) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_node(node: &mut RmsMeter, signal: &[f32], block_size: usize) -> Vec<f32> {
        node.prepare("test", 48000, block_size);
        let mut rms_buf = vec![0.0f32; block_size];

        let n_blocks = signal.len().div_ceil(block_size);
        for b in 0..n_blocks {
            let start = b * block_size;
            let end = (start + block_size).min(signal.len());
            let nframes = end - start;
            let slice = &signal[start..end];
            node.process(&[slice], &mut [&mut rms_buf[..nframes]], nframes);
        }
        rms_buf
    }

    #[test]
    fn dc_signal_rms() {
        // DC of 0.5: every sample of every block should be exactly 0.5.
        let signal: Vec<f32> = vec![0.5f32; 1024];
        let mut node = RmsMeter;
        let rms_buf = run_node(&mut node, &signal, 256);
        for &v in &rms_buf {
            assert!((v - 0.5).abs() < 1e-6, "rms sample={v}");
        }
    }

    #[test]
    fn sine_rms() {
        // 440 Hz sine at amplitude 1.0 for one block at sr=48000.
        // True RMS of a full-cycle sine is 1/sqrt(2) ~= 0.7071, but one block is unlikely to
        // hold an integer number of cycles, so allow +/-0.05.
        let sr = 48000u32;
        let block_size = 512;
        let freq = 440.0f32;
        let signal: Vec<f32> = (0..block_size)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin())
            .collect();
        let mut node = RmsMeter;
        let rms_buf = run_node(&mut node, &signal, block_size);
        let expected = 1.0f32 / 2.0f32.sqrt();
        for &v in &rms_buf {
            assert!(
                (v - expected).abs() < 0.05,
                "sine rms={v}, expected approx {expected}"
            );
        }
    }

    #[test]
    fn zero_input_is_zero() {
        let signal: Vec<f32> = vec![0.0f32; 1024];
        let mut node = RmsMeter;
        let rms_buf = run_node(&mut node, &signal, 256);
        for &v in &rms_buf {
            assert_eq!(v, 0.0, "expected zero rms for silent input");
        }
    }
}
