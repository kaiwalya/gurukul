use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct Passthrough;

impl Node for Passthrough {
    fn declare_ports(&self) -> (Vec<PortSpec>, Vec<PortSpec>) {
        (
            vec![PortSpec {
                name: "audio_in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "audio_out",
                ty: PortType::Audio,
            }],
        )
    }

    fn declare_parameters(&self) -> Vec<ParamSpec> {
        vec![]
    }

    fn prepare(&mut self, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.is_empty() {
            return;
        }
        outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register(
        "Passthrough",
        Box::new(|_params: &HashMap<String, f64>| Box::new(Passthrough) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_output_equals_input() {
        let mut node = Passthrough;
        node.prepare(48000, 512);

        let input: Vec<f32> = (0..512).map(|i| i as f32 * 0.001).collect();
        let mut output = vec![0.0f32; 512];

        let input_slice: &[f32] = &input;
        let mut output_slice: &mut [f32] = &mut output;

        node.process(&[input_slice], &mut [&mut output_slice], 512);

        for (i, (&expected, &got)) in input.iter().zip(output.iter()).enumerate() {
            assert_eq!(expected, got, "sample {i} mismatch");
        }
    }
}
