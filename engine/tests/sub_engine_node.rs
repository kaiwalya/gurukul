//! SubEngineNode conformance test.
//!
//! Proves that the engine's faceplate is "node-shaped": a struct wrapping a child
//! Engine can implement `Node` and produce sample-exact output compared to running
//! the inner world directly. This is the seam's conformance guard — any future
//! change that breaks node-shape equivalence fails this test.

use engine::{
    BoundaryPort, BoundaryPortSpec, Connection, Engine, EngineError, InPortHandle, Node, NodeDef,
    NodeRegistry, OutPortHandle, PortSpec, World,
};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// SubEngineNode: wraps an Engine and exposes its boundary ports as Node ports.
// ---------------------------------------------------------------------------

struct SubEngineNode {
    inner: Engine,
    in_handles: Vec<InPortHandle>,
    out_handles: Vec<OutPortHandle>,
    // Mirrors of the boundary port types, kept for future use (e.g. input_ports/output_ports).
    #[allow(dead_code)]
    in_specs: Vec<PortSpec>,
    #[allow(dead_code)]
    out_specs: Vec<PortSpec>,
}

impl SubEngineNode {
    fn new(
        world: &World,
        registry: &NodeRegistry,
        sample_rate: u32,
        block_size: usize,
    ) -> Result<Self, EngineError> {
        let inner = Engine::build(world, registry, sample_rate, block_size)?;

        // Resolve handles for every boundary port.
        let in_handles: Vec<InPortHandle> = inner
            .in_port_specs()
            .iter()
            .map(|s| inner.resolve_in_port(&s.id))
            .collect::<Result<_, _>>()?;

        let out_handles: Vec<OutPortHandle> = inner
            .out_port_specs()
            .iter()
            .map(|s| inner.resolve_out_port(&s.id))
            .collect::<Result<_, _>>()?;

        // Build static PortSpec mirrors for Node::input_ports / output_ports.
        let in_specs: Vec<PortSpec> = inner.in_port_specs().iter().map(to_port_spec).collect();
        let out_specs: Vec<PortSpec> = inner.out_port_specs().iter().map(to_port_spec).collect();

        Ok(SubEngineNode {
            inner,
            in_handles,
            out_handles,
            in_specs,
            out_specs,
        })
    }
}

fn to_port_spec(s: &BoundaryPortSpec) -> PortSpec {
    PortSpec {
        name: "port", // static str — only used for type matching in this test
        ty: s.ty.clone(),
    }
}

