//! Real-voice characterization test.
//!
//! Marked `#[ignore]` because it depends on wav files under
//! `node-pitch-yin/test_data/` that are not checked into the repo
//! (license + size). Run locally with:
//!
//!     cargo test -p node-pitch-yin --release --test real_voice -- --ignored --nocapture
//!
//! For each wav file, the test runs PitchYin with the same params the
//! macOS cabinet uses (window=2048, hop=512, fmin=70, fmax=1000,
//! threshold=0.15) and dumps a per-hop CSV alongside a console
//! summary so we can see what YIN is actually doing on real voice.
//!
//! Metrics — interpret these together; no single number is "the score":
//!
//!   * coverage_voiced — fraction of hops where YIN emitted hz > 0.
//!   * frame_jitter_cents_p50 / _p95 / _max — robust stats on
//!     frame-to-frame |Δcents| within voiced runs. Big numbers here
//!     mean the trace would look jittery even on a sustained note.
//!   * octave_jump_count — frame-to-frame |Δcents| > 600 (more than
//!     half an octave). YIN's classic failure mode is jumping to half
//!     or double the right pitch.

use engine::Node;
use node_pitch_yin::PitchYin;
use std::fs;
use std::path::{Path, PathBuf};

const SR: u32 = 48000;
const HOP: usize = 512;
const WINDOW: usize = 2048;
const FMIN: f32 = 70.0;
const FMAX: f32 = 1000.0;
const THRESHOLD: f32 = 0.15;

fn test_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data")
}

fn collect_wavs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("wav") {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Read a wav file as mono Float32 at 48 kHz. Panics if the file
/// isn't already 48 kHz mono — we expect the user to convert with
/// `afconvert` (or equivalent) before dropping the file in.
fn read_wav_mono_48k(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(
        spec.sample_rate,
        SR,
        "{}: sample rate {} != {}",
        path.display(),
        spec.sample_rate,
        SR
    );
    assert_eq!(
        spec.channels,
        1,
        "{}: {} channels, want mono",
        path.display(),
        spec.channels
    );
    match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.expect("read sample"))
            .collect(),
        hound::SampleFormat::Int => {
            // Normalize to [-1, 1] by max int magnitude for the bit
            // depth. We support 16 / 24 / 32 bit ints, which is what
            // afconvert produces.
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.expect("read sample") as f32 / max)
                .collect()
        }
    }
}

fn hz_to_cents(hz: f32, ref_hz: f32) -> f32 {
    1200.0 * (hz / ref_hz).log2()
}

fn hz_to_midi_name(hz: f32) -> String {
    if hz <= 0.0 || !hz.is_finite() {
        return "—".into();
    }
    let midi = 69.0 + 12.0 * (hz / 440.0).log2();
    let midi_round = midi.round() as i32;
    let cents = ((midi - midi_round as f32) * 100.0).round() as i32;
    let note_names = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let idx = ((midi_round % 12) + 12) % 12;
    let octave = midi_round / 12 - 1;
    let sign = if cents >= 0 { "+" } else { "" };
    format!("{}{}{}{}¢", note_names[idx as usize], octave, sign, cents)
}

struct ClipReport {
    name: String,
    hops_total: usize,
    hops_voiced: usize,
    frame_jitter: Stats,
    octave_jumps: usize,
}

#[derive(Default)]
struct Stats {
    samples: Vec<f32>,
}
impl Stats {
    fn push(&mut self, v: f32) {
        self.samples.push(v);
    }
    fn percentile(&self, p: f32) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((p / 100.0) * (s.len() - 1) as f32).round() as usize;
        s[idx.min(s.len() - 1)]
    }
    fn max(&self) -> f32 {
        self.samples
            .iter()
            .copied()
            .fold(0.0f32, |a, b| a.max(b.abs()))
    }
}

