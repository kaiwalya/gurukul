//! Vibrato analyzer: estimates `(rate_hz, depth_cents)` from an f0 contour.
//!
//! Consumes a Feature port carrying YIN-style f0 estimates (Hz, with `0.0` =
//! unvoiced sentinel). Maintains a ring buffer of the most recent samples'
//! worth of f0 values. Every `analysis_hop` blocks, runs one analysis on the
//! current window and updates zero-order-held estimates of vibrato rate and
//! depth. Outputs are Feature ports filled with the latest held value each
//! block.
//!
//! Algorithm (correctness-first, allocating; realtime-safe pass deferred to a
//! follow-up):
//!   1. Decimate the f0 ring by `decimation` (default 256× → 187.5 Hz contour
//!      rate at 48 kHz audio). The information in YIN's ZOH-filled f0 port
//!      only changes at the hop rate, so decimation collapses redundant work
//!      with no information loss for vibrato frequencies ≪ contour rate / 2.
//!   2. Convert voiced f0 → cents (1200 × log2(f)); drop unvoiced samples.
//!   3. Subtract the mean → centred contour.
//!   4. Autocorrelate over candidate lags (mapped from the rate range);
//!      peak-pick to estimate rate. Reject if the peak is weak relative to
//!      the signal energy.
//!   5. Depth = half the peak-to-peak range. For sin-modulated log2(f) with
//!      amplitude `D` cents, the centred contour has range 2D, so half-range
//!      is D.
//!
//! Tier-1 oracle target: recover the synth's `vibrato_rate` and
//! `vibrato_depth_cents` from `node-synth-vibrato-sine` to within tolerance
//! across a small (rate, depth) grid. See `tests/vibrato_sweep.rs`.

use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

pub struct Vibrato {
    // Parameters resolved at construction.
    window_samples: usize,
    analysis_hop: usize,
    decimation: usize,
    rate_min_hz: f32,
    rate_max_hz: f32,

    // Set in prepare().
    sample_rate: f32,

    // Ring buffer of the most recent f0 values, sampled at the audio rate.
    ring: Vec<f32>,
    ring_write: usize,
    samples_since_analysis: usize,
    total_samples: usize,

    // Last held estimates (zero-order hold).
    held_rate: f32,
    held_depth: f32,

    // Scratches sized in new(); reused each analysis via clear() + push() so
    // no allocation occurs once the node is constructed. See the no_alloc
    // integration test.
    centred: Vec<f32>,
    rns: Vec<f32>,
}

impl Vibrato {
    pub fn new(
        window_samples: usize,
        analysis_hop: usize,
        decimation: usize,
        rate_min_hz: f32,
        rate_max_hz: f32,
    ) -> Self {
        let decimation = decimation.max(1);
        let decim_cap = window_samples / decimation;
        Self {
            window_samples,
            analysis_hop,
            decimation,
            rate_min_hz,
            rate_max_hz,
            sample_rate: 48000.0,
            ring: vec![0.0; window_samples],
            ring_write: 0,
            samples_since_analysis: 0,
            total_samples: 0,
            held_rate: 0.0,
            held_depth: 0.0,
            // Centred contour can be at most `decim_cap` entries (when every
            // decimated sample is voiced). The autocorr scratch is sized to
            // the worst-case lag range, which is bounded by `decim_cap / 2`.
            centred: Vec::with_capacity(decim_cap),
            rns: Vec::with_capacity(decim_cap / 2 + 1),
        }
    }

