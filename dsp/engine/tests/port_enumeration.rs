//! Runtime port enumeration.
//!
//! PR 1.4.8.1 surface: `Engine::node_ids` and `Engine::out_port_names` let host
//! code walk the live graph without re-reading the source World JSON. The
//! debug pane in the Mac cabinet (PR 1.4.8.5) is the first user.

use engine::{BoundaryPort, Connection, Engine, NodeDef, NodeRegistry, World};
use std::collections::HashMap;

fn build_test_registry() -> NodeRegistry {
    let mut reg = NodeRegistry::new();
    node_synth_sine::register(&mut reg);
    node_gain::register(&mut reg);
    node_passthrough::register(&mut reg);
    reg
}

/// Two-node world: synth → gain → out-port "y".
/// Plus a passthrough on its own to make node enumeration non-trivial.
fn small_world() -> World {
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
                id: "x_through".to_string(),
                name: None,
                description: None,
            },
        ],
        nodes: vec![
            NodeDef {
                id: "synth".to_string(),
                ty: "SynthSine".to_string(),
                params: [("freq".to_string(), 440.0), ("amplitude".to_string(), 0.5)].into(),
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
            Connection {
                from: "synth.audio_out".to_string(),
                to: "gain.audio_in".to_string(),
            },
            Connection {
                from: "gain.audio_out".to_string(),
                to: "y".to_string(),
            },
            Connection {
                from: "x".to_string(),
                to: "pt.audio_in".to_string(),
            },
            Connection {
                from: "pt.audio_out".to_string(),
                to: "x_through".to_string(),
            },
        ],
    }
}

#[test]
fn node_ids_returns_all_nodes_in_topo_order() {
    let registry = build_test_registry();
    let world = small_world();
    let engine = Engine::build(&world, &registry, 48_000, 64).unwrap();

    let ids = engine.node_ids();
    // The three nodes from the world.
    let mut expected: Vec<&str> = vec!["synth", "gain", "pt"];
    let mut actual: Vec<&str> = ids.iter().map(String::as_str).collect();
    expected.sort();
    actual.sort();
    assert_eq!(
        actual, expected,
        "node_ids must list every node in the world"
    );

    // Topo order: synth must come before gain (data dependency). pt has no
    // dependency on the other two so its position is unconstrained — only the
    // synth→gain relationship matters here.
    let synth_pos = ids.iter().position(|s| s == "synth").unwrap();
    let gain_pos = ids.iter().position(|s| s == "gain").unwrap();
    assert!(
        synth_pos < gain_pos,
        "node_ids must respect topological order: synth before gain"
    );
}

#[test]
fn out_port_names_lists_declared_outputs() {
    let registry = build_test_registry();
    let world = small_world();
    let engine = Engine::build(&world, &registry, 48_000, 64).unwrap();

    // SynthSine declares one output: audio_out.
    let synth_outs = engine.out_port_names("synth").unwrap();
    assert_eq!(synth_outs, vec!["audio_out"]);

    // GainNode declares one output: audio_out.
    let gain_outs = engine.out_port_names("gain").unwrap();
    assert_eq!(gain_outs, vec!["audio_out"]);

    // Passthrough declares one output: audio_out.
    let pt_outs = engine.out_port_names("pt").unwrap();
    assert_eq!(pt_outs, vec!["audio_out"]);
}

#[test]
fn out_port_names_unknown_node_returns_node_not_found() {
    let registry = build_test_registry();
    let world = small_world();
    let engine = Engine::build(&world, &registry, 48_000, 64).unwrap();

    let err = engine.out_port_names("nope").unwrap_err();
    assert!(
        matches!(err, engine::EngineError::NodeNotFound(ref id) if id == "nope"),
        "expected NodeNotFound, got {err:?}"
    );
}

#[test]
fn out_port_names_empty_string_returns_node_not_found() {
    let registry = build_test_registry();
    let world = small_world();
    let engine = Engine::build(&world, &registry, 48_000, 64).unwrap();

    let err = engine.out_port_names("").unwrap_err();
    assert!(
        matches!(err, engine::EngineError::NodeNotFound(ref id) if id.is_empty()),
        "empty node id should hit the HashMap lookup branch cleanly, got {err:?}"
    );
}
