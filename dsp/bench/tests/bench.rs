//! Bench-driven tests: mount a world, drive its in-ports, assert in Rust.
//!
//! The real-voice case is `#[ignore]` because it depends on a WAV under
//! `dsp/bench/test_data/` that is not checked in (license + size). Run it with:
//!
//!     cargo test -p dsp-bench --release --test bench -- --ignored --nocapture
//!
//! Drop a 48 kHz mono WAV named `sa-re-ga-ma-pa.wav` into `dsp/bench/test_data/`.

use dsp_bench::{Bench, Captured, Run, Source};
use std::path::PathBuf;

fn test_data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_data")
        .join(name)
}

/// Inline throwaway world: a single GainNode at unity (0 dB) should pass a
/// constant through unchanged. Proves `Bench::new` + `bind` + `capture_out`.
#[test]
fn gain_unity_passes_constant_through() {
    let out: Captured = Bench::new(
        r#"{
            "world_version": 1,
            "in_ports": [{ "id": "sig" }],
            "out_ports": [{ "id": "loud" }],
            "nodes": [
                { "id": "g", "type": "GainNode", "params": { "gain_db": 0.0 } }
            ],
            "connections": [
                { "from": "sig", "to": "g.audio_in" },
                { "from": "g.audio_out", "to": "loud" }
            ]
        }"#,
    )
    .bind("sig", Source::constant(0.5))
    .capture_out(["loud"])
    .run(Run::blocks(2));

    let loud = out.out("loud");
    assert!(!loud.is_empty(), "captured no output");
    for (i, &v) in loud.iter().enumerate() {
        assert!(
            (v - 0.5).abs() < 1e-6,
            "frame {i}: unity gain changed 0.5 -> {v}"
        );
    }
}

/// Sine at 440 Hz, amplitude 0.5 → RMS ≈ 0.3536 (0.5/√2). Ported from the
/// old self-asserting `worlds/test/sine.json`.
#[test]
fn sine_rms_is_amplitude_over_sqrt2() {
    let out = Bench::new(
        r#"{
            "world_version": 1, "in_ports": [], "out_ports": [],
            "nodes": [
                { "id": "src", "type": "SynthSine", "params": { "freq": 440.0, "amplitude": 0.5, "phase": 0.0 } },
                { "id": "rms", "type": "RmsMeter", "params": {} }
            ],
            "connections": [{ "from": "src.audio_out", "to": "rms.audio_in" }]
        }"#,
    )
    .capture(["rms.rms"])
    .run(Run::secs(1.0));

    Captured::assert_near_db(out.last_wire("rms.rms"), 0.3536, 0.5);
}

/// Vibrato'd sine, same RMS ≈ 0.3536 (vibrato doesn't change amplitude).
/// Ported from `worlds/test/vibrato.json`.
#[test]
fn vibrato_sine_rms_matches_amplitude() {
    let out = Bench::new(
        r#"{
            "world_version": 1, "in_ports": [], "out_ports": [],
            "nodes": [
                { "id": "src", "type": "SynthVibratoSine", "params": { "carrier_freq": 440.0, "amplitude": 0.5, "vibrato_rate": 6.0, "vibrato_depth_cents": 50.0 } },
                { "id": "rms", "type": "RmsMeter", "params": {} }
            ],
            "connections": [{ "from": "src.audio_out", "to": "rms.audio_in" }]
        }"#,
    )
    .capture(["rms.rms"])
    .run(Run::secs(1.0));

    Captured::assert_near_db(out.last_wire("rms.rms"), 0.3536, 0.1);
}

/// Tone (0.7071) + pink noise (0.0707) summed → RMS ≈ 0.50498. Ported from
/// `worlds/test/sine-plus-pink.json`.
#[test]
fn sine_plus_pink_rms() {
    let out = Bench::new(
        r#"{
            "world_version": 1, "in_ports": [], "out_ports": [],
            "nodes": [
                { "id": "tone", "type": "SynthSine", "params": { "freq": 440.0, "amplitude": 0.7071, "phase": 0.0 } },
                { "id": "noise", "type": "SynthPinkNoise", "params": { "amplitude": 0.0707, "seed": 1.0 } },
                { "id": "mix", "type": "MixSum", "params": { "channels": 2.0 } },
                { "id": "rms", "type": "RmsMeter", "params": {} }
            ],
            "connections": [
                { "from": "tone.audio_out", "to": "mix.in_0" },
                { "from": "noise.audio_out", "to": "mix.in_1" },
                { "from": "mix.out", "to": "rms.audio_in" }
            ]
        }"#,
    )
    .capture(["rms.rms"])
    .run(Run::secs(1.0));

    Captured::assert_near_db(out.last_wire("rms.rms"), 0.50498, 0.5);
}

/// The live practice world, unmodified, run on a real recording. Reports the
/// same characterization metrics the old node-level `real_voice` test did, but
/// now end-to-end through `coach.json` via the engine — and asserts a loose
/// sanity floor instead of merely printing.
#[test]
#[ignore]
fn coach_world_tracks_real_voice() {
    let wav = test_data("sa-re-ga-ma-pa.wav");
    assert!(
        wav.exists(),
        "missing {} — drop a 48 kHz mono WAV there and rerun",
        wav.display()
    );

    let out = Bench::mount("../worlds/coach.json")
        .bind("mic", Source::wav(&wav))
        .capture(["pitch_yin.f0"])
        .run(Run::secs(6.0));

    let coverage = out.coverage_voiced("pitch_yin.f0");
    let jitter = out.median_jitter_cents("pitch_yin.f0");
    let jumps = out.octave_jumps("pitch_yin.f0");

    eprintln!("coverage_voiced = {coverage:.3}");
    eprintln!("median_jitter_cents = {jitter:.1}");
    eprintln!("octave_jumps = {jumps}");

    // Loose sanity floor — a sung sargam should be voiced most of the time and
    // not octave-jump on every other hop. Tighten as the gate/cleanup lands.
    assert!(coverage > 0.4, "voiced coverage too low: {coverage:.3}");
}
