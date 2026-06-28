//! Sidecar/manifest format for headless audio replay, plus shared pitch metrics.
//!
//! This module owns the durable artifact format for the audio-trace system:
//!
//! - [`SidecarHop`] — one JSON line per engine hop in a `.features.jsonl` file.
//! - [`Manifest`] — a single `.manifest.json` pinning the run configuration.
//! - [`replay_samples`] — the testable core: runs samples through the coach
//!   pitch engine and produces `(Vec<SidecarHop>, Manifest)` with no file I/O.
//! - [`count_octave_jumps`], [`median_jitter_cents_of`], [`coverage_voiced_of`]
//!   — shared metric free functions called by both `Captured` (lib) and
//!   `cmd_diff_features` (bin).

use anyhow::Result;
use std::path::Path;

pub use audio_trace_format::{Manifest, SidecarHop};

// ── Shared metrics ────────────────────────────────────────────────────────────

/// Count voiced-to-voiced hop transitions whose pitch jumps more than
/// `threshold_cents`. The canonical octave-error metric, shared by the bench
/// `Captured` helper and the sidecar diff.
pub fn count_octave_jumps(hops_hz: &[f32], threshold_cents: f32) -> usize {
    let mut prev: Option<f32> = None;
    let mut jumps = 0;
    for &hz in hops_hz {
        let voiced = hz.is_finite() && hz > 0.0;
        if let (Some(p), true) = (prev, voiced)
            && (1200.0 * (hz / p).log2()).abs() > threshold_cents
        {
            jumps += 1;
        }
        prev = if voiced { Some(hz) } else { None };
    }
    jumps
}

/// Median absolute frame-to-frame jitter in cents within voiced runs.
/// Returns `0.0` when there are fewer than two voiced consecutive hops.
pub fn median_jitter_cents_of(hops_hz: &[f32]) -> f32 {
    let mut deltas: Vec<f32> = Vec::new();
    let mut prev: Option<f32> = None;
    for &hz in hops_hz {
        let voiced = hz.is_finite() && hz > 0.0;
        if let (Some(p), true) = (prev, voiced) {
            deltas.push((1200.0 * (hz / p).log2()).abs());
        }
        prev = if voiced { Some(hz) } else { None };
    }
    percentile_f32(&mut deltas, 50.0)
}

/// Fraction of hops that are voiced (finite and > 0).
pub fn coverage_voiced_of(hops_hz: &[f32]) -> f32 {
    if hops_hz.is_empty() {
        return 0.0;
    }
    let voiced = hops_hz
        .iter()
        .filter(|&&hz| hz.is_finite() && hz > 0.0)
        .count();
    voiced as f32 / hops_hz.len() as f32
}

fn percentile_f32(samples: &mut [f32], p: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((p / 100.0) * (samples.len() - 1) as f32).round() as usize;
    samples[idx.min(samples.len() - 1)]
}

// ── Core replay ───────────────────────────────────────────────────────────────

