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
//!                                        │ publish latest + queued snapshot
//!                                        ▼
//!                            head: latest_features / drain_features
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
//! the RT thread). The ordered-history ring push is also allocation-free.
//! Publishing the latest value through `ArcSwap` allocates an `Arc` per
//! hop; that worker-thread cost is intentionally outside the RT callback.

use crate::audio_recorder::Recorder;
use crate::pitch_world::build_pitch_engine;
use crate::pitch_world::COACH_WORLD_JSON;
use arc_swap::ArcSwap;
use audio_trace_format::SidecarHop;
use domain_ports::app_coach::FeatureSnapshot;
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use domain_ports::{tel_info, tel_warn};
use engine::{Engine, InPortHandle, OutPortHandle};
use rtrb::{Consumer, Producer, RingBuffer};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Engine block size = PitchYin hop. One f0 estimate per block.
pub(crate) const BLOCK_FRAMES: usize = 512;

/// SPSC ring capacity in samples. ~85ms at 48kHz — comfortably larger
/// than any reasonable cpal buffer + worker scheduling jitter.
const RING_CAPACITY: usize = 4096;

/// Ordered feature snapshots retained for the head. At ~85 hops/s this
/// absorbs roughly three seconds without a drain.
pub(crate) const FEATURE_RING_CAPACITY: usize = 256;

// ---------------------------------------------------------------------
// Public: spawn / teardown
// ---------------------------------------------------------------------

pub(crate) struct DataPlane {
    quit: Arc<AtomicBool>,
    worker: Option<JoinHandle<WorkerExit>>,
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
    pub(crate) fn start(
        deps: DataPlaneDeps,
        feature_producer: &mut Option<Producer<FeatureSnapshot>>,
    ) -> Result<DataPlaneStartup, DataPlaneError> {
        if feature_producer.is_none() {
            return Err(DataPlaneError::FeatureProducerUnavailable);
        }
        let (producer, consumer) = RingBuffer::<f32>::new(RING_CAPACITY);
        let (feature_tx, feature_rx) = std::sync::mpsc::sync_channel(0);

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

        let session_prefix = deps.session_prefix;
        let worker = thread::Builder::new()
            .name("app-coach-data".into())
            .spawn(move || {
                let feature_producer = feature_rx
                    .recv()
                    .expect("feature producer handoff sender dropped");
                preserve_feature_producer_on_unwind(feature_producer, |feature_producer| {
                    run_worker(
                        WorkerArgs {
                            consumer,
                            sample_rate,
                            quit: quit_for_thread,
                            feature_publisher,
                            clock,
                            telemetry,
                            inspect,
                            session_prefix,
                        },
                        feature_producer,
                    );
                })
            })
            .map_err(|e| DataPlaneError::Spawn(e.to_string()))?;

        let producer_for_worker = feature_producer
            .take()
            .expect("feature producer must be available before data-plane start");
        if let Err(e) = feature_tx.send(producer_for_worker) {
            let _ = worker.join();
            *feature_producer = Some(e.0);
            return Err(DataPlaneError::Spawn(
                "data-plane worker exited before feature producer handoff".into(),
            ));
        }

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
    pub(crate) fn stop(mut self, telemetry: &dyn Telemetry) -> Option<Producer<FeatureSnapshot>> {
        self.quit.store(true, Ordering::Release);
        let feature_producer = if let Some(h) = self.worker.take() {
            // Worker checks `quit` every iteration of its drain loop
            // (≤BLOCK_FRAMES samples / a few ms), so the join here is
            // bounded even without a deadline.
            match h.join() {
                Ok(exit) => {
                    if exit.panicked {
                        tel_warn!(telemetry, "data-plane: worker panicked");
                    }
                    Some(exit.feature_producer)
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let dropped = self.samples_dropped.load(Ordering::Acquire);
        if dropped > 0 {
            tel_warn!(
                telemetry,
                "data-plane: RT samples dropped (worker fell behind)",
                samples = dropped,
            );
        }
        feature_producer
    }
}

pub(crate) struct DataPlaneDeps {
    pub(crate) sample_rate: u32,
    pub(crate) feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) telemetry: Arc<dyn Telemetry>,
    pub(crate) inspect: Arc<crate::inspect::InspectShared>,
    /// Path prefix for this session's audio trace, or `None` to skip recording.
    pub(crate) session_prefix: Option<std::path::PathBuf>,
}

#[derive(Debug)]
pub(crate) enum DataPlaneError {
    /// Worker thread failed to spawn (OS resource exhaustion).
    Spawn(String),
    /// A prior worker failed before returning the persistent feature
    /// producer, so ordered history cannot be published safely.
    FeatureProducerUnavailable,
}

impl std::fmt::Display for DataPlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(s) => write!(f, "data plane worker spawn failed: {s}"),
            Self::FeatureProducerUnavailable => {
                write!(f, "feature history producer unavailable")
            }
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
    session_prefix: Option<std::path::PathBuf>,
}

struct WorkerExit {
    feature_producer: Producer<FeatureSnapshot>,
    panicked: bool,
}

fn preserve_feature_producer_on_unwind<F>(
    mut feature_producer: Producer<FeatureSnapshot>,
    worker: F,
) -> WorkerExit
where
    F: FnOnce(&mut Producer<FeatureSnapshot>),
{
    let panicked = catch_unwind(AssertUnwindSafe(|| worker(&mut feature_producer))).is_err();
    WorkerExit {
        feature_producer,
        panicked,
    }
}

fn run_worker(args: WorkerArgs, feature_producer: &mut Producer<FeatureSnapshot>) {
    let WorkerArgs {
        mut consumer,
        sample_rate,
        quit,
        feature_publisher,
        clock,
        telemetry,
        inspect,
        session_prefix,
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

    // Compute vibrato latency once at boot (boot-time only, not hot-path).
    // vibrato_rate and vibrato_amplitude are produced by the same node, so their
    // latencies must be equal — assert the invariant and use either.
    let lat_frames = engine.out_port_latency(ports.vibrato_rate);
    debug_assert_eq!(
        lat_frames,
        engine.out_port_latency(ports.vibrato_amplitude),
        "vibrato_rate and vibrato_amplitude must come from the same node"
    );
    // Convert frames → ms with rounding (not truncation) in f64 to avoid
    // systematic bias at non-divisor sample rates.
    let vibrato_latency_ms: u64 = (lat_frames as f64 * 1000.0 / sample_rate as f64).round() as u64;

    tel_info!(
        &*telemetry,
        "data-plane: worker up",
        sample_rate = sample_rate,
        block_frames = BLOCK_FRAMES as u32,
    );

    let mut recorder = session_prefix
        .and_then(|prefix| Recorder::new(prefix, sample_rate, COACH_WORLD_JSON, "coach.json"));

    // Scratch buffer for one block worth of samples drained from the
    // ring. Heap-allocated once, before the hot loop.
    let mut block: Vec<f32> = vec![0.0; BLOCK_FRAMES];

    // Monotonic block index. Heads use it to detect missed taps.
    let mut block_seq: u64 = 0;
    // Session-local sequence for the published feature stream.
    let mut hop_index: u64 = 0;

    'worker: while !quit.load(Ordering::Acquire) {
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
                    // Producer dropped (cpal stream ended). Break out so
                    // the single exit point after the loop can finish().
                    break 'worker;
                }
            }
        }

