//! Breath / aspiration detector: emits a sustained `1.0` while breath is
//! active, `0.0` otherwise.
//!
//! Algorithm (correctness-first, realtime-safe by construction):
//!   1. Compute short-term RMS energy in a sliding window of
//!      `frame_samples` samples (default 1024 ≈ 21 ms at 48 kHz).
//!   2. Compute short-term HF energy in the same window, where HF means
//!      the output of a one-pole first-difference high-pass filter:
//!      `y[n] = x[n] - x[n-1]`. Voiced sines concentrate energy below
//!      ~1 kHz; broadband breath spreads it across the spectrum, so the
//!      HF/total ratio rises sharply during breath.
//!   3. Fire (output 1.0) while the HF/total ratio is above
//!      `hf_ratio_enter` AND total energy is above `min_energy`. Stay
//!      latched until the ratio falls below `hf_ratio_exit` for at least
//!      `min_release_samples` consecutive samples. Hysteresis prevents
//!      chatter at envelope edges.
//!
//! Output port:
//!   breath [Feature]: `1.0` while breath is detected, `0.0` otherwise.
//!
//! Note: a real-world breath detector would benefit from a proper
//! spectral-centroid or band-energy split (e.g. an FFT or Linkwitz-Riley
//! crossover). The first-difference HPF is a stand-in that is cheap,
//! realtime-safe, and discriminates breath-shaped noise from a sine
//! cleanly enough for the synthetic oracle.

use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct Breath {
    hf_ratio_enter: f32,
    hf_ratio_exit: f32,
    min_energy: f32,
    min_release_samples: usize,

    sample_rate: f32,

    // Sliding sum-of-squares for total energy.
    total_ring: Vec<f32>,
    total_write: usize,
    total_sum_sq: f32,

    // Sliding sum-of-squares for HF energy.
    hf_ring: Vec<f32>,
    hf_write: usize,
    hf_sum_sq: f32,

    filled: bool,
    prev_sample: f32,
    latched: bool,
    samples_below_exit: usize,
}

impl Breath {
    pub fn new(
        frame_samples: usize,
        hf_ratio_enter: f32,
        hf_ratio_exit: f32,
        min_energy: f32,
        min_release_samples: usize,
    ) -> Self {
        let n = frame_samples.max(1);
        Self {
            hf_ratio_enter,
            hf_ratio_exit,
            min_energy,
            min_release_samples,
            sample_rate: 48000.0,
            total_ring: vec![0.0; n],
            total_write: 0,
            total_sum_sq: 0.0,
            hf_ring: vec![0.0; n],
            hf_write: 0,
            hf_sum_sq: 0.0,
            filled: false,
            prev_sample: 0.0,
            latched: false,
            samples_below_exit: 0,
        }
    }
}

