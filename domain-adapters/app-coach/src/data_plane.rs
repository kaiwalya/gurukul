//! The data plane: the audio-rate side of the coach.
//!
//! Lives entirely behind the control plane — the control plane spawns
//! a [`DataPlane`] in `do_start_session` and tears it down in
//! `do_stop_session`. Heads never touch it directly; they read the
//! latest pitch reading via [`AppCoach::latest_pitch`], which under
//! the hood is just `pitch_publisher.load()`.
//!
//! # Wiring
//!
//! ```text
//! cpal callback (RT)                worker thread
//!   │                                    │
//!   │ samples ─► rtrb::Producer ─► rtrb::Consumer
//!   ↓                                    │
//!                                        │ engine.in_port(audio_in).copy_from(...)
//!                                        │ engine.process_block(BLOCK_FRAMES)
//!                                        │ f0 = engine.out_port(f0_hz)[0]
//!                                        │ if f0 > 0: publisher.store(Some(reading))
//!                                        ▼
//!                                   head: coach.latest_pitch()
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
use domain_ports::app_coach::PitchReading;
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

impl DataPlane {
    /// Spawn the worker thread and return a `(DataPlane, producer)`
    /// pair. The producer is moved into the capture callback so the
    /// RT thread can push samples; the [`DataPlane`] handle is held
    /// by the control plane so it can [`stop`](Self::stop) the worker
    /// on session teardown.
    pub(crate) fn start(
        deps: DataPlaneDeps,
    ) -> Result<(Self, Producer<f32>, Arc<AtomicU64>), DataPlaneError> {
        let (producer, consumer) = RingBuffer::<f32>::new(RING_CAPACITY);

        // Build the engine on the *worker* thread, not here — engines
        // hold trait objects that aren't always `Send`, and the
        // worker is the only place process_block is ever called.
        // The `World` itself is just data and crosses the boundary.
        let quit = Arc::new(AtomicBool::new(false));
        let samples_dropped = Arc::new(AtomicU64::new(0));
        let pitch_publisher = Arc::clone(&deps.pitch_publisher);
        let clock = Arc::clone(&deps.clock);
        let telemetry = Arc::clone(&deps.telemetry);
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
                    pitch_publisher,
                    clock,
                    telemetry,
                });
            })
            .map_err(|e| DataPlaneError::Spawn(e.to_string()))?;

        Ok((
            Self {
                quit,
                worker: Some(worker),
                samples_dropped,
            },
            producer,
            dropped_for_callback,
        ))
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
    pub(crate) pitch_publisher: Arc<ArcSwap<Option<PitchReading>>>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) telemetry: Arc<dyn Telemetry>,
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
    pitch_publisher: Arc<ArcSwap<Option<PitchReading>>>,
    clock: Arc<dyn Clock>,
    telemetry: Arc<dyn Telemetry>,
}

fn run_worker(args: WorkerArgs) {
    let WorkerArgs {
        mut consumer,
        sample_rate,
        quit,
        pitch_publisher,
        clock,
        telemetry,
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

    let (audio_in, f0_out) = match resolve_ports(&engine) {
        Ok(pair) => pair,
        Err(e) => {
            tel_warn!(
                &*telemetry,
                "data-plane: boundary ports missing from coach.json; pitch will be unavailable",
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
        engine.in_port(audio_in).copy_from_slice(&block);
        engine.process_block(BLOCK_FRAMES);

        // Read f0; publish if voiced.
        let f0 = engine.out_port(f0_out)[0];
        let reading = PitchReading {
            f0_hz: f0,
            t_ms: clock.now_ms(),
        };
        // Always publish — heads can tell voiced from unvoiced via
        // the f0_hz == 0.0 sentinel. Publishing unvoiced too lets
        // heads detect "alive but silent" vs "stalled".
        pitch_publisher.store(Arc::new(Some(reading)));
    }

    tel_info!(&*telemetry, "data-plane: worker down");
}

fn resolve_ports(engine: &Engine) -> Result<(InPortHandle, OutPortHandle), String> {
    let in_h = engine
        .resolve_in_port("audio_in")
        .map_err(|e| format!("audio_in: {e:?}"))?;
    let out_h = engine
        .resolve_out_port("f0_hz")
        .map_err(|e| format!("f0_hz: {e:?}"))?;
    Ok((in_h, out_h))
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