        // Tap the filled block for the audio trace before feeding the engine.
        if let Some(r) = recorder.as_mut() {
            r.record_block(&block);
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
        let f0_hz = engine.out_port(ports.pitch)[0];
        let confidence = engine.out_port(ports.confidence)[0];
        let onset = engine.out_port(ports.onset)[0];
        let breath = engine.out_port(ports.breath)[0];
        let vibrato_rate = engine.out_port(ports.vibrato_rate)[0];
        let vibrato_amplitude = engine.out_port(ports.vibrato_amplitude)[0];
        let vibrato_phase = engine.out_port(ports.vibrato_phase)[0];

        let t_ms = clock.now_ms();
        let snapshot = FeatureSnapshot {
            hop_index,
            f0_hz,
            confidence,
            onset,
            breath,
            vibrato_rate,
            vibrato_amplitude,
            vibrato_phase,
            vibrato_t_ms: t_ms.saturating_sub(vibrato_latency_ms),
            t_ms,
        };
        if let Some(r) = recorder.as_mut() {
            r.record_hop(SidecarHop {
                hop: hop_index,
                f0_hz,
                confidence,
                onset,
                breath,
                vibrato_rate,
                vibrato_amplitude,
                vibrato_phase,
            });
        }
        hop_index = hop_index.wrapping_add(1);
        feature_publisher.store(Arc::new(Some(snapshot)));
        push_feature(feature_producer, snapshot);
    }

    if let Some(r) = recorder.take() {
        r.finish(&*telemetry);
    }

    tel_info!(&*telemetry, "data-plane: worker down");
}

