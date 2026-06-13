//! Audio-trace recorder for one live coach session.
//!
//! When the env var `GURUKUL_AUDIO_TRACE_DIR` is set, records:
//! - a float32 mono WAV of every 512-sample block fed to the engine
//! - a `.features.jsonl` sidecar with one hop-feature JSON line per block
//! - a `.manifest.json` after the session ends cleanly
//!
//! The recorder is deliberately non-blocking: `record_block` / `record_hop`
//! use `try_send` on a bounded channel. On channel-full the run is marked
//! invalid and the data is dropped rather than stalling the worker.

use audio_trace_format::{Manifest, SidecarHop};
use domain_ports::tel_warn;
use domain_ports::telemetry::Telemetry;
use hound::{SampleFormat, WavSpec, WavWriter};
use sha2::{Digest, Sha256};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::{env, fs};

/// Bounded channel capacity. Large enough to absorb disk hiccups, small
/// enough to bound memory (~256 * (512 * 4 + overhead) ≈ a few MB).
const CHANNEL_CAPACITY: usize = 256;

enum WriterMsg {
    Block(Vec<f32>),
    Hop(SidecarHop),
}

pub(crate) struct Recorder {
    sender: SyncSender<WriterMsg>,
    invalid: Arc<AtomicBool>,
    writer_thread: Option<JoinHandle<()>>,
    /// Fields needed to write the manifest after joining the writer thread.
    manifest_meta: ManifestMeta,
    /// Number of hops sent so far — counted on the worker side so `finish`
    /// knows `n_hops` without re-reading the file.
    n_hops_sent: usize,
}

struct ManifestMeta {
    dir: PathBuf,
    stamp: u64,
    sample_rate: u32,
    world_name: String,
    world_sha256: String,
}

impl Recorder {
    /// Returns `Some` only if `GURUKUL_AUDIO_TRACE_DIR` is set and non-empty.
    ///
    /// `stamp_ms` must come from the worker's `clock.now_ms()` — this function
    /// does NOT read a wall clock. `world_json` is the embedded world bytes
    /// (for SHA-256). `world_name` is the logical basename (e.g. `"coach.json"`).
    pub(crate) fn from_env(
        sample_rate: u32,
        stamp_ms: u64,
        world_json: &str,
        world_name: &str,
    ) -> Option<Self> {
        let dir_str = env::var("GURUKUL_AUDIO_TRACE_DIR").ok()?;
        if dir_str.is_empty() {
            return None;
        }
        let dir = PathBuf::from(&dir_str);
        if let Err(e) = fs::create_dir_all(&dir) {
            // Best-effort: if we can't create the dir, silently skip recording.
            // The worker must not fail to start just because of a bad trace dir.
            eprintln!("audio-trace: could not create dir {dir_str}: {e}");
            return None;
        }

        // Compute SHA-256 of the world JSON.
        let mut hasher = Sha256::new();
        hasher.update(world_json.as_bytes());
        let world_sha256 = format!("{:x}", hasher.finalize());

        let wav_path = dir.join(format!("{stamp_ms}-engine-input.wav"));
        let sidecar_path = dir.join(format!("{stamp_ms}-engine-input.features.jsonl"));

        let wav_spec = WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };

        let invalid = Arc::new(AtomicBool::new(false));
        let invalid_for_thread = Arc::clone(&invalid);

