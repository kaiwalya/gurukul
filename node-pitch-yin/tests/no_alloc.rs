/// Verifies that PitchYin::process() performs zero heap allocations on the hot path.
///
/// The global allocator is replaced with AllocDisabler for this test binary.
/// Any allocation inside assert_no_alloc(|| { ... }) aborts the process.
#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use engine::Node;
use node_pitch_yin::PitchYin;

const SR: u32 = 48000;
const BLOCK: usize = 512;

/// 440 Hz sine at the given amplitude, `n_samples` long.
fn sine_440(n_samples: usize) -> Vec<f32> {
    (0..n_samples)
        .map(|i| 0.5_f64 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SR as f64).sin())
        .map(|s| s as f32)
        .collect()
}

#[test]
fn process_does_not_allocate() {
    // prepare() may allocate — that is fine and expected.
    let mut node = PitchYin::new(2048, 512, 50.0, 2000.0, 0.1);
    node.prepare("no_alloc_test", SR, BLOCK);

    // Generate enough audio for several analyses: 25 blocks × 512 = 12800 samples.
    // With hop=512, every block triggers one analysis after the ring fills (2048 samples).
    let signal = sine_440(BLOCK * 25);
    let mut out = vec![0.0f32; BLOCK];

    // All process() calls must be allocation-free.
    assert_no_alloc(|| {
        for chunk in signal.chunks(BLOCK) {
            let nframes = chunk.len();
            out[..nframes].fill(0.0);
            node.process(&[chunk], &mut [&mut out[..nframes]], nframes);
        }
    });
}
