/// Pitch × SNR sweep: Tier-1 oracle for the YIN pitch detector.
///
/// Wires SynthSine + SynthPinkNoise(scaled) → MixSum → PitchYin → PitchError
/// across a grid of frequencies and SNRs. Asserts ≤ 10 cents median error for
/// all cells with SNR ≥ 20 dB.
use engine::{Connection, Engine, Node, NodeDef, NodeRegistry, PortSpec, PortType, World};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Test-only helper: emits a constant Feature value (ZOH) from a field.
// Strictly private to this integration test — not exposed as a public node.
// ---------------------------------------------------------------------------
struct ConstantFeature {
    value: f32,
}

impl Node for ConstantFeature {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}

    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if let Some(out) = outputs.first_mut() {
            out[..nframes].fill(self.value);
        }
    }
}

fn register_constant_feature(registry: &mut NodeRegistry, value: f32) {
    // Registered under a unique name per call so each cell gets its own factory.
    // All instances use the same type name "ConstantFeature"; params select the value.
    registry.register_full(
        "ConstantFeature",
        vec![],
        vec![PortSpec {
            name: "feature_out",
            ty: PortType::Feature,
        }],
        vec![],
        Box::new(move |_params: &HashMap<String, f64>| {
            Box::new(ConstantFeature { value }) as Box<dyn Node>
        }),
    );
}

// ---------------------------------------------------------------------------
// Grid definition
// ---------------------------------------------------------------------------
const FREQS_HZ: &[f32] = &[82.0, 220.0, 440.0, 880.0, 1760.0];
const SNRS_DB: &[f32] = &[f32::INFINITY, 40.0, 30.0, 20.0, 10.0, 0.0];

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
// 2 seconds = 48000*2 / 512 = 187.5 → 188 blocks
const N_BLOCKS: u64 = 188;

// ---------------------------------------------------------------------------
// Per-cell result
// ---------------------------------------------------------------------------
struct CellResult {
    freq_hz: f32,
    snr_db: f32,
    median_abs_err_cents: f32,
    voiced_frac: f32,
    /// True iff snr_db >= 20 AND median_abs_err_cents <= 10.
    pass: bool,
}

fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn run_cell(freq_hz: f32, snr_db: f32, freq_idx: usize, snr_idx: usize) -> CellResult {
    // Sine amplitude = 0.5; RMS = amplitude / sqrt(2).
    let sine_amplitude = 0.5f32;
    let sine_rms = sine_amplitude / 2.0f32.sqrt();

    // Pink noise amplitude=1.0 → RMS ≈ 1.0. Scale it via GainNode.
    // gain = sine_rms / 10^(snr_db/20); for SNR=∞ gain=0.
    let gain_linear: f32 = if snr_db.is_finite() {
        sine_rms / 10.0f32.powf(snr_db / 20.0)
    } else {
        0.0
    };

    // Deterministic, cell-indexed seed (SplitMix64 in SynthPinkNoise ensures
    // uncorrelated streams for nearby indices).
    let noise_seed = 0xC0FFEEu64 ^ ((freq_idx as u64) << 16) ^ (snr_idx as u64);

    // Build a fresh registry for this cell so the ConstantFeature closure captures
    // the correct freq_hz value.
    let mut registry = NodeRegistry::new();
    node_synth_sine::register(&mut registry);
    node_synth_pink_noise::register(&mut registry);
    node_gain::register(&mut registry);
    node_mix_sum::register(&mut registry);
    node_pitch_yin::register(&mut registry);
    node_pitch_error::register(&mut registry);
    register_constant_feature(&mut registry, freq_hz);

    let sine_params: HashMap<String, f64> = [
        ("freq".to_string(), freq_hz as f64),
        ("amplitude".to_string(), sine_amplitude as f64),
    ]
    .into();

    let noise_params: HashMap<String, f64> = [
        ("amplitude".to_string(), 1.0f64),
        ("seed".to_string(), noise_seed as f64),
    ]
    .into();

    let gain_params: HashMap<String, f64> =
        [("gain_linear".to_string(), gain_linear as f64)].into();

    // MixSum with channels=2 → ports: in_0, in_1
    let mix_params: HashMap<String, f64> = [("channels".to_string(), 2.0f64)].into();

    // PitchYin defaults (window=2048, hop=512, fmin=50, fmax=2000, threshold=0.1)
    let yin_params: HashMap<String, f64> = HashMap::new();

    let world = World {
        schema: None,
        nodes: vec![
            NodeDef {
                id: "sine".to_string(),
                ty: "SynthSine".to_string(),
                params: sine_params,
            },
            NodeDef {
                id: "noise".to_string(),
                ty: "SynthPinkNoise".to_string(),
                params: noise_params,
            },
            NodeDef {
                id: "gain".to_string(),
                ty: "GainNode".to_string(),
                params: gain_params,
            },
            NodeDef {
                id: "mix".to_string(),
                ty: "MixSum".to_string(),
                params: mix_params,
            },
            NodeDef {
                id: "yin".to_string(),
                ty: "PitchYin".to_string(),
                params: yin_params,
            },
            NodeDef {
                id: "refnode".to_string(),
                ty: "ConstantFeature".to_string(),
                params: HashMap::new(),
            },
            NodeDef {
                id: "perr".to_string(),
                ty: "PitchError".to_string(),
                params: HashMap::new(),
            },
        ],
        connections: vec![
            // sine → mix.in_0
            Connection {
                from: "sine.audio_out".to_string(),
                to: "mix.in_0".to_string(),
            },
            // noise → gain → mix.in_1
            Connection {
                from: "noise.audio_out".to_string(),
                to: "gain.audio_in".to_string(),
            },
            Connection {
                from: "gain.audio_out".to_string(),
                to: "mix.in_1".to_string(),
            },
            // mix → yin
            Connection {
                from: "mix.out".to_string(),
                to: "yin.audio_in".to_string(),
            },
            // yin.f0 → perr.f0_estimated
            Connection {
                from: "yin.f0".to_string(),
                to: "perr.f0_estimated".to_string(),
            },
            // refnode → perr.f0_reference
            Connection {
                from: "refnode.feature_out".to_string(),
                to: "perr.f0_reference".to_string(),
            },
        ],
    };

    let mut engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("engine build failed");

    let mut error_samples: Vec<f32> = Vec::with_capacity(N_BLOCKS as usize);
    let mut voiced_count = 0u64;
    let total_blocks = N_BLOCKS;

    for _ in 0..total_blocks {
        engine.run_blocks(1);

        let error_buf = engine.last_block("perr", "error_cents").unwrap();
        let voiced_buf = engine.last_block("perr", "voiced").unwrap();

        // ZOH: last sample of the block holds the scalar for this block.
        let err = *error_buf.last().unwrap();
        let v = *voiced_buf.last().unwrap();

        if v == 1.0 {
            voiced_count += 1;
            error_samples.push(err.abs());
        }
    }

    let voiced_frac = voiced_count as f32 / total_blocks as f32;
    let median_abs_err_cents = median(&mut error_samples);

    let asserted_cell = snr_db >= 20.0 || snr_db.is_infinite();
    let pass = if asserted_cell {
        median_abs_err_cents <= 10.0 && voiced_frac >= 0.5
    } else {
        // Not asserted — record pass=false only to show "not checked"
        // but we won't fail the test on these.
        median_abs_err_cents <= 10.0 && voiced_frac >= 0.5
    };

    CellResult {
        freq_hz,
        snr_db,
        median_abs_err_cents,
        voiced_frac,
        pass,
    }
}