    /// Run one analysis on the current window contents, writing results into
    /// `(self.held_rate, self.held_depth)`. Allocation-free: scratches are
    /// pre-sized and reused.
    fn analyse(&mut self) {
        // Pull voiced f0 samples from the ring (decimated), convert to cents,
        // accumulate sum for mean-centring in one pass.
        self.centred.clear();
        let n_decim = self.window_samples / self.decimation;
        let mut sum = 0.0f32;
        for k in 0..n_decim {
            let idx = (self.ring_write + k * self.decimation) % self.window_samples;
            let f = self.ring[idx];
            if f > 0.0 {
                let c = 1200.0 * f.log2();
                self.centred.push(c);
                sum += c;
            }
        }

        let n = self.centred.len();
        let voiced_frac = n as f32 / n_decim as f32;
        if voiced_frac < 0.5 || n < 32 {
            self.held_rate = 0.0;
            self.held_depth = 0.0;
            return;
        }

        // Centre in place.
        let mean = sum / n as f32;
        for c in self.centred.iter_mut() {
            *c -= mean;
        }

        // Depth = half the peak-to-peak range. Sin modulation of amplitude D
        // gives a centred range of 2D, so half-range = D.
        let mut cmax = f32::NEG_INFINITY;
        let mut cmin = f32::INFINITY;
        let mut sum_sq = 0.0f32;
        for &c in self.centred.iter() {
            if c > cmax {
                cmax = c;
            }
            if c < cmin {
                cmin = c;
            }
            sum_sq += c * c;
        }
        let depth = (cmax - cmin) * 0.5;
        self.held_depth = depth;

        // Contour sample rate is audio_sr / decimation.
        let contour_sr = self.sample_rate / self.decimation as f32;
        let lag_max = (contour_sr / self.rate_min_hz) as usize;
        let lag_min = (contour_sr / self.rate_max_hz).max(2.0) as usize;
        let lag_max = lag_max.min(n / 2);
        if lag_max <= lag_min + 2 {
            self.held_rate = 0.0;
            return;
        }

        // RMS guard — below 1 cent RMS, we're chasing f32 roundoff.
        let mean_sq = sum_sq / n as f32;
        let rms = mean_sq.sqrt();
        if rms < 1.0 {
            self.held_rate = 0.0;
            return;
        }

        // Unbiased normalised autocorrelation. Sub-harmonics also peak near
        // +1, so we pick the *first* local maximum above 0.8 × global max —
        // that's the fundamental period, not a multiple.
        self.rns.clear();
        for lag in lag_min..=lag_max {
            let mut r = 0.0f32;
            let nover = n - lag;
            for i in 0..nover {
                r += self.centred[i] * self.centred[i + lag];
            }
            self.rns.push((r / nover as f32) / mean_sq);
        }

        let mut global_max = f32::NEG_INFINITY;
        for &v in self.rns.iter() {
            if v > global_max {
                global_max = v;
            }
        }
        if global_max < 0.7 {
            self.held_rate = 0.0;
            return;
        }

        let threshold = global_max * 0.8;
        let mut chosen_lag = lag_min;
        let mut found = false;
        for k in 1..self.rns.len() - 1 {
            if self.rns[k] >= threshold
                && self.rns[k] >= self.rns[k - 1]
                && self.rns[k] >= self.rns[k + 1]
            {
                chosen_lag = lag_min + k;
                found = true;
                break;
            }
        }
        if !found {
            // Fall back to the global max position. We have to find it again
            // without allocating an iterator chain.
            let mut best_idx = 0usize;
            let mut best_val = f32::NEG_INFINITY;
            for (i, &v) in self.rns.iter().enumerate() {
                if v > best_val {
                    best_val = v;
                    best_idx = i;
                }
            }
            chosen_lag = lag_min + best_idx;
        }

        self.held_rate = contour_sr / chosen_lag as f32;
    }
}