fn analyze(path: &Path) -> ClipReport {
    let signal = read_wav_mono_48k(path);
    let mut node = PitchYin::new(WINDOW, HOP, FMIN, FMAX, THRESHOLD);
    node.prepare(
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("test"),
        SR,
        HOP,
    );

    let mut out_hop = vec![0.0f32; HOP];
    let mut per_hop_hz: Vec<f32> = Vec::new();

    for chunk in signal.chunks(HOP) {
        let n = chunk.len();
        out_hop[..n].fill(0.0);
        node.process(&[chunk], &mut [&mut out_hop[..n]], n);
        // PitchYin emits sample-and-hold f0 across the hop; one value
        // per hop is what the cabinet reads.
        per_hop_hz.push(out_hop[n - 1]);
    }

    // Write a CSV next to the wav.
    let csv_path = path.with_extension("yin.csv");
    let mut csv = String::from("time_s,hz,midi_name,delta_cents_from_prev\n");
    let mut prev_voiced_hz: Option<f32> = None;
    let mut jitter = Stats::default();
    let mut octave_jumps = 0usize;
    let mut hops_voiced = 0usize;

    for (i, &hz) in per_hop_hz.iter().enumerate() {
        let t = (i * HOP) as f32 / SR as f32;
        let voiced = hz.is_finite() && hz > 0.0;
        let name = if voiced {
            hz_to_midi_name(hz)
        } else {
            "—".into()
        };
        let delta = if let (Some(prev), true) = (prev_voiced_hz, voiced) {
            let d = hz_to_cents(hz, prev);
            jitter.push(d.abs());
            if d.abs() > 600.0 {
                octave_jumps += 1;
            }
            format!("{:.1}", d)
        } else {
            String::new()
        };
        if voiced {
            hops_voiced += 1;
            prev_voiced_hz = Some(hz);
        } else {
            prev_voiced_hz = None; // jitter computed only within a voiced run
        }
        csv.push_str(&format!("{:.4},{:.3},{},{}\n", t, hz, name, delta));
    }
    fs::write(&csv_path, csv).expect("write csv");
    eprintln!("  wrote {}", csv_path.display());

    ClipReport {
        name: path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
        hops_total: per_hop_hz.len(),
        hops_voiced,
        frame_jitter: jitter,
        octave_jumps,
    }
}

#[test]
#[ignore]
fn yin_on_real_voice() {
    let dir = test_data_dir();
    let wavs = collect_wavs(&dir);
    if wavs.is_empty() {
        panic!(
            "no wav files in {} — drop a 48 kHz mono wav into that folder and rerun",
            dir.display()
        );
    }

    eprintln!();
    eprintln!(
        "PitchYin params: window={} hop={} fmin={} fmax={} threshold={}",
        WINDOW, HOP, FMIN, FMAX, THRESHOLD
    );
    eprintln!();

    let mut reports = Vec::new();
    for path in &wavs {
        eprintln!("analyzing {}", path.display());
        reports.push(analyze(path));
    }

    eprintln!();
    eprintln!(
        "{:<32} {:>8} {:>8} {:>9} {:>9} {:>9} {:>11}",
        "clip", "hops", "voiced%", "jitter50", "jitter95", "jitterMx", "octaveJumps"
    );
    eprintln!("{}", "-".repeat(95));
    for r in &reports {
        let voiced_pct = if r.hops_total > 0 {
            100.0 * r.hops_voiced as f32 / r.hops_total as f32
        } else {
            0.0
        };
        eprintln!(
            "{:<32} {:>8} {:>7.1}% {:>9.1} {:>9.1} {:>9.1} {:>11}",
            truncate(&r.name, 32),
            r.hops_total,
            voiced_pct,
            r.frame_jitter.percentile(50.0),
            r.frame_jitter.percentile(95.0),
            r.frame_jitter.max(),
            r.octave_jumps
        );
    }
    eprintln!();
    eprintln!("  hops      = total hops analyzed ({} samples each)", HOP);
    eprintln!("  voiced%   = fraction of hops where YIN emitted hz > 0");
    eprintln!("  jitter50/95/Mx = median / p95 / max of |Δcents| between adjacent voiced hops");
    eprintln!("  octaveJumps    = count of voiced-to-voiced transitions with |Δcents| > 600");
    eprintln!();
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.into()
    } else {
        format!("{}…", &s[..n - 1])
    }
}