        let wav_writer = match WavWriter::create(&wav_path, wav_spec) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("audio-trace: could not create WAV {wav_path:?}: {e}");
                return None;
            }
        };
        let sidecar_file = match fs::File::create(&sidecar_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("audio-trace: could not create sidecar {sidecar_path:?}: {e}");
                return None;
            }
        };

        let (sender, receiver) = sync_channel::<WriterMsg>(CHANNEL_CAPACITY);

        let writer_thread = match thread::Builder::new()
            .name("audio-trace-writer".into())
            .spawn(move || {
                let mut wav = wav_writer;
                let mut sidecar = BufWriter::new(sidecar_file);

                loop {
                    match receiver.recv() {
                        Ok(WriterMsg::Block(samples)) => {
                            // Once the run is invalid, stop touching the WAV:
                            // further writes would only spam stderr and waste
                            // work on an artifact that will get no manifest.
                            if invalid_for_thread.load(Ordering::Acquire) {
                                continue;
                            }
                            for s in samples {
                                if let Err(e) = wav.write_sample(s) {
                                    eprintln!("audio-trace: WAV write error: {e}");
                                    invalid_for_thread.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                        Ok(WriterMsg::Hop(hop)) => {
                            if invalid_for_thread.load(Ordering::Acquire) {
                                continue;
                            }
                            let line = match serde_json::to_string(&hop) {
                                Ok(l) => l,
                                Err(e) => {
                                    eprintln!("audio-trace: sidecar serialize error: {e}");
                                    invalid_for_thread.store(true, Ordering::Release);
                                    continue;
                                }
                            };
                            if let Err(e) = writeln!(sidecar, "{line}") {
                                eprintln!("audio-trace: sidecar write error: {e}");
                                invalid_for_thread.store(true, Ordering::Release);
                            }
                        }
                        Err(_) => {
                            // Channel closed — sender was dropped in finish()
                            // (or on Drop). Flush sidecar, finalize WAV.
                            if let Err(e) = sidecar.flush() {
                                eprintln!("audio-trace: sidecar flush error: {e}");
                                invalid_for_thread.store(true, Ordering::Release);
                            }
                            if let Err(e) = wav.finalize() {
                                eprintln!("audio-trace: WAV finalize error: {e}");
                                invalid_for_thread.store(true, Ordering::Release);
                            }
                            break;
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => {
                // A recorder must never take down the worker. If the writer
                // thread can't spawn, skip recording entirely.
                eprintln!("audio-trace: could not spawn writer thread: {e}");
                return None;
            }
        };

        Some(Self {
            sender,
            invalid,
            writer_thread: Some(writer_thread),
            manifest_meta: ManifestMeta {
                dir,
                stamp: stamp_ms,
                sample_rate,
                world_name: world_name.to_string(),
                world_sha256,
            },
            n_hops_sent: 0,
        })
    }

    /// Push one consumed block. Non-blocking: drops and marks invalid on
    /// channel-full rather than stalling the worker.
    pub(crate) fn record_block(&mut self, block: &[f32]) {
        match self.sender.try_send(WriterMsg::Block(block.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.invalid.store(true, Ordering::Release);
            }
            Err(TrySendError::Disconnected(_)) => {
                // Writer thread died; mark invalid so finish skips the manifest.
                self.invalid.store(true, Ordering::Release);
            }
        }
    }

    /// Push one hop's features. Same non-blocking contract.
    pub(crate) fn record_hop(&mut self, hop: SidecarHop) {
        match self.sender.try_send(WriterMsg::Hop(hop)) {
            // Count only accepted hops, so `n_hops_sent` can never exceed the
            // sidecar line count. (The invalid flag already gates the manifest,
            // but this removes the footgun for any future reader.)
            Ok(()) => self.n_hops_sent += 1,
            Err(TrySendError::Full(_)) => {
                self.invalid.store(true, Ordering::Release);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.invalid.store(true, Ordering::Release);
            }
        }
    }

    /// Drop the sender (signalling the writer thread to finalize) and join it.
    /// Idempotent: a no-op once the handle has been taken. Shared by `finish`
    /// and `Drop`.
    fn join_writer(&mut self) {
        if let Some(handle) = self.writer_thread.take() {
            // Replace the live sender with a fresh, immediately-dropped one so
            // the writer's recv() sees the channel close. `finish` takes `self`
            // by value, so this also covers the panic/early-drop path.
            let (dead, _) = sync_channel::<WriterMsg>(1);
            let live = std::mem::replace(&mut self.sender, dead);
            drop(live);
            // A writer-thread panic means the WAV/sidecar may be half-written
            // and was never finalized — invalidate so no manifest is emitted.
            if handle.join().is_err() {
                self.invalid.store(true, Ordering::Release);
            }
        }
    }

    /// Finish recording. Drops the sender (signals writer thread to finalize),
    /// joins the writer thread, then writes the manifest if the run was valid.
    pub(crate) fn finish(mut self, telemetry: &dyn Telemetry) {
        self.join_writer();

        if self.invalid.load(Ordering::Acquire) {
            tel_warn!(
                telemetry,
                "audio-trace: recording invalidated (backpressure or writer I/O error); manifest omitted",
            );
            return;
        }

        let meta = &self.manifest_meta;
        let n_hops = self.n_hops_sent;
        let wav_basename = format!("{}-engine-input.wav", meta.stamp);
        let manifest_path = meta
            .dir
            .join(format!("{}-engine-input.manifest.json", meta.stamp));

        let manifest = Manifest {
            schema: 1,
            world: meta.world_name.clone(),
            world_sha256: meta.world_sha256.clone(),
            sample_rate: meta.sample_rate,
            block_size: 512,
            channels: 1,
            total_samples: n_hops * 512,
            n_hops,
            source_wav: wav_basename,
            recorder_version: env!("CARGO_PKG_VERSION").to_string(),
        };

        match serde_json::to_string_pretty(&manifest) {
            Ok(json) => {
                if let Err(e) = fs::write(&manifest_path, json) {
                    tel_warn!(
                        telemetry,
                        "audio-trace: failed to write manifest",
                        error = e.to_string(),
                    );
                }
            }
            Err(e) => {
                tel_warn!(
                    telemetry,
                    "audio-trace: failed to serialize manifest",
                    error = e.to_string(),
                );
            }
        }
    }
}

impl Drop for Recorder {
    /// If `finish` was never called (e.g. the worker panicked between
    /// `from_env` and `finish`), still join the writer thread rather than
    /// detaching it. No manifest is written — a recording without a manifest
    /// is fail-closed: the replay tooling refuses it.
    fn drop(&mut self) {
        self.join_writer();
    }
}

/// Shared lock for any test that mutates the process-global
/// `GURUKUL_AUDIO_TRACE_DIR` env var. Lives at module scope (not inside
/// `mod tests`) so the data-plane worker test in `data_plane.rs` can serialize
/// against these tests through the SAME mutex — two separate mutexes wouldn't.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_trace_format::{Manifest, SidecarHop};

    /// Telemetry stub that ignores everything (shared across tests).
    struct NullTelemetry;
    impl Telemetry for NullTelemetry {
        fn log(
            &self,
            _level: domain_ports::telemetry::Level,
            _msg: &str,
            _fields: &domain_ports::telemetry::Fields,
        ) {
        }
        fn child(&self, _fields: domain_ports::telemetry::Fields) -> std::sync::Arc<dyn Telemetry> {
            std::sync::Arc::new(NullTelemetry)
        }
        fn event(&self, _e: &domain_ports::telemetry::Event) {}
    }

    // Test 1: Recorder off by default
    #[test]
    fn recorder_off_by_default() {
        let _guard = super::test_env_lock();
        // Ensure env var is not set.
        // (In CI/dev it shouldn't be; we remove it to be safe.)
        env::remove_var("GURUKUL_AUDIO_TRACE_DIR");
        let recorder = Recorder::from_env(48_000, 0, "{}", "coach.json");
        assert!(
            recorder.is_none(),
            "from_env must return None when env var is unset"
        );
    }

    // Test 2: Records a known signal
    #[test]
    fn records_a_known_signal() {
        use std::io::BufRead;

        let _guard = super::test_env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().to_str().unwrap().to_string();

        env::set_var("GURUKUL_AUDIO_TRACE_DIR", &dir_path);

        let stamp_ms: u64 = 12345;
        let world_json = r#"{"nodes":[],"edges":[]}"#;
        let world_name = "coach.json";
        let sample_rate: u32 = 48_000;
        const N_HOPS: usize = 4;
        const BLOCK: usize = 512;

        let mut recorder = Recorder::from_env(sample_rate, stamp_ms, world_json, world_name)
            .expect("recorder must be Some when env var is set");

        // Generate a simple sine-ish block.
        let block: Vec<f32> = (0..BLOCK)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect();

        for hop in 0..N_HOPS as u64 {
            recorder.record_block(&block);
            recorder.record_hop(SidecarHop {
                hop,
                f0_hz: 440.0,
                confidence: 0.9,
                onset: 0.0,
                breath: 0.0,
                vibrato_rate: 0.0,
                vibrato_depth: 0.0,
            });
        }

        recorder.finish(&NullTelemetry);

        env::remove_var("GURUKUL_AUDIO_TRACE_DIR");

        // Check WAV exists and has N_HOPS * BLOCK samples.
        let wav_path = dir.path().join(format!("{stamp_ms}-engine-input.wav"));
        assert!(wav_path.exists(), "WAV file must exist");
        let mut wav_reader = hound::WavReader::open(&wav_path).expect("open WAV");
        let samples: Vec<f32> = wav_reader
            .samples::<f32>()
            .map(|s| s.expect("sample"))
            .collect();
        assert_eq!(
            samples.len(),
            N_HOPS * BLOCK,
            "WAV must have n_hops * block_size samples"
        );

        // Check sidecar has N_HOPS lines each parseable as SidecarHop.
        let sidecar_path = dir
            .path()
            .join(format!("{stamp_ms}-engine-input.features.jsonl"));
        assert!(sidecar_path.exists(), "sidecar must exist");
        let sidecar_file = fs::File::open(&sidecar_path).expect("open sidecar");
        let lines: Vec<String> = std::io::BufReader::new(sidecar_file)
            .lines()
            .map(|l| l.expect("line"))
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), N_HOPS, "sidecar must have N_HOPS lines");
        for line in &lines {
            let _hop: SidecarHop = serde_json::from_str(line).expect("sidecar line must parse");
        }

        // Check manifest.
        let manifest_path = dir
            .path()
            .join(format!("{stamp_ms}-engine-input.manifest.json"));
        assert!(manifest_path.exists(), "manifest must exist");
        let manifest_str = fs::read_to_string(&manifest_path).expect("read manifest");
        let manifest: Manifest = serde_json::from_str(&manifest_str).expect("manifest must parse");
        assert_eq!(manifest.schema, 1);
        assert_eq!(manifest.block_size, 512);
        assert_eq!(manifest.channels, 1);
        assert_eq!(manifest.n_hops, N_HOPS);
        assert_eq!(manifest.total_samples, N_HOPS * BLOCK);
        assert_eq!(
            manifest.world_sha256.len(),
            64,
            "SHA-256 hex must be 64 chars"
        );
    }

    // Test 3: Re-deserializes with the shared crate (round-trip)
    #[test]
    fn round_trips_with_shared_crate() {
        use std::io::BufRead;

        let _guard = super::test_env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().to_str().unwrap().to_string();

        env::set_var("GURUKUL_AUDIO_TRACE_DIR", &dir_path);

        let stamp_ms: u64 = 99999;
        let world_json = r#"{"nodes":[],"edges":[]}"#;
        let sample_rate: u32 = 48_000;
        const N_HOPS: usize = 2;
        const BLOCK: usize = 512;

        let mut recorder = Recorder::from_env(sample_rate, stamp_ms, world_json, "coach.json")
            .expect("recorder must be Some");

        let block = vec![0.0_f32; BLOCK];
        for hop in 0..N_HOPS as u64 {
            recorder.record_block(&block);
            recorder.record_hop(SidecarHop {
                hop,
                f0_hz: 220.0,
                confidence: 0.8,
                onset: 0.1,
                breath: 0.05,
                vibrato_rate: 5.0,
                vibrato_depth: 0.01,
            });
        }

        recorder.finish(&NullTelemetry);
        env::remove_var("GURUKUL_AUDIO_TRACE_DIR");

        // Deserialize sidecar with audio_trace_format types (same as Phase 1).
        let sidecar_path = dir
            .path()
            .join(format!("{stamp_ms}-engine-input.features.jsonl"));
        let sidecar_file = fs::File::open(&sidecar_path).expect("open sidecar");
        let hops: Vec<SidecarHop> = std::io::BufReader::new(sidecar_file)
            .lines()
            .map(|l| l.expect("line"))
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<SidecarHop>(&l).expect("SidecarHop parse"))
            .collect();
        assert_eq!(hops.len(), N_HOPS);
        assert!((hops[0].f0_hz - 220.0).abs() < f32::EPSILON);

        // Deserialize manifest with audio_trace_format::Manifest.
        let manifest_path = dir
            .path()
            .join(format!("{stamp_ms}-engine-input.manifest.json"));
        let manifest_str = fs::read_to_string(&manifest_path).expect("read manifest");
        let manifest: Manifest =
            serde_json::from_str(&manifest_str).expect("Manifest round-trip parse");
        assert_eq!(manifest.n_hops, N_HOPS);
        assert_eq!(manifest.schema, 1);
    }

    // Test 4: Fail-closed — an invalidated run writes NO manifest.
    // The Heisen guard's whole purpose: a recording marked invalid (by
    // backpressure or a writer I/O error) must not produce a manifest, so
    // replay tooling refuses the partial artifact. We trip the flag directly
    // (rather than racing the writer to force real backpressure) so the test
    // is deterministic.
    #[test]
    fn invalid_run_omits_manifest() {
        let _guard = super::test_env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        env::set_var("GURUKUL_AUDIO_TRACE_DIR", dir.path().to_str().unwrap());

        let stamp_ms: u64 = 7;
        let mut recorder = Recorder::from_env(48_000, stamp_ms, "{}", "coach.json")
            .expect("recorder must be Some");

        let block = vec![0.0_f32; 512];
        recorder.record_block(&block);
        recorder.record_hop(SidecarHop {
            hop: 0,
            f0_hz: 0.0,
            confidence: 0.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
        });

        // Simulate a mid-run invalidation (what try_send-full or a writer I/O
        // error would do).
        recorder.invalid.store(true, Ordering::Release);
        recorder.finish(&NullTelemetry);
        env::remove_var("GURUKUL_AUDIO_TRACE_DIR");

        let manifest_path = dir
            .path()
            .join(format!("{stamp_ms}-engine-input.manifest.json"));
        assert!(
            !manifest_path.exists(),
            "an invalidated run must NOT write a manifest (fail-closed)"
        );
    }
}
