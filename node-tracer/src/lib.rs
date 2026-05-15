//! Pass-through node that prints its first input sample per block to stderr.
//!
//! Tracer is a debug/inspection affordance, not a subscription API. The CLI
//! splices it onto a port at runtime and consults a side-table to render a
//! human-readable legend mapping each tracer id back to its observed port.
//! When the real subscription API (`ARCHITECTURE.md` § "Port addressing and
//! subscription") lands in Phase 1.4, Tracer can go away.

use engine::{Node, NodeRegistry, PortSpec, PortType};
use std::collections::HashMap;

pub struct Tracer {
    /// The node id assigned by the world. Used as the label in stderr output;
    /// no information is encoded in it.
    label: String,
    samples_seen: u64,
    block_size: usize,
}

impl Tracer {
    fn new() -> Self {
        Self {
            label: String::new(),
            samples_seen: 0,
            block_size: 512,
        }
    }
}

impl Node for Tracer {
    fn prepare(&mut self, id: &str, _sample_rate: u32, block_size: usize) {
        self.label = id.to_string();
        self.block_size = block_size;
        self.samples_seen = 0;
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if !inputs.is_empty() && !outputs.is_empty() {
            outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
        }
        let first = inputs
            .first()
            .and_then(|s| s.first())
            .copied()
            .unwrap_or(0.0);
        eprintln!("{}\t{}\t{:.6}", self.samples_seen, self.label, first);
        self.samples_seen += self.block_size as u64;
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "Tracer",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![],
        Box::new(|_params: &HashMap<String, f64>| Box::new(Tracer::new()) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracer_passes_audio_through() {
        let mut node = Tracer::new();
        node.prepare("trace_0", 48000, 4);

        let input = vec![1.0f32, 2.0, 3.0, 4.0];
        let mut output = vec![0.0f32; 4];
        let input_slice: &[f32] = &input;
        let mut output_slice: &mut [f32] = &mut output;
        node.process(&[input_slice], &mut [&mut output_slice], 4);

        assert_eq!(output, input);
    }

    #[test]
    fn tracer_uses_id_as_label() {
        let mut node = Tracer::new();
        node.prepare("trace_42", 48000, 512);
        assert_eq!(node.label, "trace_42");
    }

    #[test]
    fn tracer_increments_sample_counter() {
        let mut node = Tracer::new();
        node.prepare("trace_0", 48000, 512);
        assert_eq!(node.samples_seen, 0);

        let input = vec![0.0f32; 512];
        let mut output = vec![0.0f32; 512];
        let input_slice: &[f32] = &input;
        let mut output_slice: &mut [f32] = &mut output;
        node.process(&[input_slice], &mut [&mut output_slice], 512);
        assert_eq!(node.samples_seen, 512);
    }
}
