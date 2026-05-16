//! Onset × BPM sweep: Tier-1 oracle for the onset detector.
//!
//! Wires SynthOnsets → Onset across a grid of BPMs. For each cell:
//!   - count the detected onsets in a fixed duration,
//!   - verify the count matches the expected onset count (±1 boundary slack),
//!   - verify the mean inter-onset interval matches 60/bpm s within 5%.
//!
//! The detector's output is read via the boundary out-port API (`engine.out_port`)
//! rather than splicing a Tracer. This is the proof-of-life for the Phase 1.4
//! boundary port API.

use engine::{BoundaryPort, Connection, Engine, NodeDef, NodeRegistry, World};
use std::collections::HashMap;

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
const DURATION_S: f32 = 4.0;
const N_BLOCKS: u64 = ((SAMPLE_RATE as f32 * DURATION_S) as u64).div_ceil(BLOCK_SIZE as u64);

const BPMS: &[f32] = &[60.0, 90.0, 120.0, 180.0];

struct CellResult {
    bpm: f32,
    expected_count: usize,
    detected_count: usize,
    expected_interval_s: f32,
    mean_interval_s: f32,
    pass: bool,
}

fn run_cell(bpm: f32) -> CellResult {
    let mut registry = NodeRegistry::new();
    node_synth_onsets::register(&mut registry);
    node_onset::register(&mut registry);

    let synth_params: HashMap<String, f64> = [
        ("bpm".to_string(), bpm as f64),
        ("note_freq".to_string(), 440.0),
        ("note_duration_s".to_string(), 0.15),
        ("amplitude".to_string(), 0.5),
    ]
    .into();

    // Declare an out_port "trig" wired to the onset detector's output.
    let world = World {
        schema: None,
        world_version: 1,
        in_ports: vec![],
        out_ports: vec![BoundaryPort {
            id: "trig".to_string(),
            name: None,
            description: None,
        }],
        nodes: vec![
            NodeDef {
                id: "synth".to_string(),
                ty: "SynthOnsets".to_string(),
                params: synth_params,
                name: None,
                description: None,
            },
            NodeDef {
                id: "det".to_string(),
                ty: "Onset".to_string(),
                params: HashMap::new(),
                name: None,
                description: None,
            },
        ],
        connections: vec![
            Connection {
                from: "synth.audio_out".to_string(),
                to: "det.audio_in".to_string(),
            },
            // Route onset output to the boundary out_port "trig".
            Connection {
                from: "det.onset".to_string(),
                to: "trig".to_string(),
            },
        ],
    };

    let mut engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("engine build");

    let h_trig = engine.resolve_out_port("trig").expect("resolve trig");

    // Collect onset positions by block-by-block scan of the boundary out-port.
    let mut onset_sample_indices: Vec<u64> = Vec::new();
    for block_idx in 0..N_BLOCKS {
        engine.process_block(BLOCK_SIZE);
        let buf = engine.out_port(h_trig);
        for (i, &v) in buf.iter().enumerate() {
            if v > 0.5 {
                onset_sample_indices.push(block_idx * BLOCK_SIZE as u64 + i as u64);
            }
        }
    }

    let expected_interval_s = 60.0 / bpm;
    let expected_count = (DURATION_S / expected_interval_s) as usize;
    let detected_count = onset_sample_indices.len();

    // Mean inter-onset interval (skip the first since it's anchored at t=0).
    let mut intervals_s: Vec<f32> = Vec::new();
    for w in onset_sample_indices.windows(2) {
        intervals_s.push((w[1] - w[0]) as f32 / SAMPLE_RATE as f32);
    }
    let mean_interval_s = if intervals_s.is_empty() {
        0.0
    } else {
        intervals_s.iter().sum::<f32>() / intervals_s.len() as f32
    };

    // Tolerances: count within ±1 (boundary slack at start/end of window);
    // mean interval within 5 %.
    let count_ok = (detected_count as i64 - expected_count as i64).abs() <= 1;
    let interval_ok = if mean_interval_s > 0.0 {
        (mean_interval_s - expected_interval_s).abs() / expected_interval_s < 0.05
    } else {
        false
    };
    let pass = count_ok && interval_ok;

    CellResult {
        bpm,
        expected_count,
        detected_count,
        expected_interval_s,
        mean_interval_s,
        pass,
    }
}

#[test]
fn onset_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let results: Vec<CellResult> = BPMS.iter().map(|&bpm| run_cell(bpm)).collect();

    let csv_path = format!("{tmpdir}/onset_sweep.csv");
    let mut csv = String::from(
        "bpm,expected_count,detected_count,expected_interval_s,mean_interval_s,pass\n",
    );
    for r in &results {
        csv.push_str(&format!(
            "{:.1},{},{},{:.4},{:.4},{}\n",
            r.bpm,
            r.expected_count,
            r.detected_count,
            r.expected_interval_s,
            r.mean_interval_s,
            r.pass
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    let mut grid = String::from("  bpm | expected | detected | mean IOI |  pass\n");
    for r in &results {
        grid.push_str(&format!(
            " {:>4.0} | {:>8} | {:>8} | {:>7.3}s | {}\n",
            r.bpm,
            r.expected_count,
            r.detected_count,
            r.mean_interval_s,
            if r.pass { " ok " } else { "FAIL" }
        ));
    }
    let grid_path = format!("{tmpdir}/onset_sweep_grid.txt");
    std::fs::write(&grid_path, &grid).expect("writing grid artifact");
    println!("Grid written to: {grid_path}");
    println!("\n{grid}");

    let failures: Vec<&CellResult> = results.iter().filter(|r| !r.pass).collect();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!(
                "FAIL bpm={:.1} detected={} expected={} mean_ioi={:.3}s expected_ioi={:.3}s",
                f.bpm, f.detected_count, f.expected_count, f.mean_interval_s, f.expected_interval_s
            );
        }
        panic!("{} cells failed", failures.len());
    }
}
