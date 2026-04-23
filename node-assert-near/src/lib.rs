use engine::{Node, NodeError, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

/// Assertion node: compares the last sample seen on its `value` Control input against an
/// `expected` parameter and returns `Err` from `finish()` if the difference exceeds `tolerance`.
///
/// Parameters:
///   expected      — the target value (required, no sensible default; set to 0.0 by convention)
///   tolerance_db  — tolerance in dB (default 0.5 dB, mode "db" only)
///   mode          — "db" (default) or "abs"; controls how tolerance is interpreted
///
/// For block-rate held signals (e.g. from AudioStats), the last sample of the last processed
/// block is compared. This matches the zero-order-hold output convention of Control ports.
pub struct AssertNear {
    id: String,
    expected: f64,
    tolerance_db: f64,
    mode: Mode,
    last_value: f32,
}

#[derive(Clone, Copy)]
enum Mode {
    Db,
    Abs,
}

impl AssertNear {
    fn new(expected: f64, tolerance_db: f64, mode: Mode) -> Self {
        Self {
            id: String::new(),
            expected,
            tolerance_db,
            mode,
            last_value: 0.0,
        }
    }
}

impl Node for AssertNear {
    fn prepare(&mut self, id: &str, _sample_rate: u32, _block_size: usize) {
        self.id = id.to_string();
        self.last_value = 0.0;
    }

    fn process(&mut self, inputs: &[&[f32]], _outputs: &mut [&mut [f32]], nframes: usize) {
        if let Some(input) = inputs.first()
            && nframes > 0
        {
            self.last_value = input[nframes - 1];
        }
    }

    fn finish(&mut self) -> Result<(), NodeError> {
        let actual = self.last_value as f64;
        let expected = self.expected;

        let within = match self.mode {
            Mode::Db => {
                // Avoid log(0) — if expected is 0 fall back to abs comparison.
                if expected == 0.0 {
                    actual.abs() <= self.tolerance_db
                } else {
                    let ratio = actual / expected;
                    if ratio <= 0.0 {
                        false
                    } else {
                        let diff_db = (20.0 * ratio.log10()).abs();
                        diff_db <= self.tolerance_db
                    }
                }
            }
            Mode::Abs => (actual - expected).abs() <= self.tolerance_db,
        };

        if within {
            Ok(())
        } else {
            let mode_label = match self.mode {
                Mode::Db => "dB",
                Mode::Abs => "abs",
            };
            Err(NodeError(format!(
                "assert-near[{}]: FAIL (expected={:.6}, actual={:.6}, tolerance={}{mode_label})",
                self.id, expected, actual, self.tolerance_db
            )))
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "AssertNear",
        vec![PortSpec {
            name: "value",
            ty: PortType::Control,
        }],
        vec![],
        vec![
            ParamSpec {
                name: "expected",
                default: 0.0,
                min: f64::NEG_INFINITY,
                max: f64::INFINITY,
                unit: "",
            },
            ParamSpec {
                name: "tolerance_db",
                default: 0.5,
                min: 0.0,
                max: 120.0,
                unit: "dB",
            },
            ParamSpec {
                name: "mode",
                default: 0.0, // 0.0 = "db", 1.0 = "abs"
                min: 0.0,
                max: 1.0,
                unit: "",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let expected = *params.get("expected").unwrap_or(&0.0);
            let tolerance_db = *params.get("tolerance_db").unwrap_or(&0.5);
            let mode_raw = *params.get("mode").unwrap_or(&0.0);
            let mode = if mode_raw >= 0.5 { Mode::Abs } else { Mode::Db };
            Box::new(AssertNear::new(expected, tolerance_db, mode)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_assert(node: &mut AssertNear, value: f32) -> Result<(), NodeError> {
        node.prepare("test", 48000, 256);
        // Simulate a block-rate held signal: fill a block with a constant.
        let buf = vec![value; 256];
        node.process(&[&buf], &mut [], 256);
        node.finish()
    }

    #[test]
    fn pass_when_expected_matches() {
        let mut node = AssertNear::new(0.5, 0.5, Mode::Db);
        assert!(run_assert(&mut node, 0.5).is_ok());
    }

    #[test]
    fn fail_when_expected_off() {
        // 0.5 vs 0.1 is ~14 dB difference, well outside 0.5 dB tolerance.
        let mut node = AssertNear::new(0.1, 0.5, Mode::Db);
        assert!(run_assert(&mut node, 0.5).is_err());
    }

    #[test]
    fn pass_abs_mode() {
        let mut node = AssertNear::new(0.5, 0.01, Mode::Abs);
        assert!(run_assert(&mut node, 0.505).is_ok());
    }

    #[test]
    fn fail_abs_mode() {
        let mut node = AssertNear::new(0.5, 0.01, Mode::Abs);
        assert!(run_assert(&mut node, 0.6).is_err());
    }

    #[test]
    fn pass_when_within_db_tolerance() {
        // 0.3536 vs 0.3536 — exact match.
        let mut node = AssertNear::new(0.3536, 0.5, Mode::Db);
        assert!(run_assert(&mut node, 0.3536).is_ok());
    }

    #[test]
    fn error_message_contains_ids() {
        let mut node = AssertNear::new(0.1, 0.5, Mode::Db);
        let err = run_assert(&mut node, 0.5).unwrap_err();
        assert!(err.0.contains("test"), "error should contain node id");
        assert!(
            err.0.contains("expected"),
            "error should contain 'expected'"
        );
    }
}
