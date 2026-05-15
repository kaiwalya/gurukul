//! Breath × duration/amplitude sweep: Tier-1 oracle for the breath
//! detector.
//!
//! Wires SynthBreath → Breath across a grid of (breath_duration, amplitude).
//! For each cell we check:
//!   - active fraction within the breath window is high (≥ 0.8 — the
//!     leading ring-fill period is unavoidable but small),
//!   - active fraction outside the breath window is low (≤ 0.05),
//!   - the detector unlatches (no permanent stickiness).

use engine::{Connection, Engine, NodeDef, NodeRegistry, World};
use std::collections::HashMap;

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
const PERIOD_S: f32 = 2.0;
const DURATION_S: f32 = 4.0;
const N_BLOCKS: u64 = ((SAMPLE_RATE as f32 * DURATION_S) as u64).div_ceil(BLOCK_SIZE as u64);

const BREATH_DURATIONS_S: &[f32] = &[0.3, 0.6, 1.2];
const AMPLITUDES: &[f32] = &[0.05, 0.15, 0.40];

struct CellResult {
    breath_duration_s: f32,
    amplitude: f32,
    active_in_breath: f32,
    active_out_of_breath: f32,
    pass: bool,
}

fn run_cell(breath_duration_s: f32, amplitude: f32) -> CellResult {
    let mut registry = NodeRegistry::new();
    node_synth_breath::register(&mut registry);
    node_breath::register(&mut registry);

    let synth_params: HashMap<String, f64> = [
        ("period_s".to_string(), PERIOD_S as f64),
        ("breath_duration_s".to_string(), breath_duration_s as f64),
        ("amplitude".to_string(), amplitude as f64),
        ("seed".to_string(), 1.0),
    ]
    .into();

    let world = World {
        schema: None,
        nodes: vec![
            NodeDef {
                id: "synth".to_string(),
                ty: "SynthBreath".to_string(),
                params: synth_params,
            },
            NodeDef {
                id: "det".to_string(),
                ty: "Breath".to_string(),
                params: HashMap::new(),
            },
        ],
        connections: vec![Connection {
            from: "synth.audio_out".to_string(),
            to: "det.audio_in".to_string(),
        }],
    };

    let mut engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("engine build");

    // Tally active samples inside and outside the breath window for each period.
    let breath_samples = (SAMPLE_RATE as f32 * breath_duration_s) as u64;
    let period_samples = (SAMPLE_RATE as f32 * PERIOD_S) as u64;

    let mut active_in: u64 = 0;
    let mut total_in: u64 = 0;
    let mut active_out: u64 = 0;
    let mut total_out: u64 = 0;

    // Skip the first 100 ms (ring-fill) and the first 100 ms inside each
    // breath (envelope attack + filter settle) when measuring "in-breath"
    // activation. Also skip 150 ms after the breath ends from the
    // "out-of-breath" tally — the detector needs roughly one ring window
    // (~21 ms) plus min_release_samples (50 ms) to unlatch, and the
    // envelope itself releases over 100 ms.
    let warmup_samples: u64 = (SAMPLE_RATE / 10) as u64;
    let in_breath_skip: u64 = (SAMPLE_RATE as f32 * 0.10) as u64;
    let out_breath_skip: u64 = (SAMPLE_RATE as f32 * 0.15) as u64;

    for block_idx in 0..N_BLOCKS {
        engine.run_blocks(1);
        let buf = engine.last_block("det", "breath").unwrap();
        for (i, &v) in buf.iter().enumerate() {
            let abs_idx = block_idx * BLOCK_SIZE as u64 + i as u64;
            if abs_idx < warmup_samples {
                continue;
            }
            let into_period = abs_idx % period_samples;
            let active = v > 0.5;
            if into_period < breath_samples {
                if into_period >= in_breath_skip {
                    total_in += 1;
                    if active {
                        active_in += 1;
                    }
                }
            } else if into_period >= breath_samples + out_breath_skip {
                total_out += 1;
                if active {
                    active_out += 1;
                }
            }
        }
    }

    let active_in_breath = if total_in > 0 {
        active_in as f32 / total_in as f32
    } else {
        0.0
    };
    let active_out_of_breath = if total_out > 0 {
        active_out as f32 / total_out as f32
    } else {
        0.0
    };

    let pass = active_in_breath >= 0.80 && active_out_of_breath <= 0.05;

    CellResult {
        breath_duration_s,
        amplitude,
        active_in_breath,
        active_out_of_breath,
        pass,
    }
}

#[test]
fn breath_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let mut results: Vec<CellResult> = Vec::new();
    for &dur in BREATH_DURATIONS_S {
        for &amp in AMPLITUDES {
            results.push(run_cell(dur, amp));
        }
    }

    let csv_path = format!("{tmpdir}/breath_sweep.csv");
    let mut csv =
        String::from("breath_duration_s,amplitude,active_in_breath,active_out_of_breath,pass\n");
    for r in &results {
        csv.push_str(&format!(
            "{:.2},{:.3},{:.4},{:.4},{}\n",
            r.breath_duration_s, r.amplitude, r.active_in_breath, r.active_out_of_breath, r.pass
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    let mut grid = String::from(" dur(s) | amp   | in_frac | out_frac | pass\n");
    for r in &results {
        grid.push_str(&format!(
            "  {:>4.2} | {:>5.3} | {:>7.3} | {:>8.4} | {}\n",
            r.breath_duration_s,
            r.amplitude,
            r.active_in_breath,
            r.active_out_of_breath,
            if r.pass { " ok " } else { "FAIL" }
        ));
    }
    let grid_path = format!("{tmpdir}/breath_sweep_grid.txt");
    std::fs::write(&grid_path, &grid).expect("writing grid artifact");
    println!("Grid written to: {grid_path}");
    println!("\n{grid}");

    let failures: Vec<&CellResult> = results.iter().filter(|r| !r.pass).collect();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!(
                "FAIL dur={:.2}s amp={:.3} in={:.3} out={:.4}",
                f.breath_duration_s, f.amplitude, f.active_in_breath, f.active_out_of_breath
            );
        }
        panic!("{} cells failed", failures.len());
    }
}