/// Retain one ordered feature hop for the head. If the bounded queue is
/// full, drop the new history sample; the latest-value publisher remains
/// current. Realtime-safe: no allocation, lock, blocking, or syscall.
fn push_feature(producer: &mut Producer<FeatureSnapshot>, snapshot: FeatureSnapshot) {
    let _ = producer.push(snapshot);
}

struct ResolvedPorts {
    mic: InPortHandle,
    pitch: OutPortHandle,
    confidence: OutPortHandle,
    onset: OutPortHandle,
    breath: OutPortHandle,
    vibrato_rate: OutPortHandle,
    vibrato_amplitude: OutPortHandle,
    vibrato_phase: OutPortHandle,
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
    let vibrato_amplitude = engine
        .resolve_out_port("vibrato_amplitude")
        .map_err(|e| format!("vibrato_amplitude: {e:?}"))?;
    let vibrato_phase = engine
        .resolve_out_port("vibrato_phase")
        .map_err(|e| format!("vibrato_phase: {e:?}"))?;
    Ok(ResolvedPorts {
        mic,
        pitch,
        confidence,
        onset,
        breath,
        vibrato_rate,
        vibrato_amplitude,
        vibrato_phase,
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

    fn snapshot(hop_index: u64, t_ms: u64) -> FeatureSnapshot {
        FeatureSnapshot {
            hop_index,
            f0_hz: 440.0,
            confidence: 1.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_amplitude: 0.0,
            vibrato_phase: 0.0,
            vibrato_t_ms: t_ms,
            t_ms,
        }
    }

    /// Verify the frames→ms rounding formula used by the worker.
    ///
    /// Transitive latency for the vibrato out-ports:
    ///   PitchYin own:  window/2 + hop/2 = 2048/2 + 512/2  =   1280 frames
    ///   Vibrato own:   window/2 + hop/2 = 72000/2 + 600/2  =  36300 frames
    ///   Total (max-upstream + own):                           37580 frames
    ///
    /// 37580 / 48000 * 1000 = 782.916... ms → rounds to 783.
    #[test]
    fn vibrato_latency_frames_to_ms_rounds_correctly() {
        let sample_rate: u32 = 48000;

        // Transitive total: PitchYin(1280) + Vibrato(36300) = 37580 frames.
        let lat_frames: usize = 37580;
        let lat_ms = (lat_frames as f64 * 1000.0 / sample_rate as f64).round() as u64;
        assert_eq!(
            lat_ms, 783,
            "37580 frames at 48kHz = 782.916ms → rounds to 783"
        );

        // Non-divisor just below the 0.5 threshold: 37580 + 1 = 37581 → 782.937ms → 783.
        let lat_frames_odd: usize = 37581;
        let lat_ms_odd = (lat_frames_odd as f64 * 1000.0 / sample_rate as f64).round() as u64;
        assert_eq!(lat_ms_odd, 783, "37581 frames → ~782.937ms → rounds to 783");

        // Non-divisor past the 0.5 threshold: 37604 frames → 783.416ms → rounds to 783.
        let lat_frames_up: usize = 37604;
        let lat_ms_up = (lat_frames_up as f64 * 1000.0 / sample_rate as f64).round() as u64;
        assert_eq!(lat_ms_up, 783, "37604 frames → 783.416ms → rounds to 783");
    }

    #[test]
    fn feature_queue_round_trips_in_order_without_timestamp_dedup() {
        let (mut producer, mut consumer) = RingBuffer::new(FEATURE_RING_CAPACITY);
        for hop_index in 0..8 {
            push_feature(&mut producer, snapshot(hop_index, 7));
        }

        let drained: Vec<_> = std::iter::from_fn(|| consumer.pop().ok()).collect();
        assert_eq!(drained.len(), 8);
        assert!(drained.iter().all(|s| s.t_ms == 7));
        assert_eq!(
            drained.iter().map(|s| s.hop_index).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn feature_queue_drops_newest_while_latest_keeps_advancing() {
        let (mut producer, mut consumer) = RingBuffer::new(FEATURE_RING_CAPACITY);
        let latest = Arc::new(ArcSwap::from_pointee(None));
        let produced = FEATURE_RING_CAPACITY as u64 + 4;

        for hop_index in 0..produced {
            let value = snapshot(hop_index, hop_index);
            latest.store(Arc::new(Some(value)));
            push_feature(&mut producer, value);
        }

        let drained: Vec<_> = std::iter::from_fn(|| consumer.pop().ok()).collect();
        assert_eq!(drained.len(), FEATURE_RING_CAPACITY);
        assert_eq!(drained.first().unwrap().hop_index, 0);
        assert_eq!(
            drained.last().unwrap().hop_index,
            FEATURE_RING_CAPACITY as u64 - 1
        );
        assert_eq!(
            latest.load().as_ref().unwrap().hop_index,
            produced - 1,
            "latest snapshot must stay fresh while history is full"
        );

        let after_gap = snapshot(produced, produced);
        push_feature(&mut producer, after_gap);
        assert_eq!(
            consumer.pop().unwrap().hop_index,
            produced,
            "the next accepted sample exposes the exact retained-stream gap"
        );
    }

    #[test]
    fn feature_producer_survives_worker_panic() {
        let (producer, mut consumer) = RingBuffer::new(FEATURE_RING_CAPACITY);
        let exit = preserve_feature_producer_on_unwind(producer, |_producer| {
            panic!("forced worker failure");
        });

        assert!(exit.panicked);
        let mut producer = exit.feature_producer;
        push_feature(&mut producer, snapshot(7, 11));
        assert_eq!(consumer.pop().unwrap(), snapshot(7, 11));
    }

    #[test]
    fn feature_producer_is_reused_and_queued_sessions_expose_reset() {
        const SR: u32 = 48_000;

        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(11));
        let telemetry: Arc<dyn Telemetry> = Arc::new(TestTelemetry::new(Arc::clone(&clock)));
        let feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>> =
            Arc::new(ArcSwap::from_pointee(None));
        let (feature_producer, mut feature_consumer) =
            RingBuffer::<FeatureSnapshot>::new(FEATURE_RING_CAPACITY);
        let mut feature_producer = Some(feature_producer);

        for _session in 0..2 {
            feature_publisher.store(Arc::new(None));
            let DataPlaneStartup {
                data_plane,
                mut producer,
                ..
            } = DataPlane::start(
                DataPlaneDeps {
                    sample_rate: SR,
                    feature_publisher: Arc::clone(&feature_publisher),
                    clock: Arc::clone(&clock),
                    telemetry: Arc::clone(&telemetry),
                    inspect: crate::inspect::InspectShared::new(),
                    session_prefix: None,
                },
                &mut feature_producer,
            )
            .expect("data plane starts");

            let deadline = Instant::now() + Duration::from_secs(1);
            for _ in 0..BLOCK_FRAMES {
                loop {
                    if producer.push(0.0).is_ok() {
                        break;
                    }
                    assert!(Instant::now() <= deadline, "audio ring stayed full");
                    thread::sleep(Duration::from_millis(1));
                }
            }

            let published = loop {
                if let Some(value) = **feature_publisher.load() {
                    break value;
                }
                assert!(
                    Instant::now() <= deadline,
                    "worker did not publish a feature snapshot"
                );
                thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(published.hop_index, 0, "each session starts at hop zero");

            drop(producer);
            feature_producer = data_plane.stop(&*telemetry);
            assert!(
                feature_producer.is_some(),
                "worker must return the persistent feature producer"
            );
        }

        let retained: Vec<_> = std::iter::from_fn(|| feature_consumer.pop().ok()).collect();
        assert_eq!(
            retained.iter().map(|s| s.hop_index).collect::<Vec<_>>(),
            vec![0, 0],
            "undrained prior-session history remains ordered before the reset"
        );
    }

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
        let (feature_producer, _feature_consumer) =
            RingBuffer::<FeatureSnapshot>::new(FEATURE_RING_CAPACITY);
        let mut feature_producer = Some(feature_producer);

        let DataPlaneStartup {
            data_plane,
            mut producer,
            samples_dropped: dropped,
        } = DataPlane::start(
            DataPlaneDeps {
                sample_rate: SR,
                feature_publisher: Arc::clone(&feature_publisher),
                clock,
                telemetry: Arc::clone(&telemetry),
                inspect: crate::inspect::InspectShared::new(),
                session_prefix: None,
            },
            &mut feature_producer,
        )
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
        assert!(
            data_plane.stop(&*telemetry).is_some(),
            "feature producer returns on stop"
        );
        assert_eq!(dropped.load(Ordering::Acquire), 0, "no drops expected");
    }

    /// End-to-end live-path check for the audio-trace recorder: drive a real
    /// `DataPlane` worker (real thread, real engine) with a session prefix, then
    /// confirm the WAV / sidecar / manifest land with a coherent shape. Unlike
    /// the `audio_recorder` unit tests (which exercise `Recorder` in isolation),
    /// this proves the worker wiring — the block tap, the six `out_port[0]`
    /// reads, and the single-`finish` exit — through genuine concurrency.
    #[test]
    fn worker_records_audio_trace_when_env_set() {
        use audio_trace_format::{Manifest, SidecarHop};
        use std::io::BufRead;

        const SR: u32 = 48_000;
        const F0: f32 = 440.0;

        let dir = tempfile::tempdir().expect("tempdir");
        let prefix = dir.path().join("test-engine-input");

        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        let telemetry: Arc<dyn Telemetry> = Arc::new(TestTelemetry::new(Arc::clone(&clock)));
        let feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>> =
            Arc::new(ArcSwap::from_pointee(None));
        let (feature_producer, _feature_consumer) =
            RingBuffer::<FeatureSnapshot>::new(FEATURE_RING_CAPACITY);
        let mut feature_producer = Some(feature_producer);

        let DataPlaneStartup {
            data_plane,
            mut producer,
            samples_dropped: _dropped,
        } = DataPlane::start(
            DataPlaneDeps {
                sample_rate: SR,
                feature_publisher: Arc::clone(&feature_publisher),
                clock,
                telemetry: Arc::clone(&telemetry),
                inspect: crate::inspect::InspectShared::new(),
                session_prefix: Some(prefix),
            },
            &mut feature_producer,
        )
        .expect("data plane starts");

        // Feed a whole number of 512-blocks so the recorded WAV length is
        // predictable. 8192 = 16 blocks.
        let mut phase: f32 = 0.0;
        let step = 2.0 * std::f32::consts::PI * F0 / SR as f32;
        let mut chunk = [0.0_f32; 256];
        let total_samples = 8192;
        let mut pushed = 0;
        while pushed < total_samples {
            for s in chunk.iter_mut() {
                *s = phase.sin() * 0.5;
                phase += step;
            }
            // Generous deadline: under `cargo test --workspace` the box can be
            // saturated by parallel release builds, so the worker drains
            // slowly. This test asserts *what* lands on disk, not *how fast*.
            let deadline = Instant::now() + Duration::from_secs(5);
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

        // Wait until the worker has published at least one hop, so we know
        // blocks were consumed before we tear down. Generous deadline for the
        // same contended-CI reason as above.
        let deadline = Instant::now() + Duration::from_secs(5);
        while feature_publisher.load().is_none() {
            assert!(Instant::now() <= deadline, "worker never published a hop");
            thread::sleep(Duration::from_millis(5));
        }

        drop(producer);
        data_plane.stop(&*telemetry);

        // Use the path helpers to find artifacts by the known prefix.
        let prefix = dir.path().join("test-engine-input");
        let manifest_path = audio_trace_format::manifest_path(&prefix);
        assert!(
            manifest_path.exists(),
            "a manifest must have been written (valid run)"
        );

        let manifest: Manifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read manifest"))
                .expect("manifest parses as the shared type");
        assert_eq!(manifest.schema, 2);
        assert_eq!(manifest.block_size, 512);
        assert_eq!(manifest.channels, 1);
        assert_eq!(manifest.sample_rate, SR);
        assert_eq!(manifest.world, "coach.json");
        assert_eq!(manifest.world_sha256.len(), 64);
        assert!(manifest.n_hops > 0, "at least one block must be recorded");
        assert_eq!(manifest.total_samples, manifest.n_hops * 512);

        // WAV length matches n_hops * block_size exactly.
        let wav_path = audio_trace_format::wav_path(&prefix);
        let mut reader = hound::WavReader::open(&wav_path).expect("open WAV");
        let n_samples = reader.samples::<f32>().count();
        assert_eq!(
            n_samples,
            manifest.n_hops * 512,
            "WAV sample count must equal n_hops * block_size"
        );

        // Sidecar has n_hops lines, each a parseable SidecarHop with contiguous
        // hop indices from 0.
        let sidecar_path = audio_trace_format::features_path(&prefix);
        let sidecar = std::fs::File::open(&sidecar_path).expect("open sidecar");
        let hops: Vec<SidecarHop> = std::io::BufReader::new(sidecar)
            .lines()
            .map(|l| l.expect("line"))
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(&l).expect("hop parses"))
            .collect();
        assert_eq!(hops.len(), manifest.n_hops, "one sidecar line per hop");
        for (i, hop) in hops.iter().enumerate() {
            assert_eq!(hop.hop, i as u64, "hop indices are contiguous from 0");
        }
    }
}
