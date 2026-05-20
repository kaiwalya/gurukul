use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct MixSum;

impl Node for MixSum {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if outputs.is_empty() {
            return;
        }
        for (i, out) in outputs[0][..nframes].iter_mut().enumerate() {
            *out = inputs
                .iter()
                .map(|ch| ch.get(i).copied().unwrap_or(0.0))
                .sum();
        }
    }
}

/// Build input port specs for the given channel count. Port names are leaked to produce
/// `&'static str`; since channel counts are bounded [2, 16] the leak budget is bounded.
fn input_ports(channels: usize) -> Vec<PortSpec> {
    (0..channels)
        .map(|i| {
            let name: &'static str = Box::leak(format!("in_{i}").into_boxed_str());
            PortSpec {
                name,
                ty: PortType::Audio,
            }
        })
        .collect()
}

fn output_ports() -> Vec<PortSpec> {
    vec![PortSpec {
        name: "out",
        ty: PortType::Audio,
    }]
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full_variadic(
        "MixSum",
        input_ports(2),
        output_ports(),
        vec![ParamSpec {
            name: "channels",
            default: 2.0,
            min: 2.0,
            max: 16.0,
            unit: "count",
        }],
        Box::new(|params: &HashMap<String, f64>| {
            let channels = params
                .get("channels")
                .copied()
                .unwrap_or(2.0)
                .clamp(2.0, 16.0) as usize;
            (input_ports(channels), output_ports())
        }),
        Box::new(|_params: &HashMap<String, f64>| Box::new(MixSum) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_two_inputs() {
        let mut node = MixSum;
        node.prepare("test", 48000, 3);

        let a = [1.0f32, 2.0, 3.0];
        let b = [4.0f32, 5.0, 6.0];
        let mut out = [0.0f32; 3];

        let a_slice: &[f32] = &a;
        let b_slice: &[f32] = &b;
        let mut out_slice: &mut [f32] = &mut out;
        node.process(&[a_slice, b_slice], &mut [&mut out_slice], 3);

        assert_eq!(out, [5.0, 7.0, 9.0]);
    }

    #[test]
    fn sums_three_inputs() {
        let mut node = MixSum;
        node.prepare("test", 48000, 3);

        let a = [1.0f32, 1.0, 1.0];
        let b = [2.0f32, 2.0, 2.0];
        let c = [3.0f32, 3.0, 3.0];
        let mut out = [0.0f32; 3];

        let mut out_slice: &mut [f32] = &mut out;
        node.process(&[&a, &b, &c], &mut [&mut out_slice], 3);

        assert_eq!(out, [6.0, 6.0, 6.0]);
    }

    #[test]
    fn missing_inputs_treated_as_silence() {
        let mut node = MixSum;
        node.prepare("test", 48000, 3);

        // No inputs at all → output is zeros.
        let mut out = [1.0f32; 3];
        {
            let mut out_slice: &mut [f32] = &mut out;
            node.process(&[], &mut [&mut out_slice], 3);
        }
        assert_eq!(out, [0.0, 0.0, 0.0]);

        // Only one input → remaining channels are silence.
        let a = [2.0f32, 3.0, 4.0];
        let mut out2 = [0.0f32; 3];
        {
            let a_slice: &[f32] = &a;
            let mut out_slice: &mut [f32] = &mut out2;
            node.process(&[a_slice], &mut [&mut out_slice], 3);
        }
        assert_eq!(out2, [2.0, 3.0, 4.0]);
    }
}
