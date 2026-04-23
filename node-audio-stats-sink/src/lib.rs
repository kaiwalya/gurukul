use engine::{Node, NodeError, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct AudioStatsSink {
    id: String,
    expected_rms: f64,
    tolerance_db: f64,
    count: u64,
    sum: f64,
    sum_sq: f64,
    peak: f32,
    min: f32,
    max: f32,
}

impl AudioStatsSink {
    fn new(expected_rms: f64, tolerance_db: f64) -> Self {
        Self {
            id: String::new(),
            expected_rms,
            tolerance_db,
            count: 0,
            sum: 0.0,
            sum_sq: 0.0,
            peak: 0.0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
        }
    }
}

impl Node for AudioStatsSink {
    fn prepare(&mut self, id: &str, _sample_rate: u32, _block_size: usize) {
        self.id = id.to_string();
        self.count = 0;
        self.sum = 0.0;
        self.sum_sq = 0.0;
        self.peak = 0.0;
        self.min = f32::INFINITY;
        self.max = f32::NEG_INFINITY;
    }

    fn process(&mut self, inputs: &[&[f32]], _outputs: &mut [&mut [f32]], nframes: usize) {
        let input = match inputs.first() {
            Some(s) => s,
            None => return,
        };
        for &s in &input[..nframes] {
            self.count += 1;
            self.sum += s as f64;
            self.sum_sq += (s as f64).powi(2);
            let abs_s = s.abs();
            if abs_s > self.peak {
                self.peak = abs_s;
            }
            if s < self.min {
                self.min = s;
            }
            if s > self.max {
                self.max = s;
            }
        }
    }

    fn finish(&mut self) -> Result<(), NodeError> {
        let count = self.count.max(1); // avoid divide-by-zero on empty
        let mean = self.sum / count as f64;
        let rms = (self.sum_sq / count as f64).sqrt();

        print!(
            "audio-stats[{}]: n={} rms={:.6} peak={:.6} min={:.6} max={:.6} mean={:.6}",
            self.id, self.count, rms, self.peak, self.min, self.max, mean
        );

        if self.expected_rms.is_finite() {
            let diff_db = (20.0 * (rms / self.expected_rms).log10()).abs();
            if diff_db <= self.tolerance_db {
                println!(" PASS");
                Ok(())
            } else {
                println!(
                    " FAIL (expected_rms={:.6}, diff={:.3}dB, tol={}dB)",
                    self.expected_rms, diff_db, self.tolerance_db
                );
                Err(NodeError(format!(
                    "audio-stats[{}]: FAIL (expected_rms={:.6}, diff={:.3}dB, tol={}dB)",
                    self.id, self.expected_rms, diff_db, self.tolerance_db
                )))
            }
        } else {
            // NaN expected_rms means no check — always pass.
            println!();
            Ok(())
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "AudioStatsSink",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![],
        vec![
            ParamSpec {
                name: "expected_rms",
                default: f64::NAN,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamSpec {
                name: "tolerance_db",
                default: 0.5,
                min: 0.0,
                max: 60.0,
                unit: "dB",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let expected_rms = *params.get("expected_rms").unwrap_or(&f64::NAN);
            let tolerance_db = *params.get("tolerance_db").unwrap_or(&0.5);
            Box::new(AudioStatsSink::new(expected_rms, tolerance_db)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_sink(node: &mut AudioStatsSink, signal: &[f32]) {
        node.prepare("test", 48000, signal.len());
        let slice: &[f32] = signal;
        node.process(&[slice], &mut [], signal.len());
    }

    #[test]
    fn dc_signal_stats() {
        // DC of 0.5: rms == 0.5, peak == 0.5, mean == 0.5
        let signal: Vec<f32> = vec![0.5f32; 1024];
        let mut node = AudioStatsSink::new(f64::NAN, 0.5);
        run_sink(&mut node, &signal);

        let rms = (node.sum_sq / node.count as f64).sqrt();
        let mean = node.sum / node.count as f64;
        assert!((rms - 0.5).abs() < 1e-6, "rms={rms}");
        assert!((node.peak - 0.5).abs() < 1e-6, "peak={}", node.peak);
        assert!((mean - 0.5).abs() < 1e-6, "mean={mean}");
    }

    #[test]
    fn pass_when_expected_matches() {
        let signal: Vec<f32> = vec![0.5f32; 1024];
        let mut node = AudioStatsSink::new(0.5, 0.5);
        run_sink(&mut node, &signal);
        assert!(
            node.finish().is_ok(),
            "should PASS when rms matches expected"
        );
    }

    #[test]
    fn fail_when_expected_off() {
        let signal: Vec<f32> = vec![0.5f32; 1024];
        let mut node = AudioStatsSink::new(0.1, 0.5); // expected 0.1, actual 0.5 — big diff
        run_sink(&mut node, &signal);
        assert!(
            node.finish().is_err(),
            "should FAIL when rms doesn't match expected"
        );
    }

    #[test]
    fn nan_expected_rms_always_ok() {
        let signal: Vec<f32> = vec![0.9f32; 1024];
        let mut node = AudioStatsSink::new(f64::NAN, 0.5);
        run_sink(&mut node, &signal);
        assert!(node.finish().is_ok(), "NaN expected_rms should always pass");
    }
}
