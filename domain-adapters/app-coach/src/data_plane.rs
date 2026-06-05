//! The data plane: the audio-rate side of the coach.
//!
//! Lives entirely behind the control plane — the control plane spawns
//! a [`DataPlane`] in `do_start_session` and tears it down in
//! `do_stop_session`. Heads never touch it directly; they read the
//! latest feature snapshot via [`AppCoach::latest_features`], which
//! under the hood is just `feature_publisher.load()`.
//!
//! # Wiring
//!
//! ```text
//! cpal callback (RT)                worker thread
//!   │                                    │
//!   │ samples ─► rtrb::Producer ─► rtrb::Consumer
//!   ↓                                    │
//!                                        │ engine.in_port(mic).copy_from(...)
//!                                        │ engine.process_block(BLOCK_FRAMES)
//!                                        │ read pitch/onset/breath/vibrato out-ports
//!                                        │ publisher.store(Some(snapshot))
//!                                        ▼
//!                                   head: coach.latest_features()
//! ```
//!
//! # Block-size discipline
//!
//! The cpal callback hands us variable-size [`CaptureFrame`]s (~480 at
//! 48kHz with the default 100-buffer). The engine runs in fixed-size
//! blocks of [`BLOCK_FRAMES`] = 512 — chosen to match PitchYin's
//! default hop, so the engine emits one f0 estimate per block.
//!
//! # Realtime safety
//!
//! The RT-side `push_samples` only touches the ring producer, which
//! allocates nothing and never blocks. On ring full it drops samples
//! and counts them (logged at WARN, but only by the worker — never
//! the RT thread). The worker side allocates only at construction
//! (engine build, ringbuffer alloc); the hot loop is alloc-free.

use crate::pitch_world::build_pitch_engine;
use arc_swap::ArcSwap;
use domain_ports::app_coach::FeatureSnapshot;
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use domain_ports::{tel_info, tel_warn};
use engine::{Engine, InPortHandle, OutPortHandle};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Engine block size = PitchYin hop. One f0 estimate per block.
pub(crate) const BLOCK_FRAMES: usize = 512;

/// SPSC ring capacity in samples. ~85ms at 48kHz — comfortably larger
/// than any reasonable cpal buffer + worker scheduling jitter.
const RING_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------
// Public: spawn / teardown
// ---------------------------------------------------------------------

pub(crate) struct DataPlane {
    quit: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Count of samples the RT side had to drop because the worker
    /// fell behind. Inspected on shutdown for a single summary log.
    samples_dropped: Arc<AtomicU64>,
}

/// What [`DataPlane::start`] hands back to the control plane. Each
/// half goes to a different home:
///
/// - `data_plane` stays on the control plane (owns the worker handle).
/// - `producer` is moved into the cpal capture callback (RT thread).
/// - `samples_dropped` is shared with the callback so it can bump
///   the counter when the worker ring is full.
pub(crate) struct DataPlaneStartup {
    pub(crate) data_plane: DataPlane,
    pub(crate) producer: Producer<f32>,
    pub(crate) samples_dropped: Arc<AtomicU64>,
}