/// Run `samples` (already truncated to a whole number of blocks) through the
/// world's pitch engine and produce the sidecar hops + manifest. No file I/O —
/// `cmd_replay_audio` does the WAV-read and file-write around this.
///
/// `samples.len()` **must** be a multiple of `block_size`. The caller is
/// responsible for floor-dividing and truncating before calling here.
pub fn replay_samples(
    samples: &[f32],
    sample_rate: u32,
    block_size: usize,
    world_path: &Path,
    world_sha256: &str,
    source_wav: &str,
) -> Result<(Vec<SidecarHop>, Manifest)> {
    anyhow::ensure!(block_size > 0, "block_size must be > 0");
    anyhow::ensure!(
        samples.len().is_multiple_of(block_size),
        "samples.len() ({}) must be a multiple of block_size ({})",
        samples.len(),
        block_size
    );

    let n_hops = samples.len() / block_size;
    let total_samples = n_hops * block_size;

    // Mount the world and capture all six boundary out-ports.
    // Source::Samples with exactly n_hops*block_size samples passes through
    // materialise() unchanged (no zero-padding occurs since len == total).
    let captured = crate::Bench::mount(world_path)
        .sample_rate(sample_rate)
        .block_size(block_size)
        .bind("mic", crate::Source::samples(samples.to_vec()))
        .capture_out([
            "pitch",
            "confidence",
            "onset",
            "breath",
            "vibrato_rate",
            "vibrato_amplitude",
            "vibrato_phase",
        ])
        .run(crate::Run::blocks(n_hops as u64));

    // Read sample-0 of each block (CRITICAL — not per_hop() which reads last).
    let pitch_buf = captured.out("pitch");
    let conf_buf = captured.out("confidence");
    let onset_buf = captured.out("onset");
    let breath_buf = captured.out("breath");
    let vrate_buf = captured.out("vibrato_rate");
    let vamp_buf = captured.out("vibrato_amplitude");
    let vphase_buf = captured.out("vibrato_phase");

    let mut hops: Vec<SidecarHop> = Vec::with_capacity(n_hops);
    for i in 0..n_hops {
        let s0 = i * block_size; // index of sample-0 for hop i
        hops.push(SidecarHop {
            hop: i as u64,
            f0_hz: pitch_buf[s0],
            confidence: conf_buf[s0],
            onset: onset_buf[s0],
            breath: breath_buf[s0],
            vibrato_rate: vrate_buf[s0],
            vibrato_amplitude: vamp_buf[s0],
            vibrato_phase: vphase_buf[s0],
        });
    }

    let world_name = world_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| world_path.to_string_lossy().into_owned());

    let manifest = Manifest {
        schema: 2,
        world: world_name,
        world_sha256: world_sha256.to_string(),
        sample_rate,
        block_size,
        channels: 1,
        total_samples,
        n_hops,
        source_wav: source_wav.to_string(),
        recorder_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    Ok((hops, manifest))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. Sidecar round-trip ─────────────────────────────────────────────────

    #[test]
    fn sidecar_hop_round_trips() {
        let hop = SidecarHop {
            hop: 42,
            f0_hz: 220.0,
            confidence: 0.9,
            onset: 0.0,
            breath: 1.0,
            vibrato_rate: 5.5,
            vibrato_amplitude: 0.02,
            vibrato_phase: 0.0,
        };
        let json = serde_json::to_string(&hop).unwrap();
        let back: SidecarHop = serde_json::from_str(&json).unwrap();
        assert_eq!(hop, back);
    }

    // ── 2. Manifest round-trip ────────────────────────────────────────────────

    #[test]
    fn manifest_round_trips() {
        let m = Manifest {
            schema: 2,
            world: "coach.json".to_string(),
            world_sha256: "deadbeef".to_string(),
            sample_rate: 48000,
            block_size: 512,
            channels: 1,
            total_samples: 376320,
            n_hops: 735,
            source_wav: "sa-re-ga-ma-pa.wav".to_string(),
            recorder_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string_pretty(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    // ── 3. count_octave_jumps agrees with Captured::octave_jumps ─────────────

    #[test]
    fn count_octave_jumps_known_series() {
        // [220, 220, 440, 220]: 220→220 = 0 cents (no jump),
        //                       220→440 = 1200 cents (jump),
        //                       440→220 = -1200 cents (jump) → 2 total
        let hz = [220.0_f32, 220.0, 440.0, 220.0];
        assert_eq!(count_octave_jumps(&hz, 600.0), 2);
    }

    #[test]
    fn count_octave_jumps_no_jumps() {
        // Small pitch variation — no octave jumps.
        let hz = [220.0_f32, 221.0, 222.0, 221.0];
        assert_eq!(count_octave_jumps(&hz, 600.0), 0);
    }

    #[test]
    fn count_octave_jumps_unvoiced_breaks_chain() {
        // Unvoiced (0.0) between voiced hops resets the chain; not a jump.
        let hz = [220.0_f32, 0.0, 440.0];
        assert_eq!(count_octave_jumps(&hz, 600.0), 0);
    }

    // ── 4. Partial-block policy ───────────────────────────────────────────────

    #[test]
    fn partial_block_policy() {
        // 2048 + 200 = 2248 samples; floor(2248/512) = 4 hops, 2248 % 512 = 200 dropped.
        let raw_len: usize = 2048 + 200;
        let block_size: usize = 512;
        let expected_hops = raw_len / block_size; // 4
        assert_eq!(expected_hops, 4);

        let total = expected_hops * block_size; // 2048
        assert_eq!(total, 2048);

        // Build a 440 Hz sine and truncate to total.
        let sample_rate: u32 = 48000;
        let mut samples: Vec<f32> = (0..raw_len)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect();
        // Floor-divide and truncate exactly as cmd_replay_audio does.
        let n_hops = samples.len() / block_size;
        samples.truncate(n_hops * block_size);

        assert_eq!(samples.len(), total, "truncated length wrong");
        assert_eq!(n_hops, expected_hops, "n_hops wrong");

        let world_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../worlds/coach.json");

        let (hops, manifest) = replay_samples(
            &samples,
            sample_rate,
            block_size,
            &world_path,
            "fakehash",
            "test.wav",
        )
        .unwrap();

        assert_eq!(hops.len(), expected_hops, "sidecar hop count wrong");
        assert_eq!(manifest.n_hops, expected_hops, "manifest n_hops wrong");
        assert_eq!(
            manifest.total_samples,
            expected_hops * block_size,
            "manifest total_samples wrong"
        );
        // Verify the tail was NOT included as an extra hop.
        assert!(hops.len() < raw_len / block_size + 1, "extra hop from tail");
    }

    // ── 5. End-to-end on the existing fixture ─────────────────────────────────

    #[test]
    fn end_to_end_sa_re_ga_ma_pa() {
        let wav_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_data/sa-re-ga-ma-pa.wav");
        let world_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../worlds/coach.json");

        let sample_rate: u32 = 48000;
        let block_size: usize = 512;

        // Read using the pub-exported reader.
        let mut samples = crate::read_wav_mono(&wav_path, sample_rate).unwrap();
        let n_hops = samples.len() / block_size;
        samples.truncate(n_hops * block_size);

        // Fixture: 376768 frames / 512 = 735 hops (remainder 448 dropped).
        assert_eq!(n_hops, 735, "expected 735 hops for sa-re-ga-ma-pa.wav");

        let (hops, manifest) = replay_samples(
            &samples,
            sample_rate,
            block_size,
            &world_path,
            "fakehash",
            "sa-re-ga-ma-pa.wav",
        )
        .unwrap();

        assert_eq!(hops.len(), 735, "sidecar must have 735 hops");
        assert_eq!(manifest.n_hops, 735);
        assert_eq!(manifest.total_samples, 735 * 512);

        let f0s: Vec<f32> = hops.iter().map(|h| h.f0_hz).collect();
        let voiced = coverage_voiced_of(&f0s);
        assert!(
            voiced > 0.3,
            "voiced coverage {voiced:.3} too low for a sung sargam"
        );
    }
}
