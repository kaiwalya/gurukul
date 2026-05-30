//! Build the coach's pitch engine from the embedded `coach.json` world.
//!
//! The world JSON is `include_str!`'d at compile time so the binary
//! carries no runtime file dependency — a host can ship as a single
//! executable and the engine still spins up on the worker thread.

use engine::{Engine, EngineError, NodeRegistry, World};

/// The world spec that wires `audio_in` → PitchYin → `f0_hz`. Embedded
/// at compile time so the adapter is self-contained.
const COACH_WORLD_JSON: &str = include_str!("../../../dsp/worlds/coach.json");

/// Build the pitch-detection engine.
///
/// Registers PitchYin, deserialises the embedded world, and calls
/// `Engine::build` at the supplied sample rate and block size. The
/// block size is expected to equal PitchYin's `hop` (512) so the
/// engine emits one f0 estimate per block.
///
/// Errors from any of the three steps are flattened to `String`; the
/// worker logs and exits rather than trying to recover — a programmer
/// error here means coach.json or the engine code is broken, and the
/// fix is a code change, not a runtime retry.
pub(crate) fn build_pitch_engine(sample_rate: u32, block_size: usize) -> Result<Engine, String> {
    let mut registry = NodeRegistry::new();
    node_pitch_yin::register(&mut registry);
    node_onset::register(&mut registry);
    node_breath::register(&mut registry);
    node_vibrato::register(&mut registry);

    let world: World =
        serde_json::from_str(COACH_WORLD_JSON).map_err(|e| format!("parse coach.json: {e}"))?;

    Engine::build(&world, &registry, sample_rate, block_size)
        .map_err(|e: EngineError| format!("engine build: {e}"))
}
