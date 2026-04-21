use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct Tracer {
    // Decoded original port path recovered from the node id in prepare().
    // Format of node id: __trace__{node_id}__{port_name}__{counter}
    // Assumption: user-chosen node ids and port names do not contain double underscores.
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

    /// Decode a tracer node id back to the original port path.
    ///
    /// Input format: `__trace__{node_id}__{port_name}__{counter}`
    /// Steps: strip `__trace__` prefix, strip trailing `__{digits}` suffix,
    /// replace the first `__` with `.`.
    fn decode_id(id: &str) -> String {
        let body = id.strip_prefix("__trace__").unwrap_or(id);
        // Strip trailing `__{digits}` suffix.
        let body = if let Some(pos) = body.rfind("__") {
            let suffix = &body[pos + 2..];
            if suffix.chars().all(|c| c.is_ascii_digit()) {
                &body[..pos]
            } else {
                body
            }
        } else {
            body
        };
        // Replace the first `__` with `.` to recover `node_id.port_name`.
        if let Some(pos) = body.find("__") {
            let mut s = body.to_string();
            s.replace_range(pos..pos + 2, ".");
            s
        } else {
            body.to_string()
        }
    }
}

impl Node for Tracer {
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

    fn prepare(&mut self, id: &str, _sample_rate: u32, block_size: usize) {
        self.label = Self::decode_id(id);
        self.block_size = block_size;
        self.samples_seen = 0;
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        // Pass audio through unchanged.
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
    registry.register(
        "Tracer",
        Box::new(|_params: &HashMap<String, f64>| Box::new(Tracer::new()) as Box<dyn Node>),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_id_round_trips() {
        assert_eq!(
            Tracer::decode_id("__trace__src__audio_out__0"),
            "src.audio_out"
        );
        assert_eq!(
            Tracer::decode_id("__trace__my_node__audio_out__1"),
            "my_node.audio_out"
        );
        assert_eq!(
            Tracer::decode_id("__trace__src__audio_out__42"),
            "src.audio_out"
        );
    }

    #[test]
    fn tracer_passes_audio_through() {
        let mut node = Tracer::new();
        node.prepare("__trace__src__audio_out__0", 48000, 4);

        let input = vec![1.0f32, 2.0, 3.0, 4.0];
        let mut output = vec![0.0f32; 4];
        let input_slice: &[f32] = &input;
        let mut output_slice: &mut [f32] = &mut output;
        node.process(&[input_slice], &mut [&mut output_slice], 4);

        assert_eq!(output, input);
    }

    #[test]
    fn tracer_increments_sample_counter() {
        let mut node = Tracer::new();
        node.prepare("__trace__src__audio_out__0", 48000, 512);
        assert_eq!(node.samples_seen, 0);

        let input = vec![0.0f32; 512];
        let mut output = vec![0.0f32; 512];
        let input_slice: &[f32] = &input;
        let mut output_slice: &mut [f32] = &mut output;
        node.process(&[input_slice], &mut [&mut output_slice], 512);
        assert_eq!(node.samples_seen, 512);
    }
}
