use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;

const NUM_OCTAVES: usize = 7;

/// SplitMix64 hash — mixes a seed value before use so nearby seeds produce uncorrelated streams.
/// Returns the mixed value; the internal state increment does not need to persist (we call once).
fn splitmix64(state: u64) -> u64 {
    let state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Xorshift64 PRNG — avoids any external dependency.
fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Map a u64 to [-1.0, 1.0].
fn u64_to_f32(v: u64) -> f32 {
    // Shift to i64 range then normalise.
    (v as i64 as f64 / i64::MAX as f64) as f32
}

pub struct SynthPinkNoise {
    amplitude: f32,
    seed: u64,
    /// Xorshift PRNG state; initialised in prepare().
    state: u64,
    /// Per-octave running values for Voss-McCartney algorithm.
    octave_values: Vec<f32>,
    /// Counter used to decide which octave to update each sample.
    /// Starts at 1 so trailing_zeros(1)=0 (row 0 updates every sample as intended).
    counter: u64,
}

impl SynthPinkNoise {
    fn new(amplitude: f32, seed: u64) -> Self {
        Self {
            amplitude,
            seed,
            state: 1,
            octave_values: Vec::new(),
            counter: 1,
        }
    }
}

impl Node for SynthPinkNoise {
    fn prepare(&mut self, _id: &str, _sample_rate: u32, _block_size: usize) {
        // Run seed through SplitMix64 so nearby seeds (e.g. per-cell sweep indices) produce
        // uncorrelated xorshift64 streams. Guard against the mixed value being zero, which
        // would also get xorshift64 stuck.
        let mixed = splitmix64(if self.seed == 0 { 1 } else { self.seed });
        self.state = if mixed == 0 { 1 } else { mixed };
        // Allocate octave state here, not in process().
        self.octave_values = vec![0.0f32; NUM_OCTAVES];
        // Warm up octave values with one white sample each.
        for v in &mut self.octave_values {
            *v = u64_to_f32(xorshift64(&mut self.state));
        }
        // Start at 1 so trailing_zeros(1)=0: row 0 updates every sample.
        self.counter = 1;
    }

    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if outputs.is_empty() {
            return;
        }
        // Voss-McCartney: N octave rows, each uniform[-1,1] with variance 1/3.
        // Sum variance = N/3. Scale so output RMS ≈ amplitude.
        let scale = self.amplitude / ((NUM_OCTAVES as f32) / 3.0f32).sqrt();
        for sample in &mut outputs[0][..nframes] {
            // Update the octave corresponding to the lowest set bit of the counter.
            // counter starts at 1, so trailing_zeros picks row 0 on the first sample,
            // row 1 every 2nd sample, row k every 2^k samples — canonical Voss cadence.
            let octave = (self.counter.trailing_zeros() as usize).min(NUM_OCTAVES - 1);
            self.octave_values[octave] = u64_to_f32(xorshift64(&mut self.state));
            self.counter = self.counter.wrapping_add(1);

            // Sum of octave rows only — no extra white term.
            let pink: f32 = self.octave_values.iter().sum::<f32>();
            *sample = pink * scale;
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "SynthPinkNoise",
        vec![],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![
            ParamSpec {
                name: "amplitude",
                default: 0.1,
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
            let amplitude = *params.get("amplitude").unwrap_or(&0.1) as f32;
            let seed = *params.get("seed").unwrap_or(&1.0) as u64;
            Box::new(SynthPinkNoise::new(amplitude, seed)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_pink(seed: u64, nframes: usize) -> Vec<f32> {
        let mut node = SynthPinkNoise::new(0.1, seed);
        node.prepare("test", 48000, nframes);
        let mut out = vec![0.0f32; nframes];
        {
            let mut out_ref: Vec<&mut [f32]> = vec![out.as_mut_slice()];
            node.process(&[], &mut out_ref, nframes);
        }
        out
    }

    #[test]
    fn bit_exact_determinism() {
        let a = run_pink(42, 4096);
        let b = run_pink(42, 4096);
        assert_eq!(a, b, "same seed must produce identical samples");
    }

    #[test]
    fn different_seeds_differ() {
        let a = run_pink(1, 512);
        let b = run_pink(2, 512);
        assert_ne!(a, b, "different seeds must produce different samples");
    }

    #[test]
    fn rms_near_amplitude() {
        let nframes = 48000;
        let amplitude = 0.1f32;
        let out = run_pink(1, nframes);

        let rms =
            (out.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / nframes as f64).sqrt() as f32;
        // Voss-McCartney with 7 octave rows; allow 10% tolerance over 48000 samples.
        assert!(
            (rms - amplitude).abs() / amplitude < 0.10,
            "rms={rms:.4} should be within 10% of amplitude={amplitude}"
        );
    }

    #[test]
    fn no_nan_or_inf() {
        let out = run_pink(7, 4096);
        for (i, &s) in out.iter().enumerate() {
            assert!(s.is_finite(), "sample {i} is not finite: {s}");
        }
    }

    fn pearson(a: &[f32], b: &[f32]) -> f64 {
        let n = a.len() as f64;
        let mean_a = a.iter().map(|&x| x as f64).sum::<f64>() / n;
        let mean_b = b.iter().map(|&x| x as f64).sum::<f64>() / n;
        let mut num = 0.0f64;
        let mut den_a = 0.0f64;
        let mut den_b = 0.0f64;
        for (&x, &y) in a.iter().zip(b.iter()) {
            let da = x as f64 - mean_a;
            let db = y as f64 - mean_b;
            num += da * db;
            den_a += da * da;
            den_b += db * db;
        }
        num / (den_a.sqrt() * den_b.sqrt())
    }

    #[test]
    fn nearby_seeds_are_uncorrelated() {
        let n = 4096;
        let s1 = run_pink(1, n);
        let s2 = run_pink(2, n);
        let s3 = run_pink(3, n);
        let s4 = run_pink(4, n);

        for (pair, (a, b)) in [
            ("(1,2)", (&s1, &s2)),
            ("(2,3)", (&s2, &s3)),
            ("(3,4)", (&s3, &s4)),
        ] {
            let r = pearson(a, b).abs();
            assert!(
                r < 0.1,
                "seeds {pair} have |r|={r:.4}, expected < 0.1 (SplitMix64 mixing insufficient)"
            );
        }
    }
}
