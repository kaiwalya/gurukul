use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;
use std::f32::consts::TAU;

pub struct SineSource {
    freq: f32,
    amplitude: f32,
    phase: f32,
    sample_rate: f32,
}

impl SineSource {
    fn new(freq: f32, amplitude: f32) -> Self {
        Self {
            freq,
            amplitude,
            phase: 0.0,
            sample_rate: 48000.0,
        }
    }
}

impl Node for SineSource {
    fn declare_ports(&self) -> (Vec<PortSpec>, Vec<PortSpec>) {
        (
            vec![],
            vec![PortSpec {
                name: "audio_out",
                ty: PortType::Audio,
            }],
        )
    }

    fn declare_parameters(&self) -> Vec<ParamSpec> {
        vec![
            ParamSpec {
                name: "freq",
                default: 440.0,
                min: 20.0,
                max: 20000.0,
            },
            ParamSpec {
                name: "amplitude",
                default: 1.0,
                min: 0.0,
                max: 1.0,
            },
        ]
    }

    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        self.phase = 0.0;
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
    registry.register(
        "SineSource",
        Box::new(|params: &HashMap<String, f64>| {
            let freq = *params.get("freq").unwrap_or(&440.0) as f32;
            let amplitude = *params.get("amplitude").unwrap_or(&1.0) as f32;
            Box::new(SineSource::new(freq, amplitude)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn sine_generates_correct_frequency_and_amplitude() {
        let mut node = SineSource::new(440.0, 0.5);
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
}
