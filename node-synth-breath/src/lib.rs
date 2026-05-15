//! Synthetic breath / aspiration source for the Tier-1 breath oracle.
//!
//! Emits white-noise bursts at a fixed cadence:
//!   - bursts last `breath_duration_s` seconds,
//!   - new bursts start every `period_s` seconds,
//!   - 50 ms attack and 100 ms release envelopes shape each burst.
//!
//! Output port:
//!   audio_out [Audio]: noise gated by the burst envelope; zero between
//!   bursts.
//!
//! Realtime-safe by construction: all state lives on the struct;
//! `process()` does no allocation. PRNG mirrors node-synth-pink-noise's
//! SplitMix64-seeded xorshift64 so nearby seeds give uncorrelated streams.

use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

fn splitmix64(state: u64) -> u64 {
    let state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn u64_to_f32(v: u64) -> f32 {
    (v as i64 as f64 / i64::MAX as f64) as f32
}

pub struct SynthBreath {
    period_s: f32,
    breath_duration_s: f32,
    amplitude: f32,
    seed: u64,

    sample_rate: f32,
    state: u64,

    samples_into_period: u64,
    samples_per_period: u64,
    breath_samples: u64,
    attack_samples: u64,
    release_samples: u64,
}

impl SynthBreath {
    pub fn new(period_s: f32, breath_duration_s: f32, amplitude: f32, seed: u64) -> Self {
        Self {
            period_s,
            breath_duration_s,
            amplitude,
            seed,
            sample_rate: 48000.0,
            state: 1,
            samples_into_period: 0,
            samples_per_period: 0,
            breath_samples: 0,
            attack_samples: 0,
            release_samples: 0,
        }
    }
}

impl Node for SynthBreath {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        let mixed = splitmix64(if self.seed == 0 { 1 } else { self.seed });
        self.state = if mixed == 0 { 1 } else { mixed };
        self.samples_into_period = 0;
        self.samples_per_period = (self.sample_rate * self.period_s) as u64;
        self.breath_samples = (self.sample_rate * self.breath_duration_s) as u64;
        self.attack_samples = (self.sample_rate * 0.05) as u64; // 50 ms
        self.release_samples = (self.sample_rate * 0.10) as u64; // 100 ms
    }

    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if outputs.is_empty() {
            return;
        }
        let out = &mut outputs[0][..nframes];

        for s in out.iter_mut() {
            if self.samples_into_period >= self.samples_per_period {
                self.samples_into_period = 0;
            }
            let t = self.samples_into_period;
            self.samples_into_period += 1;

            if t >= self.breath_samples {
                *s = 0.0;
                continue;
            }

            // Envelope: attack ramp, sustain, release ramp.
            let env = if t < self.attack_samples {
                t as f32 / self.attack_samples.max(1) as f32
            } else if t >= self.breath_samples.saturating_sub(self.release_samples) {
                let into_release = t - self.breath_samples.saturating_sub(self.release_samples);
                1.0 - (into_release as f32 / self.release_samples.max(1) as f32)
            } else {
                1.0
            };

            let n = u64_to_f32(xorshift64(&mut self.state));
            *s = n * env * self.amplitude;
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "SynthBreath",
        vec![],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![
            ParamSpec {
                name: "period_s",
                default: 2.0,
                min: 0.5,
                max: 10.0,
                unit: "s",
            },
            ParamSpec {
                name: "breath_duration_s",
                default: 0.6,
                min: 0.1,
                max: 5.0,
                unit: "s",
            },
            ParamSpec {
                name: "amplitude",
                default: 0.15,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamSpec {
                name: "seed",
                default: 1.0,
                min: 0.0,
                max: 1e18,
                unit: "seed",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let period_s = *params.get("period_s").unwrap_or(&2.0) as f32;
            let breath_duration_s = *params.get("breath_duration_s").unwrap_or(&0.6) as f32;
            let amplitude = *params.get("amplitude").unwrap_or(&0.15) as f32;
            let seed = *params.get("seed").unwrap_or(&1.0) as u64;
            Box::new(SynthBreath::new(
                period_s,
                breath_duration_s,
                amplitude,
                seed,
            )) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node: &mut SynthBreath, nframes: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; nframes];
        {
            let mut outs: Vec<&mut [f32]> = vec![out.as_mut_slice()];
            node.process(&[], &mut outs, nframes);
        }
        out
    }

    #[test]
    fn silent_outside_burst() {
        // period 1 s, breath 0.2 s — sample 30000 (0.625 s) is outside the burst.
        let mut node = SynthBreath::new(1.0, 0.2, 0.5, 1);
        node.prepare("test", 48000, 48000);
        let out = run(&mut node, 48000);
        assert!(out[30000].abs() < 1e-6, "expected silence at 0.625 s");
    }

    #[test]
    fn audible_inside_burst() {
        // Mid-burst (0.1 s, sustain region) should have appreciable noise energy.
        let mut node = SynthBreath::new(1.0, 0.2, 0.5, 1);
        node.prepare("test", 48000, 48000);
        let out = run(&mut node, 48000);
        let mid_rms = {
            let slice = &out[4800..9600]; // 0.1 s to 0.2 s
            (slice.iter().map(|&s| s * s).sum::<f32>() / slice.len() as f32).sqrt()
        };
        assert!(
            mid_rms > 0.05,
            "expected audible noise mid-burst, got rms={mid_rms}"
        );
    }

    #[test]
    fn deterministic_for_seed() {
        let mut a = SynthBreath::new(1.0, 0.2, 0.5, 7);
        let mut b = SynthBreath::new(1.0, 0.2, 0.5, 7);
        a.prepare("a", 48000, 4096);
        b.prepare("b", 48000, 4096);
        assert_eq!(run(&mut a, 4096), run(&mut b, 4096));
    }
}
