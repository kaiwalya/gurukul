//! Onset detector: emits a one-sample pulse on each detected note onset.
//!
//! Algorithm (correctness-first, realtime-safe by construction):
//!   1. Compute short-term energy in a sliding window of `frame_samples`
//!      samples (default 480 = 10 ms at 48 kHz).
//!   2. Maintain a long-term moving baseline (exponential moving average of
//!      log-energy) — the "background" the onset has to rise above.
//!   3. Fire when log-energy exceeds baseline + `threshold_db` AND no onset
//!      has fired in the last `min_separation_samples` samples.
//!
//! Output port:
//!   onset [Feature]: `1.0` on the sample at which an onset is detected,
//!   `0.0` otherwise. Pulses are exactly one sample wide.
//!
//! This is the energy-based variant — fine for the synthetic oracle (which
//! has clean note boundaries). A spectral-flux variant is appropriate for
//! real audio and can land as a sibling node later.

use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct Onset {
    threshold_db: f32,
    min_separation_samples: usize,
    baseline_alpha: f32,

    sample_rate: f32,

    // Short-term energy state — running sum of squares in a ring of length
    // frame_samples.
    energy_ring: Vec<f32>,
    energy_ring_write: usize,
    energy_sum_sq: f32,
    energy_filled: bool,

    // Long-term baseline (log10 energy, EMA).
    baseline_log10: f32,
    baseline_initialised: bool,

    // Cooldown.
    samples_since_onset: usize,
}

impl Onset {
    pub fn new(
        frame_samples: usize,
        threshold_db: f32,
        min_separation_samples: usize,
        baseline_alpha: f32,
    ) -> Self {
        Self {
            threshold_db,
            min_separation_samples,
            baseline_alpha: baseline_alpha.clamp(1e-6, 1.0),
            sample_rate: 48000.0,
            energy_ring: vec![0.0; frame_samples.max(1)],
            energy_ring_write: 0,
            energy_sum_sq: 0.0,
            energy_filled: false,
            baseline_log10: 0.0,
            baseline_initialised: false,
            samples_since_onset: usize::MAX / 2,
        }
    }
}

impl Node for Onset {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        for v in self.energy_ring.iter_mut() {
            *v = 0.0;
        }
        self.energy_ring_write = 0;
        self.energy_sum_sq = 0.0;
        self.energy_filled = false;
        self.baseline_log10 = 0.0;
        self.baseline_initialised = false;
        self.samples_since_onset = usize::MAX / 2;
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.is_empty() {
            return;
        }
        let audio_in = &inputs[0][..nframes];
        let onset_out = &mut outputs[0][..nframes];

        // Default the output to zero each block; we'll set 1.0 only on onset.
        for s in onset_out.iter_mut() {
            *s = 0.0;
        }

        let ring_len = self.energy_ring.len();
        // Onset threshold expressed in log10-energy units. 20 dB = factor 10
        // in amplitude = factor 100 in energy = +2.0 in log10(energy).
        let log10_threshold = self.threshold_db / 10.0;

