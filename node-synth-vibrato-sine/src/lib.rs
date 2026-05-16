use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;
use std::f32::consts::TAU;

pub struct SynthVibratoSine {
    carrier_freq: f32,
    amplitude: f32,
    vibrato_rate: f32,
    vibrato_depth_cents: f32,
    carrier_phase: f32,
    vibrato_phase: f32,
    sample_rate: f32,
}

impl SynthVibratoSine {
    fn new(carrier_freq: f32, amplitude: f32, vibrato_rate: f32, vibrato_depth_cents: f32) -> Self {
        Self {
            carrier_freq,
            amplitude,
            vibrato_rate,
            vibrato_depth_cents,
            carrier_phase: 0.0,
            vibrato_phase: 0.0,
            sample_rate: 48000.0,
        }
    }
}

impl Node for SynthVibratoSine {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        self.carrier_phase = 0.0;
        self.vibrato_phase = 0.0;
    }

    fn reset(&mut self) {
        self.carrier_phase = 0.0;
        self.vibrato_phase = 0.0;
    }

    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if outputs.is_empty() {
            return;
        }
        let vibrato_phase_inc = TAU * self.vibrato_rate / self.sample_rate;
        let depth = self.vibrato_depth_cents;
        for sample in &mut outputs[0][..nframes] {
            // Instantaneous frequency: carrier modulated by vibrato LFO in cents
            let inst_freq =
                self.carrier_freq * 2.0f32.powf((depth / 1200.0) * self.vibrato_phase.sin());
            let carrier_phase_inc = TAU * inst_freq / self.sample_rate;

            *sample = self.amplitude * self.carrier_phase.sin();
            self.carrier_phase = (self.carrier_phase + carrier_phase_inc) % TAU;
            self.vibrato_phase = (self.vibrato_phase + vibrato_phase_inc) % TAU;
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "SynthVibratoSine",
        vec![],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![
            ParamSpec {
                name: "carrier_freq",
                default: 440.0,
                min: 20.0,
                max: 20000.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "amplitude",
                default: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
            ParamSpec {
                name: "vibrato_rate",
                default: 5.0,
                min: 0.0,
                max: 30.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "vibrato_depth_cents",
                default: 50.0,
                min: 0.0,
                max: 1200.0,
                unit: "cents",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let carrier_freq = *params.get("carrier_freq").unwrap_or(&440.0) as f32;
            let amplitude = *params.get("amplitude").unwrap_or(&1.0) as f32;
            let vibrato_rate = *params.get("vibrato_rate").unwrap_or(&5.0) as f32;
            let vibrato_depth_cents = *params.get("vibrato_depth_cents").unwrap_or(&50.0) as f32;
            Box::new(SynthVibratoSine::new(
                carrier_freq,
                amplitude,
                vibrato_rate,
                vibrato_depth_cents,
            )) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn run_node(node: &mut SynthVibratoSine, nframes: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; nframes];
        {
            let mut out_ref: Vec<&mut [f32]> = vec![out.as_mut_slice()];
            node.process(&[], &mut out_ref, nframes);
        }
        out
    }

    #[test]
    fn zero_depth_closed_form_match() {
        // With vibrato_depth_cents = 0, output should match amplitude * sin(TAU * freq * i / sr).
        // Closed-form vs iterative f32 accumulation drifts slightly due to repeated %TAU wrapping;
        // epsilon 1e-3 catches algorithmic errors without being tripped by f32 precision drift.
        let sr = 48000u32;
        let freq = 440.0f32;
        let amp = 0.8f32;
        let nframes = 1024;

        let mut vibrato = SynthVibratoSine::new(freq, amp, 5.0, 0.0);
        vibrato.prepare("test", sr, nframes);
        let vibrato_out = run_node(&mut vibrato, nframes);

        for (i, &v) in vibrato_out.iter().enumerate() {
            let expected = amp * (TAU * freq * i as f32 / sr as f32).sin();
            assert!(
                (v - expected).abs() < 1e-3,
                "sample {i}: vibrato={v}, closed-form={expected}, diff={}",
                (v - expected).abs()
            );
        }
    }

    #[test]
    fn instantaneous_frequency_range() {
        // Estimate instantaneous frequency via zero-crossing counts in 4096-sample windows.
        // Window size chosen for resolution: sr/4096 ≈ 11.7 Hz per half-crossing step, sufficient
        // to resolve ±50-cent deviations (≈ ±12.7 Hz) around 440 Hz within the 3% tolerance.
        let sr = 48000u32;
        let freq = 440.0f32;
        let amp = 1.0f32;
        let depth = 50.0f32;
        let vibrato_rate = 5.0f32;
        let nframes = 5 * 48000usize; // 5 seconds gives ~25 full vibrato cycles

        let mut node = SynthVibratoSine::new(freq, amp, vibrato_rate, depth);
        node.prepare("test", sr, nframes);
        let out = run_node(&mut node, nframes);

        let window = 4096usize;
        let mut window_freqs: Vec<f32> = Vec::new();

        for chunk in out.chunks(window) {
            if chunk.len() < window {
                break;
            }
            // Count zero crossings to estimate average frequency in this window.
            let crossings: usize = chunk
                .windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count();
            // frequency = (crossings/2) * (sr / window_len)
            let est_freq = (crossings as f32 / 2.0) * (sr as f32 / window as f32);
            window_freqs.push(est_freq);
        }

        let min_freq = window_freqs.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_freq = window_freqs
            .iter()
            .cloned()
            .fold(f32::NEG_INFINITY, f32::max);

        let expected_min = freq * 2.0f32.powf(-depth / 1200.0);
        let expected_max = freq * 2.0f32.powf(depth / 1200.0);

        assert!(
            (min_freq - expected_min).abs() / expected_min < 0.03,
            "min instantaneous freq {min_freq:.2} Hz not within 3% of expected {expected_min:.2} Hz"
        );
        assert!(
            (max_freq - expected_max).abs() / expected_max < 0.03,
            "max instantaneous freq {max_freq:.2} Hz not within 3% of expected {expected_max:.2} Hz"
        );

        // Vibrato rate: count local maxima in the frequency envelope.
        // Over 5 seconds at 5 Hz vibrato rate, expect ~25 peaks.
        let peaks: usize = window_freqs
            .windows(3)
            .filter(|w| w[1] > w[0] && w[1] >= w[2])
            .count();
        let expected_peaks = (vibrato_rate * nframes as f32 / sr as f32).round() as usize;
        assert!(
            peaks.abs_diff(expected_peaks) <= 3,
            "peak count {peaks} should be close to vibrato_rate×duration {expected_peaks}"
        );
    }

    #[test]
    fn deterministic_two_instances() {
        let sr = 48000u32;
        let nframes = 512;

        let mut a = SynthVibratoSine::new(440.0, 0.7, 6.0, 50.0);
        a.prepare("test_a", sr, nframes);
        let out_a = run_node(&mut a, nframes);

        let mut b = SynthVibratoSine::new(440.0, 0.7, 6.0, 50.0);
        b.prepare("test_b", sr, nframes);
        let out_b = run_node(&mut b, nframes);

        assert_eq!(
            out_a, out_b,
            "two fresh instances should produce identical output"
        );
    }

    #[test]
    fn phase_continuity_across_blocks() {
        // B3: one call of N samples must equal two calls of N/2 samples each.
        let sr = 48000u32;
        let n = 1024usize;

        // Buffer A: single process call.
        let mut node_a = SynthVibratoSine::new(440.0, 0.7, 6.0, 50.0);
        node_a.prepare("test", sr, n);
        let buf_a = run_node(&mut node_a, n);

        // Buffer B: two process calls of n/2 each.
        let mut node_b = SynthVibratoSine::new(440.0, 0.7, 6.0, 50.0);
        node_b.prepare("test", sr, n);
        let mut buf_b = Vec::with_capacity(n);
        buf_b.extend_from_slice(&run_node(&mut node_b, n / 2));
        buf_b.extend_from_slice(&run_node(&mut node_b, n / 2));

        assert_eq!(
            buf_a, buf_b,
            "output must be identical regardless of how samples are split across process() calls"
        );
    }
}
