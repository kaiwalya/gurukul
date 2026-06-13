//! The DSP bench: mount a world, drive its boundary in-ports from a source,
//! capture internal wires and boundary out-ports, then assert against the
//! captured samples.
//!
//! This is the cabinet for offline testing. The engine is mounted from a
//! world (by path via [`Bench::mount`] or inline JSON via [`Bench::new`]); the
//! cabinet drives input wires from a [`Source`] (a recorded file, a generator
//! world, or a constant) and records the wires named via [`Bench::capture`] /
//! [`Bench::capture_out`]. Expectations are ordinary Rust `assert!`s over the
//! returned [`Captured`] — there is no expectation DSL and no in-graph assert
//! node. `cargo test --release` is the runner.
//!
//! ```no_run
//! use dsp_bench::{Bench, Source, Run};
//! let out = Bench::mount("dsp/worlds/coach.json")
//!     .bind("mic", Source::wav("sa-re-ga-ma-pa.wav"))
//!     .capture(["pitch_yin.f0"])
//!     .run(Run::secs(4.0));
//! assert!(out.coverage_voiced("pitch_yin.f0") > 0.5);
//! ```

pub mod audio_trace;

use anyhow::{Context, Result};
use engine::{Engine, NodeRegistry, World};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Build the registry of every node type the bench knows about. Shared by the
/// CLI and the test harness so both see the same node set.
pub fn build_registry() -> NodeRegistry {
    let mut registry = NodeRegistry::new();
    node_synth_sine::register(&mut registry);
    node_synth_vibrato_sine::register(&mut registry);
    node_synth_pink_noise::register(&mut registry);
    node_mix_sum::register(&mut registry);
    node_rms_meter::register(&mut registry);
    node_gain::register(&mut registry);
    node_passthrough::register(&mut registry);
    node_null_sink::register(&mut registry);
    node_pitch_error::register(&mut registry);
    node_pitch_yin::register(&mut registry);
    node_tracer::register(&mut registry);
    node_vibrato::register(&mut registry);
    node_synth_onsets::register(&mut registry);
    node_onset::register(&mut registry);
    node_synth_breath::register(&mut registry);
    node_breath::register(&mut registry);
    registry
}

/// How long to run the engine: a duration in audio time, or a fixed block
/// count. The source may run out of samples first (see [`Source`]).
#[derive(Clone, Copy, Debug)]
pub enum Run {
    /// Run for this many seconds of audio time (rounded up to whole blocks).
    Secs(f64),
    /// Run for this many milliseconds of audio time.
    Millis(f64),
    /// Run exactly this many blocks.
    Blocks(u64),
}

impl Run {
    pub fn secs(s: f64) -> Run {
        Run::Secs(s)
    }
    pub fn millis(ms: f64) -> Run {
        Run::Millis(ms)
    }
    pub fn blocks(n: u64) -> Run {
        Run::Blocks(n)
    }

    fn n_blocks(self, sample_rate: u32, block_size: usize) -> u64 {
        match self {
            Run::Secs(s) => {
                let samples = (s * sample_rate as f64).ceil() as u64;
                samples.div_ceil(block_size as u64)
            }
            Run::Millis(ms) => {
                let samples = (ms / 1000.0 * sample_rate as f64).ceil() as u64;
                samples.div_ceil(block_size as u64)
            }
            Run::Blocks(n) => n,
        }
    }
}

/// A source of samples for one boundary in-port. Produces a flat sample buffer
/// up front; the bench feeds it block-by-block and pads with silence once
/// exhausted.
pub enum Source {
    /// A recorded mono WAV file, resampled-by-assertion to the bench rate
    /// (the file must already be the bench's sample rate and mono).
    Wav(PathBuf),
    /// A constant value held for the whole run.
    Constant(f32),
    /// Pre-baked samples (e.g. produced by a generator world the caller ran).
    Samples(Vec<f32>),
}

impl Source {
    pub fn wav(path: impl Into<PathBuf>) -> Source {
        Source::Wav(path.into())
    }
    pub fn constant(v: f32) -> Source {
        Source::Constant(v)
    }
    pub fn samples(s: impl Into<Vec<f32>>) -> Source {
        Source::Samples(s.into())
    }

    /// Materialise `total` samples for this source at `sample_rate`. Files
    /// shorter than `total` are zero-padded; longer ones are truncated.
    fn materialise(&self, total: usize, sample_rate: u32) -> Result<Vec<f32>> {
        let mut buf = match self {
            Source::Wav(path) => read_wav_mono(path, sample_rate)?,
            Source::Constant(v) => vec![*v; total],
            Source::Samples(s) => s.clone(),
        };
        buf.resize(total, 0.0);
        Ok(buf)
    }
}