impl DataPlane {
    /// Spawn the worker thread and return a [`DataPlaneStartup`] bundle.
    /// The producer is moved into the capture callback so the RT thread
    /// can push samples; the [`DataPlane`] handle is held by the control
    /// plane so it can [`stop`](Self::stop) the worker on session
    /// teardown.
    pub(crate) fn start(deps: DataPlaneDeps) -> Result<DataPlaneStartup, DataPlaneError> {
        let (producer, consumer) = RingBuffer::<f32>::new(RING_CAPACITY);

        // Build the engine on the *worker* thread, not here — engines
        // hold trait objects that aren't always `Send`, and the
        // worker is the only place process_block is ever called.
        // The `World` itself is just data and crosses the boundary.
        let quit = Arc::new(AtomicBool::new(false));
        let samples_dropped = Arc::new(AtomicU64::new(0));
        let feature_publisher = Arc::clone(&deps.feature_publisher);
        let clock = Arc::clone(&deps.clock);
        let telemetry = Arc::clone(&deps.telemetry);
        let inspect = Arc::clone(&deps.inspect);
        let quit_for_thread = Arc::clone(&quit);
        let dropped_for_callback = Arc::clone(&samples_dropped);
        let sample_rate = deps.sample_rate;

        let worker = thread::Builder::new()
            .name("app-coach-data".into())
            .spawn(move || {
                run_worker(WorkerArgs {
                    consumer,
                    sample_rate,
                    quit: quit_for_thread,
                    feature_publisher,
                    clock,
                    telemetry,
                    inspect,
                });
            })
            .map_err(|e| DataPlaneError::Spawn(e.to_string()))?;

        Ok(DataPlaneStartup {
            data_plane: Self {
                quit,
                worker: Some(worker),
                samples_dropped,
            },
            producer,
            samples_dropped: dropped_for_callback,
        })
    }

    /// Signal the worker to quit and join it. Idempotent in practice
    /// — the control plane only calls this once per session.
    pub(crate) fn stop(mut self, telemetry: &dyn Telemetry) {
        self.quit.store(true, Ordering::Release);
        if let Some(h) = self.worker.take() {
            // Worker checks `quit` every iteration of its drain loop
            // (≤BLOCK_FRAMES samples / a few ms), so the join here is
            // bounded even without a deadline.
            let _ = h.join();
        }
        let dropped = self.samples_dropped.load(Ordering::Acquire);
        if dropped > 0 {
            tel_warn!(
                telemetry,
                "data-plane: RT samples dropped (worker fell behind)",
                samples = dropped,
            );
        }
    }
}

pub(crate) struct DataPlaneDeps {
    pub(crate) sample_rate: u32,
    pub(crate) feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) telemetry: Arc<dyn Telemetry>,
    pub(crate) inspect: Arc<crate::inspect::InspectShared>,
}

#[derive(Debug)]
pub(crate) enum DataPlaneError {
    /// Worker thread failed to spawn (OS resource exhaustion).
    Spawn(String),
}

impl std::fmt::Display for DataPlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(s) => write!(f, "data plane worker spawn failed: {s}"),
        }
    }
}

// ---------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------

struct WorkerArgs {
    consumer: Consumer<f32>,
    sample_rate: u32,
    quit: Arc<AtomicBool>,
    feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    clock: Arc<dyn Clock>,
    telemetry: Arc<dyn Telemetry>,
    inspect: Arc<crate::inspect::InspectShared>,
}