#[test]
fn pitch_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let mut results: Vec<CellResult> = Vec::new();

    for (fi, &freq) in FREQS_HZ.iter().enumerate() {
        for (si, &snr) in SNRS_DB.iter().enumerate() {
            let cell = run_cell(freq, snr, fi, si);
            println!(
                "  freq={:6.0} Hz  SNR={:>6}  median_abs_err={:.2} cents  voiced_frac={:.2}  {}",
                cell.freq_hz,
                if cell.snr_db.is_infinite() {
                    "  inf".to_string()
                } else {
                    format!("{:>5.0} dB", cell.snr_db)
                },
                cell.median_abs_err_cents,
                cell.voiced_frac,
                if cell.pass { "OK" } else { "FAIL" },
            );
            results.push(cell);
        }
    }

    // --- ASCII grid --------------------------------------------------------
    // Rows = freqs, columns = SNRs
    let snr_labels: Vec<String> = SNRS_DB
        .iter()
        .map(|&s| {
            if s.is_infinite() {
                " inf".to_string()
            } else {
                format!("{:>4.0}", s)
            }
        })
        .collect();

    let mut grid = String::new();
    // Header
    grid.push_str("freq\\SNR |");
    for label in &snr_labels {
        grid.push_str(&format!("{:>12}", format!("{} dB", label)));
    }
    grid.push('\n');
    grid.push_str(&"-".repeat(9 + 12 * SNRS_DB.len()));
    grid.push('\n');

    for (fi, &freq) in FREQS_HZ.iter().enumerate() {
        grid.push_str(&format!("{:>8} |", format!("{:.0}Hz", freq)));
        for si in 0..SNRS_DB.len() {
            let cell = &results[fi * SNRS_DB.len() + si];
            let marker = if cell.pass { "OK" } else { "FAIL" };
            let cell_str = format!("{:+.1}c {}", cell.median_abs_err_cents, marker);
            grid.push_str(&format!("{:>12}", cell_str));
        }
        grid.push('\n');
    }

    println!("\nPitch sweep grid (rows=freq, cols=SNR):");
    println!("{grid}");

    // --- CSV artifact -------------------------------------------------------
    let csv_path = format!("{tmpdir}/pitch_sweep.csv");
    let mut csv = String::from("freq_hz,snr_db,median_err_cents,voiced_frac,pass\n");
    for cell in &results {
        let snr_str = if cell.snr_db.is_infinite() {
            "inf".to_string()
        } else {
            format!("{}", cell.snr_db)
        };
        csv.push_str(&format!(
            "{},{},{:.4},{:.4},{}\n",
            cell.freq_hz,
            snr_str,
            cell.median_abs_err_cents,
            cell.voiced_frac,
            if cell.pass { "true" } else { "false" },
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    // --- Grid text artifact -------------------------------------------------
    let grid_path = format!("{tmpdir}/pitch_sweep_grid.txt");
    std::fs::write(&grid_path, &grid).expect("writing grid artifact");
    println!("Grid written to: {grid_path}");

    // --- Assertions (only for SNR >= 20 dB) ---------------------------------
    let mut failures: Vec<String> = Vec::new();
    for cell in &results {
        let must_pass = cell.snr_db >= 20.0 || cell.snr_db.is_infinite();
        if must_pass && !cell.pass {
            let snr_label = if cell.snr_db.is_infinite() {
                "inf".to_string()
            } else {
                format!("{:.0} dB", cell.snr_db)
            };
            failures.push(format!(
                "freq={:.0}Hz SNR={} median_abs_err={:.2}c voiced_frac={:.2}",
                cell.freq_hz, snr_label, cell.median_abs_err_cents, cell.voiced_frac
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Pitch sweep FAILED on {} cell(s). See {grid_path} for the full grid.\n{}",
        failures.len(),
        failures.join("\n")
    );
}
