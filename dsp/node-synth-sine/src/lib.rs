use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;
use std::f32::consts::TAU;

pub struct SynthSine {
    freq: f32,
    amplitude: f32,
    /// Initial phase offset in radians, applied in prepare() so tests can set a known start.
    initial_phase: f32,
    phase: f32,
    sample_rate: f32,
}

impl SynthSine {
    fn new(freq: f32, amplitude: f32, initial_phase: f32) -> Self {
        Self {
            freq,
            amplitude,
            initial_phase,
            phase: 0.0,
            sample_rate: 48000.0,
        }
    }
}

impl Node for SynthSine {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        self.phase = self.initial_phase;
    }

    fn reset(&mut self) {
        self.phase = self.initial_phase;
    }

    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if outputs.is_empty() {
            return;
        }
        let phase_inc = TAU * self.freq / self.sample_rate;
        for sample in &mut outputs[0][..nframes] {
            *sample = self.amplitude * self.phase.sin();
            self.phase = (self.phase + phase_inc) % TAU;
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "SynthSine",
        vec![],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![
            ParamSpec {
                name: "freq",
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
                name: "phase",
                default: 0.0,
                min: 0.0,
                max: std::f64::consts::TAU,
                unit: "radians",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let freq = *params.get("freq").unwrap_or(&440.0) as f32;
            let amplitude = *params.get("amplitude").unwrap_or(&1.0) as f32;
            let initial_phase = *params.get("phase").unwrap_or(&0.0) as f32;
            Box::new(SynthSine::new(freq, amplitude, initial_phase)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn sine_generates_correct_frequency_and_amplitude() {
        let mut node = SynthSine::new(440.0, 0.5, 0.0);
        node.prepare("test", 48000, 512);

        let mut out = vec![0.0f32; 512];
        {
            let mut out_ref: Vec<&mut [f32]> = vec![out.as_mut_slice()];
            node.process(&[], &mut out_ref, 512);
        }

        // Check first sample: sin(0) = 0
        assert!(
            (out[0]).abs() < 1e-6,
            "first sample should be ~0, got {}",
            out[0]
        );

        // Check amplitude: max should be close to 0.5
        let max = out.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            max > 0.49 && max <= 0.5,
            "amplitude peak should be ~0.5, got {max}"
        );

        // Check frequency by counting zero crossings
        let crossings: usize = out
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        // 440 Hz at 48000 Hz SR: ~5.87 cycles in 512 samples → ~11-12 zero crossings
        let expected_cycles: f32 = 440.0 * 512.0 / 48000.0;
        let expected_crossings = (expected_cycles * 2.0).round() as usize;
        assert!(
            crossings.abs_diff(expected_crossings) <= 2,
            "expected ~{expected_crossings} zero crossings, got {crossings}"
        );

        // Verify specific sample value
        let phase_inc = TAU * 440.0 / 48000.0;
        let expected_sample_4 = 0.5 * (phase_inc * 4.0).sin();
        assert!(
            (out[4] - expected_sample_4).abs() < 1e-5,
            "sample[4] = {}, expected {}",
            out[4],
            expected_sample_4
        );
    }

    #[test]
    fn phase_param_affects_first_sample() {
        // With initial phase = π/2, first sample should be amplitude * sin(π/2) = amplitude
        let initial_phase = std::f32::consts::FRAC_PI_2;
        let mut node = SynthSine::new(440.0, 0.5, initial_phase);
        node.prepare("test", 48000, 512);

        let mut out = vec![0.0f32; 512];
        {
            let mut out_ref: Vec<&mut [f32]> = vec![out.as_mut_slice()];
            node.process(&[], &mut out_ref, 512);
        }

        // sin(π/2) = 1.0, so first sample = 0.5 * 1.0 = 0.5
        assert!(
            (out[0] - 0.5).abs() < 1e-6,
            "first sample with phase=π/2 should be 0.5, got {}",
            out[0]
        );
    }

    #[test]
    fn phase_continuity_across_blocks() {
        // B3: one call of N samples must equal two calls of N/2 samples each.
        let sr = 48000u32;
        let n = 1024usize;

        // Buffer A: single process call.
        let mut node_a = SynthSine::new(440.0, 0.5, 0.0);
        node_a.prepare("test", sr, n);
        let mut buf_a = vec![0.0f32; n];
        {
            let mut out_ref: Vec<&mut [f32]> = vec![buf_a.as_mut_slice()];
            node_a.process(&[], &mut out_ref, n);
        }

        // Buffer B: two process calls of n/2 each.
        let mut node_b = SynthSine::new(440.0, 0.5, 0.0);
        node_b.prepare("test", sr, n);
        let mut buf_b = vec![0.0f32; n];
        {
            let (first, second) = buf_b.split_at_mut(n / 2);
            let mut out_ref: Vec<&mut [f32]> = vec![first];
            node_b.process(&[], &mut out_ref, n / 2);
            let mut out_ref: Vec<&mut [f32]> = vec![second];
            node_b.process(&[], &mut out_ref, n / 2);
        }

        assert_eq!(
            buf_a, buf_b,
            "output must be identical regardless of how samples are split across process() calls"
        );
    }

    #[test]
    fn freq_param_has_hz_unit() {
        let mut registry = NodeRegistry::new();
        register(&mut registry);
        let params = registry.parameters("SynthSine").unwrap();
        let freq_param = params.iter().find(|p| p.name == "freq").unwrap();
        assert_eq!(freq_param.unit, "Hz");
    }
}
