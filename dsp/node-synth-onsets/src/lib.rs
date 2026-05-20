//! Onset oracle synth: emits sine bursts at a fixed BPM, each burst a short
//! ADSR-shaped note. Paired with `node-onset`'s analyzer for the Tier-1
//! oracle loop.
//!
//! Parameters:
//!   bpm                — beats (onsets) per minute. Default 120.
//!   note_freq          — sine pitch within each burst, Hz. Default 440.
//!   note_duration_s    — duration of each burst before silence. Default 0.2 s.
//!   amplitude          — peak amplitude. Default 0.5.
//!
//! Output:
//!   audio_out [Audio]
//!
//! Each onset starts a fresh attack envelope. Inter-onset interval is
//! 60 / bpm seconds. Realtime-safe by construction (no allocation in
//! process()).

use engine::{Node, NodeRegistry, ParamSpec, PortSpec, PortType};
use std::collections::HashMap;
use std::f32::consts::TAU;

pub struct SynthOnsets {
    bpm: f32,
    note_freq: f32,
    note_duration_s: f32,
    amplitude: f32,

    sample_rate: f32,
    phase: f32,
    // Samples elapsed since the start of the most recent onset.
    samples_since_onset: u64,
    // Samples between onsets (computed from bpm in prepare()).
    samples_per_onset: u64,
    // Samples in one note's audible portion (computed from note_duration_s).
    note_samples: u64,
    // Total samples processed; used to schedule the *next* onset.
    total_samples: u64,
}

impl SynthOnsets {
    fn new(bpm: f32, note_freq: f32, note_duration_s: f32, amplitude: f32) -> Self {
        Self {
            bpm,
            note_freq,
            note_duration_s,
            amplitude,
            sample_rate: 48000.0,
            phase: 0.0,
            samples_since_onset: 0,
            samples_per_onset: 0,
            note_samples: 0,
            total_samples: 0,
        }
    }

    /// Simple linear AR envelope: 5 ms attack, then sustained, then 20 ms
    /// release as the note ends. Gives the analyzer a clear leading edge
    /// without being a bare step (which is unrealistic for any acoustic
    /// instrument).
    fn envelope(&self, n_in_note: u64) -> f32 {
        let attack = (self.sample_rate * 0.005) as u64; // 5 ms
        let release = (self.sample_rate * 0.020) as u64; // 20 ms
        if n_in_note < attack {
            n_in_note as f32 / attack.max(1) as f32
        } else if n_in_note + release >= self.note_samples {
            let remaining = self.note_samples.saturating_sub(n_in_note);
            remaining as f32 / release.max(1) as f32
        } else {
            1.0
        }
    }
}

impl Node for SynthOnsets {
    fn prepare(&mut self, _id: &str, sample_rate: u32, _block_size: usize) {
        self.sample_rate = sample_rate as f32;
        let beats_per_sec = self.bpm / 60.0;
        self.samples_per_onset = (sample_rate as f32 / beats_per_sec) as u64;
        self.note_samples = (sample_rate as f32 * self.note_duration_s) as u64;
        self.phase = 0.0;
        self.samples_since_onset = 0;
        self.total_samples = 0;
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.samples_since_onset = 0;
        self.total_samples = 0;
    }

    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
        if outputs.is_empty() {
            return;
        }
        let phase_inc = TAU * self.note_freq / self.sample_rate;
        for sample in &mut outputs[0][..nframes] {
            // First sample of the run is an onset (samples_since_onset == 0).
            // After samples_per_onset samples, retrigger.
            if self.total_samples > 0 && self.samples_since_onset >= self.samples_per_onset {
                self.samples_since_onset = 0;
                self.phase = 0.0;
            }
            // Only audible during the note window; silent after note_samples
            // until the next onset.
            let env = if self.samples_since_onset < self.note_samples {
                self.envelope(self.samples_since_onset)
            } else {
                0.0
            };
            *sample = self.amplitude * env * self.phase.sin();
            self.phase = (self.phase + phase_inc) % TAU;
            self.samples_since_onset += 1;
            self.total_samples += 1;
        }
    }
}

pub fn register(registry: &mut NodeRegistry) {
    registry.register_full(
        "SynthOnsets",
        vec![],
        vec![PortSpec {
            name: "audio_out",
            ty: PortType::Audio,
        }],
        vec![
            ParamSpec {
                name: "bpm",
                default: 120.0,
                min: 20.0,
                max: 400.0,
                unit: "bpm",
            },
            ParamSpec {
                name: "note_freq",
                default: 440.0,
                min: 20.0,
                max: 20000.0,
                unit: "Hz",
            },
            ParamSpec {
                name: "note_duration_s",
                default: 0.2,
                min: 0.01,
                max: 5.0,
                unit: "s",
            },
            ParamSpec {
                name: "amplitude",
                default: 0.5,
                min: 0.0,
                max: 1.0,
                unit: "",
            },
        ],
        Box::new(|params: &HashMap<String, f64>| {
            let bpm = *params.get("bpm").unwrap_or(&120.0) as f32;
            let note_freq = *params.get("note_freq").unwrap_or(&440.0) as f32;
            let note_duration_s = *params.get("note_duration_s").unwrap_or(&0.2) as f32;
            let amplitude = *params.get("amplitude").unwrap_or(&0.5) as f32;
            Box::new(SynthOnsets::new(bpm, note_freq, note_duration_s, amplitude)) as Box<dyn Node>
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node: &mut SynthOnsets, nframes: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; nframes];
        {
            let mut outs: Vec<&mut [f32]> = vec![out.as_mut_slice()];
            node.process(&[], &mut outs, nframes);
        }
        out
    }

    #[test]
    fn first_sample_starts_at_zero_phase() {
        let mut node = SynthOnsets::new(120.0, 440.0, 0.2, 0.5);
        node.prepare("test", 48000, 64);
        let out = run(&mut node, 1);
        // Phase = 0 → sample = amp * env(0) * sin(0) = 0.
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn silence_between_notes() {
        // 120 bpm → onset every 0.5 s (24000 samples). Note duration 0.2 s
        // (9600 samples). The 12000th sample should be in the silent gap.
        let mut node = SynthOnsets::new(120.0, 440.0, 0.2, 0.5);
        node.prepare("test", 48000, 512);
        let out = run(&mut node, 16000);
        // Sample around 12000 (between note 1 end at ~9600 and note 2 onset at 24000).
        assert_eq!(out[12000], 0.0, "should be silent between onsets");
    }

    #[test]
    fn second_onset_retriggers_envelope() {
        // 240 bpm → onset every 0.25 s (12000 samples).
        let mut node = SynthOnsets::new(240.0, 440.0, 0.05, 0.5);
        node.prepare("test", 48000, 512);
        let out = run(&mut node, 13000);
        // At sample 12000 we just retriggered: env at 0 → sample ≈ 0.
        // At samples 12001..12100 envelope ramps up; signal grows in magnitude.
        let early = out[12001..12100].iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(
            early > 0.001,
            "second onset should produce audible signal: got {early}"
        );
    }
}
