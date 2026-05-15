//! Verifies that Breath::process() performs zero heap allocations on the
//! hot path. Mirrors node-onset/tests/no_alloc.rs.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use engine::Node;
use node_breath::Breath;
use node_synth_breath::SynthBreath;

const SR: u32 = 48000;
const BLOCK: usize = 512;

#[test]
fn process_does_not_allocate() {
    let mut synth = SynthBreath::new(1.0, 0.4, 0.15, 1);
    synth.prepare("synth", SR, BLOCK);
    let mut det = Breath::new(1024, 0.20, 0.10, 1e-5, 2400);
    det.prepare("det", SR, BLOCK);

    // Pre-render 3 s of breath audio.
    let total = (SR as usize) * 3;
    let mut signal = vec![0.0f32; total];
    {
        for chunk in signal.chunks_mut(BLOCK) {
            let nframes = chunk.len();
            let mut outs: Vec<&mut [f32]> = vec![chunk];
            synth.process(&[], &mut outs, nframes);
        }
    }

    let mut out = vec![0.0f32; BLOCK];
    assert_no_alloc(|| {
        for chunk in signal.chunks(BLOCK) {
            let nframes = chunk.len();
            out[..nframes].fill(0.0);
            det.process(&[chunk], &mut [&mut out[..nframes]], nframes);
        }
    });
}
