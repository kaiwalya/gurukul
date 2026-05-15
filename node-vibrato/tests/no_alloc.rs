/// Verifies that Vibrato::process() performs zero heap allocations on the
/// hot path. Mirrors node-pitch-yin/tests/no_alloc.rs.
///
/// The global allocator is replaced with AllocDisabler for this test binary.
/// Any allocation inside assert_no_alloc(|| { ... }) aborts the process.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use engine::Node;
use node_vibrato::Vibrato;

const SR: u32 = 48000;
const BLOCK: usize = 512;

/// Synthesise an f0 contour with vibrato of `rate_hz` and `depth_cents`.
fn vibrato_f0(seconds: f32, carrier_hz: f32, rate_hz: f32, depth_cents: f32) -> Vec<f32> {
    let n = (SR as f32 * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / SR as f32;
            carrier_hz
                * 2.0f32.powf((depth_cents / 1200.0) * (std::f32::consts::TAU * rate_hz * t).sin())
        })
        .collect()
}

#[test]
fn process_does_not_allocate() {
    // new() and prepare() may allocate; that's fine and expected.
    let mut node = Vibrato::new(72000, 4800, 256, 2.0, 10.0);
    node.prepare("no_alloc_test", SR, BLOCK);

    // 3 s of contour → enough blocks to drive ring-fill + many analyses.
    let signal = vibrato_f0(3.0, 440.0, 5.0, 50.0);
    let mut rate_out = vec![0.0f32; BLOCK];
    let mut depth_out = vec![0.0f32; BLOCK];

    assert_no_alloc(|| {
        for chunk in signal.chunks(BLOCK) {
            let nframes = chunk.len();
            rate_out[..nframes].fill(0.0);
            depth_out[..nframes].fill(0.0);
            // Stack-allocated [&mut [f32]; 2] array literal — no Vec, no heap.
            node.process(
                &[chunk],
                &mut [&mut rate_out[..nframes], &mut depth_out[..nframes]],
                nframes,
            );
        }
    });
}