/// A mounted world ready to be configured and run.
pub struct Bench {
    world: World,
    sample_rate: u32,
    block_size: usize,
    bindings: Vec<(String, Source)>,
    peek_wires: Vec<String>,
    out_wires: Vec<String>,
}

impl Bench {
    /// Mount a world from a JSON file on disk (e.g. the live `coach.json`).
    pub fn mount(path: impl AsRef<Path>) -> Bench {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading world {}: {e}", path.display()));
        Bench::new(&raw)
    }

    /// Mount an inline world from a JSON string. For small throwaway worlds in
    /// a focused test — no file on disk.
    pub fn new(world_json: &str) -> Bench {
        let world: World =
            serde_json::from_str(world_json).unwrap_or_else(|e| panic!("parsing world JSON: {e}"));
        Bench {
            world,
            sample_rate: 48_000,
            block_size: 512,
            bindings: Vec::new(),
            peek_wires: Vec::new(),
            out_wires: Vec::new(),
        }
    }

    /// Override the sample rate (default 48000).
    pub fn sample_rate(mut self, sr: u32) -> Bench {
        self.sample_rate = sr;
        self
    }

    /// Override the block size (default 512).
    pub fn block_size(mut self, bs: usize) -> Bench {
        self.block_size = bs;
        self
    }

    /// Drive boundary in-port `id` from `source`.
    pub fn bind(mut self, id: impl Into<String>, source: Source) -> Bench {
        self.bindings.push((id.into(), source));
        self
    }

    /// Record one or more internal wires by `"<node_id>.<port>"` path (read via
    /// the engine's peek API; non-invasive).
    pub fn capture<I, S>(mut self, wires: I) -> Bench
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.peek_wires.extend(wires.into_iter().map(Into::into));
        self
    }

    /// Record one or more boundary out-ports by id.
    pub fn capture_out<I, S>(mut self, ports: I) -> Bench
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.out_wires.extend(ports.into_iter().map(Into::into));
        self
    }

    /// Build the engine, drive it, and return the captured samples. Panics on
    /// any setup error (unknown port, bad world) — these are test bugs, not
    /// conditions to handle.
    pub fn run(self, run: Run) -> Captured {
        self.try_run(run).expect("bench run")
    }

    fn try_run(self, run: Run) -> Result<Captured> {
        let registry = build_registry();
        let mut engine = Engine::build(&self.world, &registry, self.sample_rate, self.block_size)
            .context("building engine")?;

        let n_blocks = run.n_blocks(self.sample_rate, self.block_size);
        let total_samples = n_blocks as usize * self.block_size;

        // Resolve + materialise every input binding up front.
        let mut inputs: Vec<(engine::InPortHandle, Vec<f32>)> =
            Vec::with_capacity(self.bindings.len());
        for (id, source) in &self.bindings {
            let handle = engine
                .resolve_in_port(id)
                .with_context(|| format!("binding in-port '{id}'"))?;
            let samples = source.materialise(total_samples, self.sample_rate)?;
            inputs.push((handle, samples));
        }

        // Pre-validate capture wires so a typo fails before the run loop.
        for wire in &self.peek_wires {
            let (node, port) = split_wire(wire)?;
            engine
                .peek(node, port)
                .with_context(|| format!("capture wire '{wire}'"))?;
        }
        let out_handles: Vec<(String, engine::OutPortHandle)> = self
            .out_wires
            .iter()
            .map(|id| {
                engine
                    .resolve_out_port(id)
                    .map(|h| (id.clone(), h))
                    .with_context(|| format!("capture out-port '{id}'"))
            })
            .collect::<Result<_>>()?;

        let mut peeked: HashMap<String, Vec<f32>> = self
            .peek_wires
            .iter()
            .map(|w| (w.clone(), Vec::new()))
            .collect();
        let mut outs: HashMap<String, Vec<f32>> = self
            .out_wires
            .iter()
            .map(|w| (w.clone(), Vec::new()))
            .collect();

        for block_idx in 0..n_blocks {
            // Feed this block of every input.
            let off = block_idx as usize * self.block_size;
            for (handle, samples) in &inputs {
                let dst = engine.in_port(*handle);
                let src = &samples[off..off + self.block_size];
                dst.copy_from_slice(src);
            }

            engine.process_block(self.block_size);

            // Capture internal wires (peek).
            for wire in &self.peek_wires {
                let (node, port) = split_wire(wire).unwrap();
                let block = engine.peek(node, port).unwrap();
                peeked.get_mut(wire).unwrap().extend_from_slice(block);
            }
            // Capture boundary out-ports.
            for (id, handle) in &out_handles {
                outs.get_mut(id)
                    .unwrap()
                    .extend_from_slice(engine.out_port(*handle));
            }
        }

        Ok(Captured {
            peeked,
            outs,
            sample_rate: self.sample_rate,
            block_size: self.block_size,
        })
    }
}

