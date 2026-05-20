//! Breath × sustained-tone distractor sweep: Tier-2 oracle for the breath
//! detector.
//!
//! SNR-style noise mixing is a poor stress for breath since breath is itself
//! broadband noise. The realistic confounder is a *sustained pitched tone*
//! (vowel-like, sung) underneath: the detector must
//!   1. not fire on the sine alone (false-positive check), AND
//!   2. still fire during the breath when the sine is mixed in
//!      (true-positive-with-distractor check).
//!
//! Each cell pairs (breath_amplitude, distractor_amplitude); the breath cycles
//! every 2 s for 4 s; the sine sustains across the full 4 s.
//!
//! Pass criteria:
//!   - false-positive rate on sine-only audio ≤ 0.05 (asserted for *every*
//!     cell — this is the safety-critical bound; the detector must not turn
//!     a sustained vowel into a breath event), AND
//!   - active-in-breath fraction ≥ 0.80 when the breath is louder than the
//!     distractor (asserted). When the distractor is *louder* than the
//!     breath the burst is buried under tonal energy and the detector is
//!     expected to miss it — recorded but not asserted, so the cliff is
//!     visible in the artifact.

use engine::{
    BoundaryPort, Connection, Engine, Node, NodeDef, NodeRegistry, PortSpec, PortType, World,
};
use std::collections::HashMap;

const SAMPLE_RATE: u32 = 48000;
const BLOCK_SIZE: usize = 512;
const PERIOD_S: f32 = 2.0;
const DURATION_S: f32 = 4.0;
const BREATH_DURATION_S: f32 = 0.6;
const N_BLOCKS: u64 = ((SAMPLE_RATE as f32 * DURATION_S) as u64).div_ceil(BLOCK_SIZE as u64);

// (breath_amplitude, distractor_amplitude) — both relative to a full-scale 1.0.
const BREATH_AMPS: &[f32] = &[0.10, 0.20, 0.30];
const DISTRACTOR_AMPS: &[f32] = &[0.10, 0.20, 0.30, 0.50];

struct CellResult {
    breath_amp: f32,
    distractor_amp: f32,
    false_positive_frac: f32,
    active_in_breath: f32,
    /// True iff this cell's outcome meets the bar we assert on. The
    /// active-in-breath bar is only enforced when the breath is at least as
    /// loud as the distractor; below that, the cell is recorded but the
    /// `pass` flag still reflects whether the *false-positive* bound held.
    pass: bool,
    /// Whether the active-in-breath bar was asserted for this cell.
    asserted_active: bool,
}

/// Test-only zero source: emits silence on its audio_out port. Used as a
/// stand-in for "no breath" so the MixSum graph in mode A still has two
/// inputs without rebuilding it.
struct ZeroSource;
impl Node for ZeroSource {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {}
    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if let Some(out) = outputs.first_mut() {
            out[..nframes].fill(0.0);
        }
    }
}

fn register_zero_source(registry: &mut NodeRegistry) {
    registry.register_full(
        "ZeroSource",
        vec![],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![],
        Box::new(|_params: &HashMap<String, f64>| Box::new(ZeroSource) as Box<dyn Node>),
    );
}

/// One Engine run. `breath_amp` of 0 means SynthBreath emits silence
/// (we route a ZeroSource into the mix instead). Returns the per-sample
/// breath flag stream.
fn run_one(breath_amp: f32, distractor_amp: f32, cell_seed: u64) -> Vec<f32> {
    let mut registry = NodeRegistry::new();
    node_synth_breath::register(&mut registry);
    node_synth_sine::register(&mut registry);
    node_mix_sum::register(&mut registry);
    node_breath::register(&mut registry);
    register_zero_source(&mut registry);

    let breath_ty = if breath_amp > 0.0 {
        "SynthBreath"
    } else {
        "ZeroSource"
    };
    let breath_params: HashMap<String, f64> = if breath_amp > 0.0 {
        [
            ("period_s".to_string(), PERIOD_S as f64),
            ("breath_duration_s".to_string(), BREATH_DURATION_S as f64),
            ("amplitude".to_string(), breath_amp as f64),
            ("seed".to_string(), cell_seed as f64),
        ]
        .into()
    } else {
        HashMap::new()
    };

    let sine_params: HashMap<String, f64> = [
        ("freq".to_string(), 440.0),
        ("amplitude".to_string(), distractor_amp as f64),
        ("phase".to_string(), 0.0),
    ]
    .into();

    let mix_params: HashMap<String, f64> = [("channels".to_string(), 2.0)].into();

    let world = World {
        schema: None,
        world_version: 1,
        in_ports: vec![],
        out_ports: vec![BoundaryPort {
            id: "breath_out".to_string(),
            name: None,
            description: None,
        }],
        nodes: vec![
            NodeDef {
                id: "breath_src".to_string(),
                ty: breath_ty.to_string(),
                params: breath_params,
                name: None,
                description: None,
            },
            NodeDef {
                id: "sine".to_string(),
                ty: "SynthSine".to_string(),
                params: sine_params,
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
                ty: "Breath".to_string(),
                params: HashMap::new(),
                name: None,
                description: None,
            },
        ],
        connections: vec![
            Connection {
                from: "breath_src.audio_out".to_string(),
                to: "mix.in_0".to_string(),
            },
            Connection {
                from: "sine.audio_out".to_string(),
                to: "mix.in_1".to_string(),
            },
            Connection {
                from: "mix.out".to_string(),
                to: "det.audio_in".to_string(),
            },
            Connection {
                from: "det.breath".to_string(),
                to: "breath_out".to_string(),
            },
        ],
    };

    let mut engine =
        Engine::build(&world, &registry, SAMPLE_RATE, BLOCK_SIZE).expect("engine build");

    let h_breath = engine
        .resolve_out_port("breath_out")
        .expect("resolve breath_out");

    let mut samples: Vec<f32> = Vec::with_capacity((N_BLOCKS as usize) * BLOCK_SIZE);
    for _ in 0..N_BLOCKS {
        engine.process_block(BLOCK_SIZE);
        let buf = engine.out_port(h_breath);
        samples.extend_from_slice(buf);
    }
    samples
}

