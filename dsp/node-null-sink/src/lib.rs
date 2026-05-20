use engine::{Node, NodeRegistry, PortSpec, PortType};
use std::collections::HashMap;

pub struct NullSink;

impl Node for NullSink {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, _inputs: &[&[f32]], _outputs: &mut [&mut [f32]], _nframes: usize) {
        // Intentionally discards all input.
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "NullSink",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![],
        vec![],
        Box::new(|_params: &HashMap<String, f64>| Box::new(NullSink) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_sink_does_not_crash() {
        let mut node = NullSink;
        node.prepare("test", 48000, 512);

        // Varied inputs: zeros, ramp, large values
        let zeros = vec![0.0f32; 512];
        let ramp: Vec<f32> = (0..512).map(|i| i as f32).collect();
        let large = vec![1e6f32; 512];

        for input in [&zeros, &ramp, &large] {
            let slice: &[f32] = input.as_slice();
            node.process(&[slice], &mut [], 512);
        }
    }
}