impl Node for Breath {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        for v in self.total_ring.iter_mut() {
            *v = 0.0;
        }
        for v in self.hf_ring.iter_mut() {
            *v = 0.0;
        }
        self.total_write = 0;
        self.hf_write = 0;
        self.total_sum_sq = 0.0;
        self.hf_sum_sq = 0.0;
        self.filled = false;
        self.prev_sample = 0.0;
        self.latched = false;
        self.samples_below_exit = 0;
    }

    fn reset(&mut self) {
        for v in self.total_ring.iter_mut() {
            *v = 0.0;
        }
        for v in self.hf_ring.iter_mut() {
            *v = 0.0;
        }
        self.total_write = 0;
        self.hf_write = 0;
        self.total_sum_sq = 0.0;
        self.hf_sum_sq = 0.0;
        self.filled = false;
        self.prev_sample = 0.0;
        self.latched = false;
        self.samples_below_exit = 0;
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.is_empty() {
            return;
        }
        let audio_in = &inputs[0][..nframes];
        let out = &mut outputs[0][..nframes];

        let ring_len = self.total_ring.len();

        for (i, &x) in audio_in.iter().enumerate() {
            // Total energy update.
            let x_sq = x * x;
            let old_total = self.total_ring[self.total_write];
            self.total_ring[self.total_write] = x_sq;
            self.total_sum_sq = self.total_sum_sq + x_sq - old_total;

            // HF energy update via first-difference HPF.
            let hf = x - self.prev_sample;
            self.prev_sample = x;
            let hf_sq = hf * hf;
            let old_hf = self.hf_ring[self.hf_write];
            self.hf_ring[self.hf_write] = hf_sq;
            self.hf_sum_sq = self.hf_sum_sq + hf_sq - old_hf;

            self.total_write = (self.total_write + 1) % ring_len;
            self.hf_write = (self.hf_write + 1) % ring_len;
            if self.total_write == 0 {
                self.filled = true;
            }

            if !self.filled {
                out[i] = 0.0;
                continue;
            }

            let total = (self.total_sum_sq / ring_len as f32).max(1e-12);
            let hf_e = (self.hf_sum_sq / ring_len as f32).max(0.0);
            let ratio = hf_e / total;

            if self.latched {
                if ratio < self.hf_ratio_exit || total < self.min_energy {
                    self.samples_below_exit = self.samples_below_exit.saturating_add(1);
                    if self.samples_below_exit >= self.min_release_samples {
                        self.latched = false;
                        self.samples_below_exit = 0;
                    }
                } else {
                    self.samples_below_exit = 0;
                }
            } else if ratio >= self.hf_ratio_enter && total >= self.min_energy {
                self.latched = true;
                self.samples_below_exit = 0;
            }

            out[i] = if self.latched { 1.0 } else { 0.0 };
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "Breath",
        vec![PortSpec {
            name: "audio_in",
            ty: PortType::Audio,
        }],
        vec![PortSpec {
            name: "breath",
            ty: PortType::Feature,
        }],
        vec![
            ParamSpec {
                name: "frame_samples",
                default: 1024.0,
                min: 64.0,
                max: 8192.0,
                unit: "samples",
            },
            ParamSpec {
                name: "hf_ratio_enter",
                // First-difference HPF: a 440 Hz sine has HF/total ≈
                // (2π·440/48000)² ≈ 0.0033. White noise has HF/total ≈ 2.0.
                // 0.20 is well above sine and well below noise.
                default: 0.20,
                min: 0.01,
                max: 4.0,
                unit: "",
            },
            ParamSpec {
                name: "hf_ratio_exit",
                default: 0.10,
                min: 0.005,
                max: 4.0,
                unit: "",
            },
            ParamSpec {
                name: "min_energy",
                default: 1e-5,
                min: 1e-9,
                max: 1.0,
                unit: "",
            },
            ParamSpec {
                name: "min_release_samples",
                default: 2400.0, // 50 ms at 48 kHz.
                min: 0.0,
                max: 48000.0,
                unit: "samples",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let frame_samples = *params.get("frame_samples").unwrap_or(&1024.0) as usize;
            let hf_ratio_enter = *params.get("hf_ratio_enter").unwrap_or(&0.20) as f32;
            let hf_ratio_exit = *params.get("hf_ratio_exit").unwrap_or(&0.10) as f32;
            let min_energy = *params.get("min_energy").unwrap_or(&1e-5) as f32;
            let min_release_samples =
                *params.get("min_release_samples").unwrap_or(&2400.0) as usize;
            Box::new(Breath::new(
                frame_samples,
                hf_ratio_enter,
                hf_ratio_exit,
                min_energy,
                min_release_samples,
            )) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn run_blocks(node: &mut Breath, audio: &[f32], block_size: usize) -> Vec<f32> {
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

    fn xs(seed: &mut u64) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed as i64 as f64 / i64::MAX as f64) as f32
    }

    #[test]
    fn fires_on_noise_quiet_on_sine() {
        let mut node = Breath::new(1024, 0.20, 0.10, 1e-5, 2400);
        node.prepare("test", 48000, 512);

        // 1 s of 440 Hz sine at amplitude 0.5.
        let mut audio = (0..48000)
            .map(|i| 0.5 * (TAU * 440.0 * i as f32 / 48000.0).sin())
            .collect::<Vec<f32>>();
        // Followed by 1 s of white noise at amplitude 0.15.
        let mut seed: u64 = 0xDEAD_BEEF;
        audio.extend((0..48000).map(|_| 0.15 * xs(&mut seed)));

        let out = run_blocks(&mut node, &audio, 512);

        // During the sine segment (after ring fill), should be 0.
        let sine_active: usize = out[5000..48000].iter().filter(|&&v| v > 0.5).count();
        assert!(
            sine_active < 100,
            "sine should not register as breath; got {sine_active} active samples"
        );

        // During the noise segment, should latch on quickly and stay on.
        // Allow up to 4 k samples (~83 ms) at the boundary for the ring to refill.
        let noise_active: usize = out[52000..96000].iter().filter(|&&v| v > 0.5).count();
        assert!(
            noise_active > 40_000,
            "noise should register as breath; got {noise_active} active samples out of 44000"
        );
    }

    #[test]
    fn silent_on_silence() {
        let mut node = Breath::new(1024, 0.20, 0.10, 1e-5, 2400);
        node.prepare("test", 48000, 512);

        let audio = vec![0.0f32; 48000];
        let out = run_blocks(&mut node, &audio, 512);
        let active: usize = out.iter().filter(|&&v| v > 0.5).count();
        assert_eq!(active, 0, "silence should produce no breath detections");
    }
}
