use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

/// Multiplies an audio signal by a configurable gain. Supports either `gain_db` (dB,
/// default 0.0) or `gain_linear` (raw multiplier). When `gain_linear` is finite it
/// takes precedence. Unity gain by default. Used by the pitch×SNR sweep to scale
/// noise relative to signal RMS without SNR math leaking into world files.
pub struct GainNode {
    gain: f32,
}

impl Node for GainNode {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.is_empty() {
            return;
        }
        for (out, &inp) in outputs[0][..nframes]
            .iter_mut()
            .zip(inputs[0][..nframes].iter())
        {
            *out = inp * self.gain;
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "GainNode",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![
            ParamSpec {
                name: "gain_db",
                default: 0.0,
                min: -120.0,
                max: 60.0,
                unit: "dB",
            },
            ParamSpec {
                name: "gain_linear",
                default: f64::NAN,
                min: 0.0,
                max: 1e6,
                unit: "",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let linear = *params.get("gain_linear").unwrap_or(&f64::NAN);
            let gain_db = *params.get("gain_db").unwrap_or(&0.0);
            let gain = if linear.is_finite() {
                linear as f32
            } else {
                10.0f32.powf((gain_db as f32) / 20.0)
            };
            Box::new(GainNode { gain }) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(params: &[(&str, f64)]) -> GainNode {
        let map: HashMap<String, f64> = params.iter().map(|&(k, v)| (k.to_string(), v)).collect();
        let linear = *map.get("gain_linear").unwrap_or(&f64::NAN);
        let gain_db = *map.get("gain_db").unwrap_or(&0.0);
        let gain = if linear.is_finite() {
            linear as f32
        } else {
            10.0f32.powf((gain_db as f32) / 20.0)
        };
        GainNode { gain }
    }

    fn run(node: &mut GainNode, input: &[f32]) -> Vec<f32> {
        let nframes = input.len();
        let mut output = vec![0.0f32; nframes];
        {
            let inp: &[f32] = input;
            let out: &mut [f32] = &mut output;
            node.process(&[inp], &mut [out], nframes);
        }
        output
    }

    #[test]
    fn unity_gain_passes_through() {
        let mut node = make_node(&[]);
        let input: Vec<f32> = (0..512)
            .map(|i| if i % 2 == 0 { 0.3f32 } else { -0.3f32 })
            .collect();
        let output = run(&mut node, &input);
        for (i, (&expected, &got)) in input.iter().zip(output.iter()).enumerate() {
            assert!(
                (expected - got).abs() < 1e-6,
                "sample {i}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn six_db_doubles_amplitude() {
        let mut node = make_node(&[("gain_db", 6.0206)]);
        let input: Vec<f32> = (0..512)
            .map(|i| if i % 2 == 0 { 0.3f32 } else { -0.3f32 })
            .collect();
        let output = run(&mut node, &input);
        for (i, (&inp, &got)) in input.iter().zip(output.iter()).enumerate() {
            let expected = inp * 2.0;
            assert!(
                (expected - got).abs() < 0.001,
                "sample {i}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn minus_six_db_halves_amplitude() {
        let mut node = make_node(&[("gain_db", -6.0206)]);
        let input: Vec<f32> = (0..512)
            .map(|i| if i % 2 == 0 { 0.3f32 } else { -0.3f32 })
            .collect();
        let output = run(&mut node, &input);
        for (i, (&inp, &got)) in input.iter().zip(output.iter()).enumerate() {
            let expected = inp * 0.5;
            assert!(
                (expected - got).abs() < 0.001,
                "sample {i}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn gain_linear_overrides_db() {
        let mut node = make_node(&[("gain_db", 12.0), ("gain_linear", 0.5)]);
        let input = vec![0.5f32; 512];
        let output = run(&mut node, &input);
        for (i, &got) in output.iter().enumerate() {
            let expected = 0.25f32;
            assert!(
                (expected - got).abs() < 1e-6,
                "sample {i}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn gain_linear_zero_silences() {
        let mut node = make_node(&[("gain_linear", 0.0)]);
        let input = vec![0.5f32; 512];
        let output = run(&mut node, &input);
        for (i, &got) in output.iter().enumerate() {
            assert_eq!(got, 0.0f32, "sample {i}: expected 0.0, got {got}");
        }
    }

    #[test]
    fn freq_param_unit_checks() {
        let mut registry = NodeRegistry::new();
        register(&mut registry);
        let params = registry.parameters("GainNode").unwrap();
        let gain_db_param = params.iter().find(|p| p.name == "gain_db").unwrap();
        assert_eq!(gain_db_param.unit, "dB");
    }
}