impl Node for Vibrato {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        self.ring_write = 0;
        self.samples_since_analysis = 0;
        self.total_samples = 0;
        self.held_rate = 0.0;
        self.held_depth = 0.0;
        for v in self.ring.iter_mut() {
            *v = 0.0;
        }
    }

    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if inputs.is_empty() || outputs.len() < 2 {
            return;
        }
        let f0_in = &inputs[0][..nframes];

        for &f in f0_in {
            self.ring[self.ring_write] = f;
            self.ring_write = (self.ring_write + 1) % self.window_samples;
            self.samples_since_analysis += 1;
            self.total_samples += 1;

            if self.samples_since_analysis >= self.analysis_hop
                && self.total_samples >= self.window_samples
            {
                self.analyse();
                self.samples_since_analysis = 0;
            }
        }

        // ZOH: fill outputs with the latest held estimates.
        outputs[0][..nframes].fill(self.held_rate);
        outputs[1][..nframes].fill(self.held_depth);
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "Vibrato",
        vec![PortSpec {
            name: "f0",
            ty: PortType::Feature,
        }],
        vec![
            PortSpec {
                name: "rate",
                ty: PortType::Feature,
            },
            PortSpec {
                name: "depth",
                ty: PortType::Feature,
            },
        ],
        vec![
            ParamSpec {
                name: "window_samples",
                default: 72000.0, // 1.5 s at 48 kHz — enough for ~7-10 vibrato cycles at 5 Hz.
                min: 4096.0,
                max: 192000.0,
                unit: "samples",
            },
            ParamSpec {
                name: "analysis_hop",
                default: 4800.0, // 0.1 s at 48 kHz.
                min: 256.0,
                max: 48000.0,
                unit: "samples",
            },
            ParamSpec {
                name: "decimation",
                // YIN's default hop is 512; decimating by 256 gives a 187.5 Hz
                // contour rate at 48 kHz audio — well above Nyquist for any
                // realistic vibrato (≤ 20 Hz) while collapsing autocorr work
                // by 256×.
                default: 256.0,
                min: 1.0,
                max: 4096.0,
                unit: "samples",
            },
            ParamSpec {
                name: "rate_min_hz",
                default: 2.0,
                min: 0.5,
                max: 20.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "rate_max_hz",
                default: 10.0,
                min: 1.0,
                max: 30.0,
                unit: "Hz",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let window_samples = *params.get("window_samples").unwrap_or(&72000.0) as usize;
            let analysis_hop = *params.get("analysis_hop").unwrap_or(&4800.0) as usize;
            let decimation = *params.get("decimation").unwrap_or(&256.0) as usize;
            let rate_min_hz = *params.get("rate_min_hz").unwrap_or(&2.0) as f32;
            let rate_max_hz = *params.get("rate_max_hz").unwrap_or(&10.0) as f32;
            Box::new(Vibrato::new(
                window_samples,
                analysis_hop,
                decimation,
                rate_min_hz,
                rate_max_hz,
            )) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesise an f0 contour for `seconds` of audio at `sr` Hz with vibrato
    /// of `rate_hz` and `depth_cents`. Returns one f0 value per audio sample
    /// (ZOH-equivalent — matches how YIN fills its f0 port).
    fn synth_f0_contour(
        sr: u32,
        seconds: f32,
        carrier_hz: f32,
        rate_hz: f32,
        depth_cents: f32,
    ) -> Vec<f32> {
        let n = (sr as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            // f(t) = carrier * 2^((depth/1200) * sin(2π rate t))
            let inst = carrier_hz
                * 2.0f32.powf((depth_cents / 1200.0) * (std::f32::consts::TAU * rate_hz * t).sin());
            out.push(inst);
        }
        out
    }

    fn run_through_blocks(node: &mut Vibrato, contour: &[f32], block_size: usize) -> (f32, f32) {
        let mut last_rate = 0.0f32;
        let mut last_depth = 0.0f32;
        for chunk in contour.chunks(block_size) {
            let nframes = chunk.len();
            let mut rate_out = vec![0.0f32; nframes];
            let mut depth_out = vec![0.0f32; nframes];
            {
                let mut outs: Vec<&mut [f32]> =
                    vec![rate_out.as_mut_slice(), depth_out.as_mut_slice()];
                node.process(&[chunk], &mut outs, nframes);
            }
            last_rate = rate_out[nframes - 1];
            last_depth = depth_out[nframes - 1];
        }
        (last_rate, last_depth)
    }

    #[test]
    fn recovers_5hz_50cent_vibrato() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 2.0, 10.0);
        node.prepare("test", sr, 512);

        let contour = synth_f0_contour(sr, 2.5, 440.0, 5.0, 50.0);
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        // Rate within 0.5 Hz, depth within 10 cents.
        assert!((rate - 5.0).abs() < 0.5, "rate {rate} should be ~5.0 Hz");
        assert!(
            (depth - 50.0).abs() < 10.0,
            "depth {depth} should be ~50 cents"
        );
    }

    #[test]
    fn recovers_6hz_30cent_vibrato() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 2.0, 10.0);
        node.prepare("test", sr, 512);

        let contour = synth_f0_contour(sr, 2.5, 220.0, 6.0, 30.0);
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        assert!((rate - 6.0).abs() < 0.5, "rate {rate} should be ~6.0 Hz");
        assert!(
            (depth - 30.0).abs() < 10.0,
            "depth {depth} should be ~30 cents"
        );
    }

    #[test]
    fn flat_contour_yields_zero_outputs() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 2.0, 10.0);
        node.prepare("test", sr, 512);

        // Constant 440 Hz, no vibrato.
        let contour = vec![440.0f32; sr as usize * 2];
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        // Depth should be ~0 cents; rate is meaningless when there's no
        // modulation — the autocorrelation peak check should fail and return 0.
        assert!(
            depth.abs() < 5.0,
            "depth {depth} should be ~0 for flat contour"
        );
        assert_eq!(rate, 0.0, "rate should be 0 when there's no modulation");
    }

    #[test]
    fn all_unvoiced_yields_zero() {
        let sr = 48000u32;
        let mut node = Vibrato::new(72000, 4800, 256, 2.0, 10.0);
        node.prepare("test", sr, 512);

        let contour = vec![0.0f32; sr as usize * 2];
        let (rate, depth) = run_through_blocks(&mut node, &contour, 512);

        assert_eq!(rate, 0.0);
        assert_eq!(depth, 0.0);
    }
}
