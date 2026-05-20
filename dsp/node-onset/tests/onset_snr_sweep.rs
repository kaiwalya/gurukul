//! Onset × SNR sweep: Tier-2 oracle for the onset detector.
//!
//! Wires SynthOnsets + scaled SynthPinkNoise → MixSum → Onset across a
//! grid of BPMs and SNRs. Per cell we check that the detected count is
//! within ±1 of expected and the mean inter-onset interval is within 5 %
//! of 60/bpm — same Tier-1 contract, but with broadband noise mixed in.
//!
//! Assertion is gated on SNR ≥ 20 dB (matching the pitch_sweep contract);
//! lower-SNR cells are recorded for the artifact but not asserted.

use engine::{BoundaryPort, Connection, Engine, NodeDef, NodeRegistry, World};
use std::collections::HashMap;

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
const DURATION_S: f32 = 4.0;
const N_BLOCKS: u64 = ((SAMPLE_RATE as f32 * DURATION_S) as u64).div_ceil(BLOCK_SIZE as u64);

const BPMS: &[f32] = &[90.0, 120.0, 180.0];
const SNRS_DB: &[f32] = &[f32::INFINITY, 40.0, 30.0, 20.0, 10.0];
const NOTE_AMPLITUDE: f32 = 0.5;

struct CellResult {
    bpm: f32,
    snr_db: f32,
    expected_count: usize,
    detected_count: usize,
    mean_interval_s: f32,
    expected_interval_s: f32,
    pass: bool,
}

fn run_cell(bpm: f32, snr_db: f32, bpm_idx: usize, snr_idx: usize) -> CellResult {
    // SynthOnsets emits a 0.15 s sine burst at `note_freq` per beat at amplitude
    // 0.5; we treat that amplitude as the signal level for SNR (the off-beat
    // gaps don't enter the SNR calculation — the detector keys off the burst
    // energy, which is the relevant figure of merit).
    let burst_rms = NOTE_AMPLITUDE / 2.0f32.sqrt();
    let gain_linear: f32 = if snr_db.is_finite() {
        burst_rms / 10.0f32.powf(snr_db / 20.0)
    } else {
        0.0
    };
    let noise_seed = 0xBEEFC0DE_u64 ^ ((bpm_idx as u64) << 16) ^ (snr_idx as u64);

    let mut registry = NodeRegistry::new();
    node_synth_onsets::register(&mut registry);
    node_synth_pink_noise::register(&mut registry);
    node_gain::register(&mut registry);
    node_mix_sum::register(&mut registry);
    node_onset::register(&mut registry);

    let synth_params: HashMap<String, f64> = [
        ("bpm".to_string(), bpm as f64),
        ("note_freq".to_string(), 440.0),
        ("note_duration_s".to_string(), 0.15),
        ("amplitude".to_string(), NOTE_AMPLITUDE as f64),
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
        out_ports: vec![BoundaryPort {
            id: "onset_out".to_string(),
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
                to: "det.audio_in".to_string(),
            },
            Connection {
                from: "det.onset".to_string(),
                to: "onset_out".to_string(),
            },
        ],
    };

    let mut engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("engine build");

    let h_onset = engine
        .resolve_out_port("onset_out")
        .expect("resolve onset_out");

    let mut onset_sample_indices: Vec<u64> = Vec::new();
    for block_idx in 0..N_BLOCKS {
        engine.process_block(BLOCK_SIZE);
        let buf = engine.out_port(h_onset);
        for (i, &v) in buf.iter().enumerate() {
            if v > 0.5 {
                onset_sample_indices.push(block_idx * BLOCK_SIZE as u64 + i as u64);
            }
        }
    }

    let expected_interval_s = 60.0 / bpm;
    let expected_count = (DURATION_S / expected_interval_s) as usize;
    let detected_count = onset_sample_indices.len();

    let mut intervals_s: Vec<f32> = Vec::new();
    for w in onset_sample_indices.windows(2) {
        intervals_s.push((w[1] - w[0]) as f32 / SAMPLE_RATE as f32);
    }
    let mean_interval_s = if intervals_s.is_empty() {
        0.0
    } else {
        intervals_s.iter().sum::<f32>() / intervals_s.len() as f32
    };

    let count_ok = (detected_count as i64 - expected_count as i64).abs() <= 1;
    let interval_ok = mean_interval_s > 0.0
        && (mean_interval_s - expected_interval_s).abs() / expected_interval_s < 0.05;
    let pass = count_ok && interval_ok;

    CellResult {
        bpm,
        snr_db,
        expected_count,
        detected_count,
        mean_interval_s,
        expected_interval_s,
        pass,
    }
}

#[test]
fn onset_snr_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let mut results: Vec<CellResult> = Vec::new();
    for (bi, &bpm) in BPMS.iter().enumerate() {
        for (si, &snr) in SNRS_DB.iter().enumerate() {
            results.push(run_cell(bpm, snr, bi, si));
        }
    }

    let snr_label = |snr: f32| -> String {
        if snr.is_infinite() {
            "inf".to_string()
        } else {
            format!("{snr:.0}")
        }
    };

    let csv_path = format!("{tmpdir}/onset_snr_sweep.csv");
    let mut csv = String::from(
        "bpm,snr_db,expected_count,detected_count,mean_interval_s,expected_interval_s,pass\n",
    );
    for r in &results {
        csv.push_str(&format!(
            "{:.1},{},{},{},{:.4},{:.4},{}\n",
            r.bpm,
            snr_label(r.snr_db),
            r.expected_count,
            r.detected_count,
            r.mean_interval_s,
            r.expected_interval_s,
            r.pass
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    let mut grid = String::from(" bpm↓ SNR→ |");
    for &snr in SNRS_DB {
        grid.push_str(&format!("{:>8} |", format!("{} dB", snr_label(snr))));
    }
    grid.push('\n');
    for &bpm in BPMS {
        grid.push_str(&format!("    {bpm:>4.0} |"));
        for &snr in SNRS_DB {
            let r = results
                .iter()
                .find(|r| {
                    r.bpm == bpm
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
    let grid_path = format!("{tmpdir}/onset_snr_sweep_grid.txt");
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
                "FAIL bpm={:.0} SNR={} detected={} expected={} mean_ioi={:.3}s expected_ioi={:.3}s",
                f.bpm,
                snr_label(f.snr_db),
                f.detected_count,
                f.expected_count,
                f.mean_interval_s,
                f.expected_interval_s
            );
        }
        panic!("{} cells failed", failures.len());
    }
}