fn run_cell(breath_amp: f32, distractor_amp: f32, ba_idx: usize, da_idx: usize) -> CellResult {
    let cell_seed = 0x51DE5A1Du64 ^ ((ba_idx as u64) << 16) ^ (da_idx as u64);

    let warmup_samples: u64 = (SAMPLE_RATE / 10) as u64;
    let in_breath_skip: u64 = (SAMPLE_RATE as f32 * 0.10) as u64;
    let out_breath_skip: u64 = (SAMPLE_RATE as f32 * 0.15) as u64;
    let breath_samples = (SAMPLE_RATE as f32 * BREATH_DURATION_S) as u64;
    let period_samples = (SAMPLE_RATE as f32 * PERIOD_S) as u64;

    // False-positive accounting spans two windows:
    //   - Mode A (distractor alone): every post-warmup sample contributes.
    //   - Mode B (breath + distractor): only the out-of-breath windows
    //     contribute (in-breath samples should fire).
    let mut fp_active: u64 = 0;
    let mut fp_total: u64 = 0;

    // ---- Mode A: distractor alone, no breath.
    let det_a = run_one(0.0, distractor_amp, cell_seed);
    for (i, &v) in det_a.iter().enumerate() {
        let abs_idx = i as u64;
        if abs_idx < warmup_samples {
            continue;
        }
        fp_total += 1;
        if v > 0.5 {
            fp_active += 1;
        }
    }

    // ---- Mode B: breath + distractor.
    let det_b = run_one(breath_amp, distractor_amp, cell_seed);
    let mut tp_active: u64 = 0;
    let mut tp_total: u64 = 0;
    for (i, &v) in det_b.iter().enumerate() {
        let abs_idx = i as u64;
        if abs_idx < warmup_samples {
            continue;
        }
        let into_period = abs_idx % period_samples;
        if into_period < breath_samples && into_period >= in_breath_skip {
            tp_total += 1;
            if v > 0.5 {
                tp_active += 1;
            }
        } else if into_period >= breath_samples + out_breath_skip {
            fp_total += 1;
            if v > 0.5 {
                fp_active += 1;
            }
        }
    }

    let false_positive_frac = if fp_total > 0 {
        fp_active as f32 / fp_total as f32
    } else {
        0.0
    };
    let active_in_breath = if tp_total > 0 {
        tp_active as f32 / tp_total as f32
    } else {
        0.0
    };

    let asserted_active = breath_amp >= distractor_amp;
    let fp_ok = false_positive_frac <= 0.05;
    let active_ok = !asserted_active || active_in_breath >= 0.80;
    let pass = fp_ok && active_ok;

    CellResult {
        breath_amp,
        distractor_amp,
        false_positive_frac,
        active_in_breath,
        pass,
        asserted_active,
    }
}

#[test]
fn breath_distractor_sweep() {
    let tmpdir = env!("CARGO_TARGET_TMPDIR");
    println!("Artifacts will be written to: {tmpdir}");

    let mut results: Vec<CellResult> = Vec::new();
    for (bi, &ba) in BREATH_AMPS.iter().enumerate() {
        for (di, &da) in DISTRACTOR_AMPS.iter().enumerate() {
            results.push(run_cell(ba, da, bi, di));
        }
    }

    let csv_path = format!("{tmpdir}/breath_distractor_sweep.csv");
    let mut csv =
        String::from("breath_amp,distractor_amp,false_positive_frac,active_in_breath,pass\n");
    for r in &results {
        csv.push_str(&format!(
            "{:.3},{:.3},{:.4},{:.4},{}\n",
            r.breath_amp, r.distractor_amp, r.false_positive_frac, r.active_in_breath, r.pass
        ));
    }
    std::fs::write(&csv_path, &csv).expect("writing CSV artifact");
    println!("CSV written to: {csv_path}");

    let mut grid = String::from(" breath↓ tone→ |");
    for &da in DISTRACTOR_AMPS {
        grid.push_str(&format!(" {da:>5.2} |"));
    }
    grid.push('\n');
    for &ba in BREATH_AMPS {
        grid.push_str(&format!("         {ba:>5.2} |"));
        for &da in DISTRACTOR_AMPS {
            let r = results
                .iter()
                .find(|r| r.breath_amp == ba && r.distractor_amp == da)
                .unwrap();
            let mark = if r.pass {
                if r.asserted_active {
                    "  ok  "
                } else {
                    " miss "
                }
            } else {
                " FAIL "
            };
            grid.push_str(&format!(" {mark} |"));
        }
        grid.push('\n');
    }
    let grid_path = format!("{tmpdir}/breath_distractor_sweep_grid.txt");
    std::fs::write(&grid_path, &grid).expect("writing grid artifact");
    println!("Grid written to: {grid_path}");
    println!("\n{grid}");

    let failures: Vec<&CellResult> = results.iter().filter(|r| !r.pass).collect();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!(
                "FAIL breath_amp={:.2} distractor_amp={:.2} fp_frac={:.4} in_breath={:.4}",
                f.breath_amp, f.distractor_amp, f.false_positive_frac, f.active_in_breath
            );
        }
        panic!("{} cells failed", failures.len());
    }
}
