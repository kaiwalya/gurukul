//! The durable audio-trace artifact schema; owned here, consumed by dsp-bench
//! replay/diff and written by the app-coach recorder.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One hop's feature snapshot written to `.features.jsonl`.
///
/// Field order matches the JSONL line format exactly. `t_ms` is deliberately
/// absent — it is wall-clock time, varies each run, and is excluded from diffs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarHop {
    pub hop: u64,
    pub f0_hz: f32,
    pub confidence: f32,
    pub onset: f32,
    pub breath: f32,
    pub vibrato_rate: f32,
    pub vibrato_depth: f32,
}

/// Run-configuration manifest written to `.manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version. `1` for this schema; bump only on a breaking change.
    pub schema: u32,
    /// World filename (basename of the mounted path).
    pub world: String,
    /// Hex SHA-256 of the world JSON file bytes.
    pub world_sha256: String,
    pub sample_rate: u32,
    pub block_size: usize,
    /// Always 1 — the sidecar/WAV is mono engine-input.
    pub channels: u32,
    /// Samples actually fed to the engine = `n_hops * block_size`.
    pub total_samples: usize,
    pub n_hops: usize,
    /// Basename of the source WAV.
    pub source_wav: String,
    /// Crate version of the writer at build time.
    pub recorder_version: String,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Append `.wav` to a session prefix path.
pub fn wav_path(prefix: &Path) -> PathBuf {
    suffix_path(prefix, ".wav")
}

/// Append `.features.jsonl` to a session prefix path.
pub fn features_path(prefix: &Path) -> PathBuf {
    suffix_path(prefix, ".features.jsonl")
}

/// Append `.manifest.json` to a session prefix path.
pub fn manifest_path(prefix: &Path) -> PathBuf {
    suffix_path(prefix, ".manifest.json")
}

fn suffix_path(prefix: &Path, ext: &str) -> PathBuf {
    let mut s = prefix.as_os_str().to_os_string();
    s.push(ext);
    PathBuf::from(s)
}