fn run_worker(args: WorkerArgs) {
    let WorkerArgs {
        mut consumer,
        sample_rate,
        quit,
        feature_publisher,
        clock,
        telemetry,
        inspect,
    } = args;

    // Build the engine on this thread. Errors here go nowhere visible
    // to the head — the control plane has already moved past
    // do_start_session by the time we run. We log and exit; the
    // pitch publisher stays at `None`, which is the natural "no
    // pitch" state.
    let mut engine = match build_pitch_engine(sample_rate, BLOCK_FRAMES) {
        Ok(e) => e,
        Err(e) => {
            tel_warn!(
                &*telemetry,
                "data-plane: engine build failed; pitch will be unavailable",
                error = e.to_string(),
            );
            return;
        }
    };

    // Publish the node/port surface for the debug picker. Cleared on
    // teardown by the control plane (via InspectShared::clear).
    inspect.publish_node_ports(&engine);

    let ports = match resolve_ports(&engine) {
        Ok(p) => p,
        Err(e) => {
            tel_warn!(
                &*telemetry,
                "data-plane: boundary ports missing from coach.json; features will be unavailable",
                error = e,
            );
            return;
        }
    };

    tel_info!(
        &*telemetry,
        "data-plane: worker up",
        sample_rate = sample_rate,
        block_frames = BLOCK_FRAMES as u32,
    );

    // Scratch buffer for one block worth of samples drained from the
    // ring. Heap-allocated once, before the hot loop.
    let mut block: Vec<f32> = vec![0.0; BLOCK_FRAMES];

    // Monotonic block index. Heads use it to detect missed taps.
    let mut block_seq: u64 = 0;

    while !quit.load(Ordering::Acquire) {
        // Wait until at least one block is available, or quit.
        if consumer.slots() < BLOCK_FRAMES {
            // No full block yet — sleep briefly (back off scheduling
            // pressure on the audio thread). cpal at 48k/100 hands us
            // ~10ms per callback, so a 2ms poll is comfortably below
            // arrival cadence.
            thread::sleep(Duration::from_millis(2));
            continue;
        }

        // Pull exactly BLOCK_FRAMES samples.
        for slot in block.iter_mut() {
            // We just checked slots() >= BLOCK_FRAMES; pop_one cannot
            // fail. But handle defensively in case the producer was
            // dropped between the check and the pop.
            match consumer.pop() {
                Ok(v) => *slot = v,
                Err(_) => {
                    // Producer dropped (cpal stream ended). Drain what
                    // we have, then exit on the next loop tick.
                    return;
                }
            }
        }

        // Feed the engine.
        engine.in_port(ports.mic).copy_from_slice(&block);
        engine.process_block(BLOCK_FRAMES);

        // Publish a tap of the currently-selected port (if any) for
        // the debug panel. Cheap no-op when nothing is selected.
        block_seq = block_seq.wrapping_add(1);
        inspect.tap_if_selected(&engine, block_seq);

        // Read every feature for this hop and publish as one snapshot.
        // Always publish — heads can tell voiced from unvoiced via
        // the f0_hz == 0.0 sentinel. Publishing unvoiced too lets
        // heads detect "alive but silent" vs "stalled".
        let snapshot = FeatureSnapshot {
            f0_hz: engine.out_port(ports.pitch)[0],
            confidence: engine.out_port(ports.confidence)[0],
            onset: engine.out_port(ports.onset)[0],
            breath: engine.out_port(ports.breath)[0],
            vibrato_rate: engine.out_port(ports.vibrato_rate)[0],
            vibrato_depth: engine.out_port(ports.vibrato_depth)[0],
            t_ms: clock.now_ms(),
        };
        feature_publisher.store(Arc::new(Some(snapshot)));
    }

    tel_info!(&*telemetry, "data-plane: worker down");
}

struct ResolvedPorts {
    mic: InPortHandle,
    pitch: OutPortHandle,
    confidence: OutPortHandle,
    onset: OutPortHandle,
    breath: OutPortHandle,
    vibrato_rate: OutPortHandle,
    vibrato_depth: OutPortHandle,
}

fn resolve_ports(engine: &Engine) -> Result<ResolvedPorts, String> {
    let mic = engine
        .resolve_in_port("mic")
        .map_err(|e| format!("mic: {e:?}"))?;
    let pitch = engine
        .resolve_out_port("pitch")
        .map_err(|e| format!("pitch: {e:?}"))?;
    let confidence = engine
        .resolve_out_port("confidence")
        .map_err(|e| format!("confidence: {e:?}"))?;
    let onset = engine
        .resolve_out_port("onset")
        .map_err(|e| format!("onset: {e:?}"))?;
    let breath = engine
        .resolve_out_port("breath")
        .map_err(|e| format!("breath: {e:?}"))?;
    let vibrato_rate = engine
        .resolve_out_port("vibrato_rate")
        .map_err(|e| format!("vibrato_rate: {e:?}"))?;
    let vibrato_depth = engine
        .resolve_out_port("vibrato_depth")
        .map_err(|e| format!("vibrato_depth: {e:?}"))?;
    Ok(ResolvedPorts {
        mic,
        pitch,
        confidence,
        onset,
        breath,
        vibrato_rate,
        vibrato_depth,
    })
}

