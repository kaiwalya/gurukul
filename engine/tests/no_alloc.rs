//! Verifies that `Engine::run_blocks` performs zero heap allocations on the hot path.
//!
//! The global allocator is replaced with `AllocDisabler` for this test binary; any
//! allocation inside `assert_no_alloc(|| { ... })` aborts the process.
//!
//! Covers the engine harness, not the nodes — node-level guarantees are tested
//! per-crate (e.g. `node-pitch-yin/tests/no_alloc.rs`). Together these tests
//! enforce the realtime-discipline rule in `docs/ARCHITECTURE.md`.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use engine::{Connection, Engine, Node, NodeDef, NodeRegistry, PortSpec, PortType, World};
use std::collections::HashMap;

/// Trivial source: fills its single output with a constant. No allocations.
struct ConstSource;
impl Node for ConstSource {
    fn prepare(&mut self, _: &str, _: u32, _: usize) {}
    fn process(&mut self, _: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        outputs[0][..nframes].fill(0.1);
    }
}

/// Trivial inline node: copies input → output. Variadic-free; static ports.
struct Copy;
impl Node for Copy {
    fn prepare(&mut self, _: &str, _: u32, _: usize) {}
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
    }
}

/// Sink: reads input, does nothing observable.
struct Sink;
impl Node for Sink {
    fn prepare(&mut self, _: &str, _: u32, _: usize) {}
    fn process(&mut self, inputs: &[&[f32]], _: &mut [&mut [f32]], _: usize) {
        // Touch the input so the compiler can't optimise the read away.
        std::hint::black_box(inputs[0].len());
    }
}

fn build_registry() -> NodeRegistry {
    let mut reg = NodeRegistry::new();
    reg.register_full(
        "ConstSource",
        vec![],
        vec![PortSpec {
            name: "out",
            ty: PortType::Audio,
        }],
        vec![],
        Box::new(|_| Box::new(ConstSource) as Box<dyn Node>),
    );
    reg.register_full(
        "Copy",
        vec![PortSpec {
            name: "in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "out",
            ty: PortType::Audio,
        }],
        vec![],
        Box::new(|_| Box::new(Copy) as Box<dyn Node>),
    );
    reg.register_full(
        "Sink",
        vec![PortSpec {
            name: "in",
            ty: PortType::Audio,
        }],
        vec![],
        vec![],
        Box::new(|_| Box::new(Sink) as Box<dyn Node>),
    );
    reg
}

#[test]
fn run_blocks_does_not_allocate() {
    let world = World {
        schema: None,
        nodes: vec![
            NodeDef {
                id: "src".to_string(),
                ty: "ConstSource".to_string(),
                params: HashMap::new(),
            },
            NodeDef {
                id: "mid".to_string(),
                ty: "Copy".to_string(),
                params: HashMap::new(),
            },
            NodeDef {
                id: "snk".to_string(),
                ty: "Sink".to_string(),
                params: HashMap::new(),
            },
        ],
        connections: vec![
            Connection {
                from: "src.out".to_string(),
                to: "mid.in".to_string(),
            },
            Connection {
                from: "mid.out".to_string(),
                to: "snk.in".to_string(),
            },
        ],
    };

    let registry = build_registry();
    let mut engine = Engine::build(&world, &registry, 48000, 512).expect("build");

    // Warm-up block outside the no-alloc region: nothing in run_blocks should allocate
    // even on first call, but constructing the closure environment for assert_no_alloc
    // can have one-time setup costs we don't care about.
    engine.run_blocks(1);

    assert_no_alloc(|| {
        engine.run_blocks(25);
    });
}
