//! Vibrato × (rate, depth) sweep: Tier-1 oracle for the vibrato analyzer.
//!
//! Wires SynthVibratoSine → PitchYin → Vibrato across a grid of vibrato
//! `(rate, depth)` pairs and asserts the analyzer recovers each pair within
//! tolerance. End-to-end DSP — synth produces audio, YIN extracts f0, the
//! analyzer recovers `(rate, depth)`.
//!
//! Tolerance:
//!   - rate within ±0.7 Hz (≈ 14% at 5 Hz; vibrato is wobbly even in
//!     synthetic signals because YIN's analysis hop quantises the f0 update).
//!   - depth within ±25 cents (correctness-first analyzer; future PR can
//!     tighten this with a least-squares fit instead of peak-to-peak range).

use engine::{BoundaryPort, Connection, Engine, NodeDef, NodeRegistry, World};
use std::collections::HashMap;

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
// 3 seconds — long enough for the 1.5 s vibrato analysis window to fill plus
// several analysis hops worth of stable output.
const N_BLOCKS: u64 = 281;

const RATES_HZ: &[f32] = &[4.0, 5.0, 6.0, 7.0];
const DEPTHS_CENTS: &[f32] = &[30.0, 50.0, 80.0];

struct CellResult {
    carrier_hz: f32,
    rate_hz: f32,
    depth_cents: f32,
    est_rate_hz: f32,
    est_depth_cents: f32,
    pass: bool,
}

fn run_cell(carrier_hz: f32, rate_hz: f32, depth_cents: f32) -> CellResult {
    let mut registry = NodeRegistry::new();
    node_synth_vibrato_sine::register(&mut registry);
    node_pitch_yin::register(&mut registry);
    node_vibrato::register(&mut registry);

    let synth_params: HashMap<String, f64> = [
        ("carrier_freq".to_string(), carrier_hz as f64),
        ("amplitude".to_string(), 0.5),
        ("vibrato_rate".to_string(), rate_hz as f64),
        ("vibrato_depth_cents".to_string(), depth_cents as f64),
    ]
    .into();

    let world = World {
        schema: None,
        world_version: 1,
        in_ports: vec![],
        out_ports: vec![
            BoundaryPort {
                id: "rate_out".to_string(),
                name: None,
                description: None,
            },
            BoundaryPort {
                id: "depth_out".to_string(),
                name: None,
                description: None,
            },
        ],
        nodes: vec![
            NodeDef {
                id: "synth".to_string(),
                ty: "SynthVibratoSine".to_string(),
                params: synth_params,
                name: None,
                description: None,
            },
            NodeDef {
                id: "yin".to_string(),
                ty: "PitchYin".to_string(),
                params: HashMap::new(),
                name: None,
                description: None,
            },
            NodeDef {
                id: "vib".to_string(),
                ty: "Vibrato".to_string(),
                params: HashMap::new(),
                name: None,
                description: None,
            },
        ],
        connections: vec![
            Connection {
                from: "synth.audio_out".to_string(),
                to: "yin.audio_in".to_string(),
            },
            Connection {
                from: "yin.f0".to_string(),
                to: "vib.f0".to_string(),
            },
            Connection {
                from: "vib.rate".to_string(),
                to: "rate_out".to_string(),
            },
            Connection {
                from: "vib.amplitude".to_string(),
                to: "depth_out".to_string(),
            },
        ],
    };

    let mut engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("engine build");

    let h_rate = engine
        .resolve_out_port("rate_out")
        .expect("resolve rate_out");
    let h_depth = engine
        .resolve_out_port("depth_out")
        .expect("resolve depth_out");

    engine.run_blocks(N_BLOCKS);

    let est_rate_hz = *engine.out_port(h_rate).last().unwrap();
    let est_depth_cents = *engine.out_port(h_depth).last().unwrap();

    let pass =
        (est_rate_hz - rate_hz).abs() <= 0.7 && (est_depth_cents - depth_cents).abs() <= 25.0;

    CellResult {
        carrier_hz,
        rate_hz,
        depth_cents,
        est_rate_hz,
        est_depth_cents,
        pass,
    }
}

#[test]
fn vibrato_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let mut results: Vec<CellResult> = Vec::new();
    for &rate in RATES_HZ {
        for &depth in DEPTHS_CENTS {
            // Fixed carrier; sweep is over (rate, depth).
            let r = run_cell(440.0, rate, depth);
            results.push(r);
        }
    }

    // --- CSV artifact -------------------------------------------------------
    let csv_path = format!("{tmpdir}/vibrato_sweep.csv");
    let mut csv = String::from("carrier_hz,rate_hz,depth_cents,est_rate_hz,est_depth_cents,pass\n");
    for r in &results {
        csv.push_str(&format!(
            "{:.1},{:.2},{:.1},{:.3},{:.1},{}\n",
            r.carrier_hz, r.rate_hz, r.depth_cents, r.est_rate_hz, r.est_depth_cents, r.pass
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    // --- Grid text artifact -------------------------------------------------
    let mut grid = String::from("rate↓ depth→ |");
    for d in DEPTHS_CENTS {
        grid.push_str(&format!(" {d:>5.0}c |"));
    }
    grid.push('\n');
    for &rate in RATES_HZ {
        grid.push_str(&format!("       {rate:>4.1} |"));
        for &depth in DEPTHS_CENTS {
            let r = results
                .iter()
                .find(|r| r.rate_hz == rate && r.depth_cents == depth)
                .unwrap();
            let mark = if r.pass { "  ok  " } else { " FAIL " };
            grid.push_str(&format!(" {mark} |"));
        }
        grid.push('\n');
    }
    let grid_path = format!("{tmpdir}/vibrato_sweep_grid.txt");
    std::fs::write(&grid_path, &grid).expect("writing grid artifact");
    println!("Grid written to: {grid_path}");
    println!("\n{grid}");

    let failures: Vec<&CellResult> = results.iter().filter(|r| !r.pass).collect();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!(
                "FAIL rate={:.2} depth={:.1} → est_rate={:.3} est_depth={:.1}",
                f.rate_hz, f.depth_cents, f.est_rate_hz, f.est_depth_cents
            );
        }
        panic!("{} cells failed", failures.len());
    }
}