/// Samples captured from a bench run, keyed by wire path / out-port id, plus
/// metric helpers. Expectations are plain `assert!`s over these.
pub struct Captured {
    peeked: HashMap<String, Vec<f32>>,
    outs: HashMap<String, Vec<f32>>,
    sample_rate: u32,
    block_size: usize,
}

impl Captured {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// All captured samples for an internal wire (concatenated across blocks).
    pub fn wire(&self, path: &str) -> &[f32] {
        self.peeked
            .get(path)
            .unwrap_or_else(|| panic!("wire '{path}' was not captured; add it to .capture(...)"))
    }

    /// All captured samples for a boundary out-port.
    pub fn out(&self, id: &str) -> &[f32] {
        self.outs.get(id).unwrap_or_else(|| {
            panic!("out-port '{id}' was not captured; add it to .capture_out(...)")
        })
    }

    /// Per-hop values: one sample per block (the last, since hold-style feature
    /// ports emit a sample-and-hold value across the block). Use for f0-style
    /// wires that carry one logical value per analysis hop.
    pub fn per_hop(&self, path: &str) -> Vec<f32> {
        let samples = self.wire(path);
        samples
            .chunks(self.block_size)
            .filter_map(|c| c.last().copied())
            .collect()
    }

    /// Fraction of hops on `path` that are voiced (finite and > 0).
    pub fn coverage_voiced(&self, path: &str) -> f32 {
        audio_trace::coverage_voiced_of(&self.per_hop(path))
    }

    /// Count of voiced-to-voiced hop transitions on `path` whose pitch jumps by
    /// more than 600 cents (half an octave) — YIN's classic octave-error mode.
    pub fn octave_jumps(&self, path: &str) -> usize {
        audio_trace::count_octave_jumps(&self.per_hop(path), 600.0)
    }

    /// The last captured value of an internal wire. Useful for steady-state
    /// meters (RMS, etc.) that converge to one value.
    pub fn last_wire(&self, path: &str) -> f32 {
        *self
            .wire(path)
            .last()
            .unwrap_or_else(|| panic!("wire '{path}' is empty"))
    }

    /// The last captured value of a boundary out-port.
    pub fn last_out(&self, id: &str) -> f32 {
        *self
            .out(id)
            .last()
            .unwrap_or_else(|| panic!("out-port '{id}' is empty"))
    }

    /// Assert that `actual` is within `tolerance_db` of `expected` on a dB
    /// (20·log10) scale — the convention used for amplitude/RMS checks.
    pub fn assert_near_db(actual: f32, expected: f32, tolerance_db: f32) {
        let err_db = 20.0 * (actual / expected).abs().log10();
        assert!(
            err_db.abs() <= tolerance_db,
            "expected {expected} ± {tolerance_db} dB, got {actual} ({err_db:+.3} dB off)"
        );
    }

    /// Median absolute frame-to-frame jitter in cents within voiced runs on
    /// `path`. A robust "how steady is the trace" number.
    pub fn median_jitter_cents(&self, path: &str) -> f32 {
        audio_trace::median_jitter_cents_of(&self.per_hop(path))
    }
}

fn split_wire(wire: &str) -> Result<(&str, &str)> {
    wire.split_once('.')
        .with_context(|| format!("invalid wire '{wire}': expected '<node_id>.<port>'"))
}

/// Read a mono WAV at the expected sample rate. Panics if the file isn't mono
/// or isn't at `sample_rate` — convert with `afconvert` before benching.
pub fn read_wav_mono(path: &Path, sample_rate: u32) -> Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening wav {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != sample_rate {
        anyhow::bail!(
            "{}: sample rate {} != bench rate {}",
            path.display(),
            spec.sample_rate,
            sample_rate
        );
    }
    if spec.channels != 1 {
        anyhow::bail!("{}: {} channels, want mono", path.display(), spec.channels);
    }
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("reading float samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("reading int samples")?
        }
    };
    Ok(samples)
}