impl Node for SubEngineNode {
    fn prepare(&mut self, _id: &str, sample_rate: u32, block_size: usize) {
        // The inner engine is built with parent SR/block_size at construction.
        // If the outer engine later calls prepare() with different values that is
        // a programmer error: the inner world was not built for those parameters.
        debug_assert_eq!(
            sample_rate,
            self.inner.sample_rate(),
            "SubEngineNode SR mismatch: inner engine was built with {}, outer engine prepared with {}",
            self.inner.sample_rate(),
            sample_rate,
        );
        debug_assert_eq!(
            block_size,
            self.inner.block_size(),
            "SubEngineNode block_size mismatch: inner engine was built with {}, outer engine prepared with {}",
            self.inner.block_size(),
            block_size,
        );
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        // Stage: copy each caller-supplied input slice into the inner in-port buffer.
        for (i, &h) in self.in_handles.iter().enumerate() {
            if let Some(input) = inputs.get(i) {
                self.inner.in_port(h)[..nframes].copy_from_slice(&input[..nframes]);
            }
        }

        // Run one block.
        self.inner.process_block(nframes);

        // Capture: copy each inner out-port buffer into the caller-supplied output slice.
        for (i, &h) in self.out_handles.iter().enumerate() {
            if let Some(output) = outputs.get_mut(i) {
                output[..nframes].copy_from_slice(&self.inner.out_port(h)[..nframes]);
            }
        }
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

// ---------------------------------------------------------------------------
// Node registry helpers
// ---------------------------------------------------------------------------

fn build_test_registry() -> NodeRegistry {
    let mut reg = NodeRegistry::new();
    node_synth_sine::register(&mut reg);
    node_gain::register(&mut reg);
    node_passthrough::register(&mut reg);
    reg
}

// ---------------------------------------------------------------------------
// The inner world:
//   - in_port "x" → passthrough → out_port "x_passthrough"
//   - synth_sine → gain → out_port "y"
// ---------------------------------------------------------------------------

fn inner_world() -> World {
    let gain_params: HashMap<String, f64> = [("gain_linear".to_string(), 0.5)].into();
    World {
        schema: None,
        world_version: 1,
        in_ports: vec![BoundaryPort {
            id: "x".to_string(),
            name: None,
            description: None,
        }],
        out_ports: vec![
            BoundaryPort {
                id: "y".to_string(),
                name: None,
                description: None,
            },
            BoundaryPort {
                id: "x_passthrough".to_string(),
                name: None,
                description: None,
            },
        ],
        nodes: vec![
            NodeDef {
                id: "synth".to_string(),
                ty: "SynthSine".to_string(),
                params: [("freq".to_string(), 440.0), ("amplitude".to_string(), 0.8)].into(),
                name: None,
                description: None,
            },
            NodeDef {
                id: "gain".to_string(),
                ty: "GainNode".to_string(),
                params: gain_params,
                name: None,
                description: None,
            },
            NodeDef {
                id: "pt".to_string(),
                ty: "Passthrough".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            },
        ],
        connections: vec![
            // synth → gain → out_port "y"
            Connection {
                from: "synth.audio_out".to_string(),
                to: "gain.audio_in".to_string(),
            },
            Connection {
                from: "gain.audio_out".to_string(),
                to: "y".to_string(),
            },
            // in_port "x" → passthrough → out_port "x_passthrough"
            Connection {
                from: "x".to_string(),
                to: "pt.audio_in".to_string(),
            },
            Connection {
                from: "pt.audio_out".to_string(),
                to: "x_passthrough".to_string(),
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Tier-1 test: SubEngineNode produces byte-exact output vs direct inner engine.
// ---------------------------------------------------------------------------

#[test]
fn sub_engine_node_matches_direct_engine() {
    const SAMPLE_RATE: u32 = 48000;
    const BLOCK_SIZE: usize = 512;
    const N_BLOCKS: usize = 8;

    let registry = build_test_registry();
    let world = inner_world();

    // --- Reference: run the inner world directly ---
    let mut direct_engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("direct build");
    let h_in_direct = direct_engine.resolve_in_port("x").unwrap();
    let h_y_direct = direct_engine.resolve_out_port("y").unwrap();
    let h_xpt_direct = direct_engine.resolve_out_port("x_passthrough").unwrap();

    // Pre-compute an "x" signal: linear ramp, repeated per block.
    let x_signal: Vec<f32> = (0..BLOCK_SIZE)
        .map(|i| (i as f32) / BLOCK_SIZE as f32)
        .collect();

    let mut direct_y: Vec<Vec<f32>> = Vec::new();
    let mut direct_xpt: Vec<Vec<f32>> = Vec::new();

    for _ in 0..N_BLOCKS {
        direct_engine
            .in_port(h_in_direct)
            .copy_from_slice(&x_signal);
        direct_engine.process_block(BLOCK_SIZE);
        direct_y.push(direct_engine.out_port(h_y_direct).to_vec());
        direct_xpt.push(direct_engine.out_port(h_xpt_direct).to_vec());
    }

    // --- SubEngineNode: wrap a fresh inner world ---
    let mut sub =
        SubEngineNode::new(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("sub build");

    let mut out_y = vec![0.0f32; BLOCK_SIZE];
    let mut out_xpt = vec![0.0f32; BLOCK_SIZE];

    for block_idx in 0..N_BLOCKS {
        // process() takes inputs and outputs as slices of slices.
        let x_ref: &[f32] = &x_signal;
        let in_slices: Vec<&[f32]> = vec![x_ref];

        {
            let out_y_ref: &mut [f32] = &mut out_y;
            let out_xpt_ref: &mut [f32] = &mut out_xpt;
            let mut out_slices: Vec<&mut [f32]> = vec![out_y_ref, out_xpt_ref];
            sub.process(&in_slices, &mut out_slices, BLOCK_SIZE);
        }

        assert_eq!(
            out_y, direct_y[block_idx],
            "block {block_idx}: 'y' output diverged between direct and SubEngineNode"
        );
        assert_eq!(
            out_xpt, direct_xpt[block_idx],
            "block {block_idx}: 'x_passthrough' output diverged between direct and SubEngineNode"
        );
    }
}

// ---------------------------------------------------------------------------
// Verify that reset() on SubEngineNode resets the inner engine deterministically.
// ---------------------------------------------------------------------------

#[test]
fn sub_engine_node_reset_is_deterministic() {
    const SAMPLE_RATE: u32 = 48000;
    const BLOCK_SIZE: usize = 512;

    let registry = build_test_registry();
    let world = inner_world();

    let mut sub =
        SubEngineNode::new(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("sub build");

    let x_signal = vec![0.1f32; BLOCK_SIZE];

    // Run 4 blocks.
    let mut out_y_a = vec![0.0f32; BLOCK_SIZE];
    let mut out_xpt_a = vec![0.0f32; BLOCK_SIZE];
    for _ in 0..4 {
        let x_ref: &[f32] = &x_signal;
        let in_slices: Vec<&[f32]> = vec![x_ref];
        let mut out_slices: Vec<&mut [f32]> = vec![&mut out_y_a, &mut out_xpt_a];
        sub.process(&in_slices, &mut out_slices, BLOCK_SIZE);
    }
    let snapshot_y_after_4 = out_y_a.clone();

    // Reset and run 4 blocks again — must be identical.
    sub.reset();
    let mut out_y_b = vec![0.0f32; BLOCK_SIZE];
    let mut out_xpt_b = vec![0.0f32; BLOCK_SIZE];
    for _ in 0..4 {
        let x_ref: &[f32] = &x_signal;
        let in_slices: Vec<&[f32]> = vec![x_ref];
        let mut out_slices: Vec<&mut [f32]> = vec![&mut out_y_b, &mut out_xpt_b];
        sub.process(&in_slices, &mut out_slices, BLOCK_SIZE);
    }

    assert_eq!(
        snapshot_y_after_4, out_y_b,
        "reset() must restore determinism: 4 blocks post-reset must equal first 4 blocks"
    );
}
