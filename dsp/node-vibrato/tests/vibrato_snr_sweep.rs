//! Vibrato × SNR sweep: Tier-2 oracle for the vibrato analyzer.
//!
//! Wires SynthVibratoSine + scaled SynthPinkNoise → MixSum → PitchYin →
//! Vibrato across a grid of vibrato rates and SNRs. The clean case is the
//! Tier-1 oracle (see vibrato_sweep.rs); this sweep checks robustness when
//! the input is contaminated with broadband noise — what a real mic feed
//! looks like.
//!
//! Pass criterion (asserted only for SNR ≥ 20 dB, matching the
//! pitch_sweep contract): rate within ±0.7 Hz and depth within ±25 cents.
//! Lower-SNR cells are recorded for the artifact but not asserted.

use engine::{BoundaryPort, Connection, Engine, NodeDef, NodeRegistry, World};
use std::collections::HashMap;

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
// 3 s — same window the Tier-1 sweep uses.
const N_BLOCKS: u64 = 281;

const RATES_HZ: &[f32] = &[5.0, 6.0, 7.0];
const SNRS_DB: &[f32] = &[f32::INFINITY, 40.0, 30.0, 20.0, 10.0];
const FIXED_DEPTH_CENTS: f32 = 50.0;
const CARRIER_HZ: f32 = 440.0;
const SINE_AMPLITUDE: f32 = 0.5;

struct CellResult {
    rate_hz: f32,
    snr_db: f32,
    est_rate_hz: f32,
    est_depth_cents: f32,
    pass: bool,
}

fn run_cell(rate_hz: f32, snr_db: f32, rate_idx: usize, snr_idx: usize) -> CellResult {
    let sine_rms = SINE_AMPLITUDE / 2.0f32.sqrt();
    let gain_linear: f32 = if snr_db.is_finite() {
        sine_rms / 10.0f32.powf(snr_db / 20.0)
    } else {
        0.0
    };
    let noise_seed = 0xC0FFEE_u64 ^ ((rate_idx as u64) << 16) ^ (snr_idx as u64);

    let mut registry = NodeRegistry::new();
    node_synth_vibrato_sine::register(&mut registry);
    node_synth_pink_noise::register(&mut registry);
    node_gain::register(&mut registry);
    node_mix_sum::register(&mut registry);
    node_pitch_yin::register(&mut registry);
    node_vibrato::register(&mut registry);

    let synth_params: HashMap<String, f64> = [
        ("carrier_freq".to_string(), CARRIER_HZ as f64),
        ("amplitude".to_string(), SINE_AMPLITUDE as f64),
        ("vibrato_rate".to_string(), rate_hz as f64),
        ("vibrato_depth_cents".to_string(), FIXED_DEPTH_CENTS as f64),
    ]
    .into();

    let noise_params: HashMap<String, f64> = [
        ("amplitude".to_string(), 1.0),
        ("seed".to_string(), noise_seed as f64),
    ]
    .into();

    let gain_params: HashMap<String, f64> =
        [("gain_linear".to_string(), gain_linear as f64)].into();
    let mix_params: HashMap<String, f64> = [("channels".to_string(), 2.0)].into();

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
                id: "noise".to_string(),
                ty: "SynthPinkNoise".to_string(),
                params: noise_params,
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
                id: "mix".to_string(),
                ty: "MixSum".to_string(),
                params: mix_params,
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
                to: "mix.in_0".to_string(),
            },
            Connection {
                from: "noise.audio_out".to_string(),
                to: "gain.audio_in".to_string(),
            },
            Connection {
                from: "gain.audio_out".to_string(),
                to: "mix.in_1".to_string(),
            },
            Connection {
                from: "mix.out".to_string(),
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
                from: "vib.depth".to_string(),
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
        (est_rate_hz - rate_hz).abs() <= 0.7 && (est_depth_cents - FIXED_DEPTH_CENTS).abs() <= 25.0;

    CellResult {
        rate_hz,
        snr_db,
        est_rate_hz,
        est_depth_cents,
        pass,
    }
}

#[test]
fn vibrato_snr_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let mut results: Vec<CellResult> = Vec::new();
    for (ri, &rate) in RATES_HZ.iter().enumerate() {
        for (si, &snr) in SNRS_DB.iter().enumerate() {
            results.push(run_cell(rate, snr, ri, si));
        }
    }

    let snr_label = |snr: f32| -> String {
        if snr.is_infinite() {
            "inf".to_string()
        } else {
            format!("{snr:.0}")
        }
    };

    let csv_path = format!("{tmpdir}/vibrato_snr_sweep.csv");
    let mut csv = String::from("rate_hz,snr_db,est_rate_hz,est_depth_cents,pass\n");
    for r in &results {
        csv.push_str(&format!(
            "{:.2},{},{:.3},{:.1},{}\n",
            r.rate_hz,
            snr_label(r.snr_db),
            r.est_rate_hz,
            r.est_depth_cents,
            r.pass
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    let mut grid = String::from("rate↓ SNR→ |");
    for &snr in SNRS_DB {
        grid.push_str(&format!("{:>8} |", format!("{} dB", snr_label(snr))));
    }
    grid.push('\n');
    for &rate in RATES_HZ {
        grid.push_str(&format!("    {rate:>4.1} |"));
        for &snr in SNRS_DB {
            let r = results
                .iter()
                .find(|r| {
                    r.rate_hz == rate
                        && (r.snr_db == snr || (r.snr_db.is_infinite() && snr.is_infinite()))
                })
                .unwrap();
            let asserted = snr >= 20.0 || snr.is_infinite();
            let mark = if r.pass {
                "  ok  "
            } else if asserted {
                " FAIL "
            } else {
                "  -   "
            };
            grid.push_str(&format!(" {mark} |"));
        }
        grid.push('\n');
    }
    let grid_path = format!("{tmpdir}/vibrato_snr_sweep_grid.txt");
    std::fs::write(&grid_path, &grid).expect("writing grid artifact");
    println!("Grid written to: {grid_path}");
    println!("\n{grid}");

    let failures: Vec<&CellResult> = results
        .iter()
        .filter(|r| {
            let asserted = r.snr_db >= 20.0 || r.snr_db.is_infinite();
            asserted && !r.pass
        })
        .collect();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!(
                "FAIL rate={:.1} SNR={} → est_rate={:.3} est_depth={:.1}",
                f.rate_hz,
                snr_label(f.snr_db),
                f.est_rate_hz,
                f.est_depth_cents
            );
        }
        panic!("{} cells failed", failures.len());
    }
}