// ---------------------------------------------------------------------
// RT-side helper used by the capture callback closure
// ---------------------------------------------------------------------

/// Push samples from the cpal callback into the ring. Increments the
/// shared `samples_dropped` counter for any samples the ring couldn't
/// hold. Realtime-safe: no allocation, no lock, no syscall (rtrb
/// writes via atomic head/tail).
pub(crate) fn push_samples(
    producer: &mut Producer<f32>,
    samples: &[f32],
    samples_dropped: &AtomicU64,
) {
    let mut dropped: u64 = 0;
    for &s in samples {
        if producer.push(s).is_err() {
            dropped += 1;
        }
    }
    if dropped > 0 {
        samples_dropped.fetch_add(dropped, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::clock::TestClock;
    use domain_ports::telemetry::TestTelemetry;
    use std::time::Instant;

    /// Loopback: synthesise a 440Hz sine, push it through the ring,
    /// and confirm the worker publishes f0 ≈ 440. Covers the seam
    /// between push_samples and the ArcSwap publisher — YIN itself
    /// is covered by node-pitch-yin's own tests.
    #[test]
    fn sine_440_round_trips_to_publisher() {
        const SR: u32 = 48_000;
        const F0: f32 = 440.0;

        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        let telemetry: Arc<dyn Telemetry> = Arc::new(TestTelemetry::new(Arc::clone(&clock)));
        let feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>> =
            Arc::new(ArcSwap::from_pointee(None));

        let DataPlaneStartup {
            data_plane,
            mut producer,
            samples_dropped: dropped,
        } = DataPlane::start(DataPlaneDeps {
            sample_rate: SR,
            feature_publisher: Arc::clone(&feature_publisher),
            clock,
            telemetry: Arc::clone(&telemetry),
            inspect: crate::inspect::InspectShared::new(),
        })
        .expect("data plane starts");

        // Feed enough audio for YIN to lock: window=2048 must fill,
        // plus a few hops for the published estimate to stabilise.
        // 8192 samples = 4 windows, takes ~170ms of audio time but
        // gets pushed and drained as fast as the worker can keep up.
        let mut phase: f32 = 0.0;
        let step = 2.0 * std::f32::consts::PI * F0 / SR as f32;
        let mut chunk = [0.0_f32; 256];
        let total_samples = 8192;
        let mut pushed = 0;
        while pushed < total_samples {
            for s in chunk.iter_mut() {
                *s = (phase).sin() * 0.5;
                phase += step;
            }
            // Push in 256-sample bursts to mimic cpal's variable-size
            // callbacks; the ring is 4096 deep so we may need to wait
            // for the worker to drain.
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut written = 0;
            while written < chunk.len() {
                if producer.push(chunk[written]).is_ok() {
                    written += 1;
                } else if Instant::now() > deadline {
                    panic!("ring stayed full; worker not draining");
                } else {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            pushed += chunk.len();
        }

        // Poll the publisher until we see a voiced reading within
        // 1Hz of 440, or time out.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut last_f0: f32 = 0.0;
        let f0 = loop {
            if let Some(snapshot) = **feature_publisher.load() {
                last_f0 = snapshot.f0_hz;
                if snapshot.f0_hz > 0.0 && (snapshot.f0_hz - F0).abs() < 1.0 {
                    break snapshot.f0_hz;
                }
            }
            if Instant::now() > deadline {
                panic!("publisher never reported f0 ≈ {F0}Hz (last seen: {last_f0}Hz)");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!((f0 - F0).abs() < 1.0, "expected f0 ≈ {F0}, got {f0}");

        // Tear down cleanly. Drop the producer first so the worker's
        // pop loop sees the channel close; then stop joins the thread.
        drop(producer);
        data_plane.stop(&*telemetry);
        assert_eq!(dropped.load(Ordering::Acquire), 0, "no drops expected");
    }
}