        for (i, &x) in audio_in.iter().enumerate() {
            // Update sliding sum of squares.
            let x_sq = x * x;
            let old = self.energy_ring[self.energy_ring_write];
            self.energy_ring[self.energy_ring_write] = x_sq;
            self.energy_sum_sq = self.energy_sum_sq + x_sq - old;
            self.energy_ring_write = (self.energy_ring_write + 1) % ring_len;
            if self.energy_ring_write == 0 {
                self.energy_filled = true;
            }

            self.samples_since_onset = self.samples_since_onset.saturating_add(1);

            if !self.energy_filled {
                continue;
            }

            // Mean energy over the frame. Add a small epsilon to avoid log(0).
            let energy = (self.energy_sum_sq / ring_len as f32).max(1e-12);
            let log10_e = energy.log10();

            if !self.baseline_initialised {
                self.baseline_log10 = log10_e;
                self.baseline_initialised = true;
                continue;
            }

            // Onset condition: current frame energy is more than threshold_db
            // above the running baseline, *and* we've cooled down.
            let above = log10_e - self.baseline_log10;
            if above > log10_threshold && self.samples_since_onset >= self.min_separation_samples {
                onset_out[i] = 1.0;
                self.samples_since_onset = 0;
                // Snap the baseline up so we don't immediately re-fire on the
                // tail of this same onset.
                self.baseline_log10 = log10_e;
            } else {
                // Slow EMA update — track noise floor, not onsets.
                self.baseline_log10 += self.baseline_alpha * (log10_e - self.baseline_log10);
            }
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "Onset",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "onset",
            ty: PortType::Feature,
        }],
        vec![
            ParamSpec {
                name: "frame_samples",
                default: 480.0, // 10 ms at 48 kHz.
                min: 32.0,
                max: 4800.0,
                unit: "samples",
            },
            ParamSpec {
                name: "threshold_db",
                default: 12.0,
                min: 1.0,
                max: 60.0,
                unit: "dB",
            },
            ParamSpec {
                name: "min_separation_samples",
                default: 4800.0, // 100 ms at 48 kHz — minimum inter-onset gap.
                min: 100.0,
                max: 48000.0,
                unit: "samples",
            },
            ParamSpec {
                name: "baseline_alpha",
                // Slow EMA: at α=0.001 the baseline half-life is ~693 samples
                // (≈ 14 ms at 48 kHz). Fast enough to settle between notes,
                // slow enough not to chase the attack.
                default: 0.001,
                min: 1e-5,
                max: 0.5,
                unit: "",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let frame_samples = *params.get("frame_samples").unwrap_or(&480.0) as usize;
            let threshold_db = *params.get("threshold_db").unwrap_or(&12.0) as f32;
            let min_separation_samples =
                *params.get("min_separation_samples").unwrap_or(&4800.0) as usize;
            let baseline_alpha = *params.get("baseline_alpha").unwrap_or(&0.001) as f32;
            Box::new(Onset::new(
                frame_samples,
                threshold_db,
                min_separation_samples,
                baseline_alpha,
            )) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn run_blocks(node: &mut Onset, audio: &[f32], block_size: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(audio.len());
        for chunk in audio.chunks(block_size) {
            let nframes = chunk.len();
            let mut buf = vec![0.0f32; nframes];
            {
                let mut outs: Vec<&mut [f32]> = vec![buf.as_mut_slice()];
                node.process(&[chunk], &mut outs, nframes);
            }
            out.extend_from_slice(&buf);
        }
        out
    }

    fn onset_times(out: &[f32]) -> Vec<usize> {
        out.iter()
            .enumerate()
            .filter_map(|(i, &v)| if v > 0.5 { Some(i) } else { None })
            .collect()
    }

    /// Build a single sine burst starting at `start_sample`, lasting
    /// `burst_samples`, embedded in `n` total samples of silence.
    fn burst(n: usize, start_sample: usize, burst_samples: usize, freq: f32, sr: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; n];
        for i in 0..burst_samples {
            let idx = start_sample + i;
            if idx >= n {
                break;
            }
            let t = i as f32 / sr;
            out[idx] = 0.5 * (TAU * freq * t).sin();
        }
        out
    }

    #[test]
    fn fires_on_single_burst() {
        let mut node = Onset::new(480, 12.0, 4800, 0.001);
        node.prepare("test", 48000, 512);

        // 1 s of audio; burst starts at 0.3 s, lasts 0.2 s.
        let audio = burst(48000, 48000 * 3 / 10, 48000 / 5, 440.0, 48000.0);
        let out = run_blocks(&mut node, &audio, 512);
        let times = onset_times(&out);

        assert_eq!(times.len(), 1, "expected one onset, got {times:?}");
        // The detected onset should be close to the burst start (within
        // frame_samples + a few extra samples for the threshold crossing).
        let detected = times[0];
        let expected = 48000 * 3 / 10;
        let lag = detected.saturating_sub(expected);
        assert!(
            lag < 1000,
            "onset detected too late: detected={detected}, expected~{expected}"
        );
    }

    #[test]
    fn no_fires_on_silence() {
        let mut node = Onset::new(480, 12.0, 4800, 0.001);
        node.prepare("test", 48000, 512);

        let audio = vec![0.0f32; 48000];
        let out = run_blocks(&mut node, &audio, 512);
        let times = onset_times(&out);

        assert!(
            times.is_empty(),
            "silence should produce no onsets, got {times:?}"
        );
    }
}
