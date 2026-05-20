//! Verifies that Onset::process() performs zero heap allocations on the hot
//! path. Mirrors node-pitch-yin/tests/no_alloc.rs and node-vibrato's.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use engine::Node;
use node_onset::Onset;

const SR: u32 = 48000;
const BLOCK: usize = 512;

/// Build a short burst pattern: 0.5 s repeating, 0.15 s sine on / 0.35 s off.
fn burst_pattern(seconds: f32, freq: f32, on_s: f32, period_s: f32) -> Vec<f32> {
    let n = (SR as f32 * seconds) as usize;
    let period = (SR as f32 * period_s) as usize;
    let on = (SR as f32 * on_s) as usize;
    (0..n)
        .map(|i| {
            let phase_in_period = i % period;
            if phase_in_period < on {
                let t = phase_in_period as f32 / SR as f32;
                0.5 * (std::f32::consts::TAU * freq * t).sin()
            } else {
                0.0
            }
        })
        .collect()
}

#[test]
fn process_does_not_allocate() {
    let mut node = Onset::new(480, 12.0, 4800, 0.001);
    node.prepare("no_alloc_test", SR, BLOCK);

    let signal = burst_pattern(3.0, 440.0, 0.15, 0.5);
    let mut out = vec![0.0f32; BLOCK];

    assert_no_alloc(|| {
        for chunk in signal.chunks(BLOCK) {
            let nframes = chunk.len();
            out[..nframes].fill(0.0);
            node.process(&[chunk], &mut [&mut out[..nframes]], nframes);
        }
    });
}
