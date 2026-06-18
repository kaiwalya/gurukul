//! adapter-app-coach: the canonical [`AppCoach`] implementation.
//!
//! # Architecture
//!
//! Two planes, split by who owns the state:
//!
//! - **Control plane**: a single owned thread that drains an MPSC of
//!   [`Input`](control_plane::Input) values. Every session-state
//!   mutation happens here — no `Mutex` around the state, no race, no
//!   ordering question. The [`Input`](control_plane::Input) enum
//!   unifies head commands (delivered via [`AppCoach::send_command`])
//!   and (in Phase 2) internal acks from the audio callback.
//! - **Data plane**: an SPSC ring fed by the cpal RT callback,
//!   drained by a worker thread that runs the dsp engine (PitchYin +
//!   Onset + Breath + Vibrato from [`pitch_world`]) and publishes the
//!   latest per-hop feature snapshot into an
//!   `ArcSwap<Option<FeatureSnapshot>>` and appends retained hops to a
//!   bounded SPSC ring. Heads use [`AppCoach::latest_features`] for the
//!   current instant and [`AppCoach::drain_features`] for ordered history.
//!
//! # Outbound events
//!
//! Events the head consumes via [`AppCoach::poll_events`] live in a
//! bounded [`OutboundQueue`](outbound::OutboundQueue) behind a `Mutex`.
//! The control plane pushes; the head drains. On overflow the queue
//! drops the *oldest* event(s) and coalesces a single
//! [`CoachEvent::EventsDropped`] when space frees up.
//!
//! # Shutdown
//!
//! `shutdown(timeout)` sends [`Input::Quit`](control_plane::Input::Quit),
//! waits up to `timeout` for the control-plane thread to join. On
//! timeout the thread is detached and the result is
//! [`ShutdownResult::TimedOut`]. `Drop` calls `shutdown(Duration::ZERO)`
//! as belt-and-suspenders.
//!
//! # Source layout
//!
//! - [`control_plane`] — the thread, the [`Input`](control_plane::Input)
//!   enum, and the session state machine.
//! - [`data_plane`] — the worker thread, audio/feature SPSC rings, and
//!   ArcSwap publisher.
//! - [`pitch_world`] — builds the dsp engine from the embedded
//!   `coach.json` world.
//! - [`outbound`] — the bounded outbound queue.
//! - [`shutdown`] — `join_with_timeout` helper.
//! - [`helpers`] — capture-error classification, sample-rate picker.
//! - This file — `pub fn new`, `CoachImpl`, the [`AppCoach`] impl, the
//!   `Drop` glue, and the `#[cfg(test)]` test module covering the
//!   end-to-end boundary.

mod audio_recorder;
mod control_plane;
mod data_plane;
mod helpers;
mod inspect;
mod outbound;
mod pitch_world;
mod shutdown;

use control_plane::{ControlPlane, FeaturePublishers, Input};
use inspect::{EngineInspectImpl, InspectShared};
use outbound::OutboundQueue;
use shutdown::join_with_timeout;

use arc_swap::ArcSwap;
use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, AudioInfo, CoachEvent, Command, FeatureSnapshot, MusicInfo,
    ShutdownResult,
};
use domain_ports::engine_inspect::EngineInspect;
use rtrb::{Consumer, RingBuffer};
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Outbound event queue capacity. Heads draining at ≥10Hz won't see
/// `EventsDropped` in normal use; one second of completely-stalled
/// head + a sustained 1kHz event rate would be needed to overflow.
const OUTBOUND_QUEUE_CAP: usize = 1024;

/// Build the canonical [`AppCoach`]. Eagerly spawns the control-plane
/// thread; returns once it's ready to receive [`Input`]s.
pub fn new(deps: AppCoachDeps) -> impl AppCoach {
    let (coach, _inspect) = build(deps);
    coach
}

/// Build the canonical [`AppCoach`] *and* an [`EngineInspect`] handle
/// for hosts that want a debug pane. The inspect handle is a sibling
/// resource — it shares the same engine thread and gets cleared when
/// the session tears down. Production hosts call [`new`] instead and
/// pay nothing for the unused publishers.
pub fn new_with_inspect(deps: AppCoachDeps) -> (impl AppCoach, Arc<dyn EngineInspect>) {
    let (coach, inspect) = build(deps);
    (coach, inspect)
}

fn build(deps: AppCoachDeps) -> (CoachImpl, Arc<dyn EngineInspect>) {
    let outbound = Arc::new(Mutex::new(OutboundQueue::new(OUTBOUND_QUEUE_CAP)));
    let (tx_cmd, rx_cmd) = mpsc::channel::<Input>();
    let outbound_for_thread = Arc::clone(&outbound);
    let feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>> =
        Arc::new(ArcSwap::from_pointee(None));
    let feature_publisher_for_thread = Arc::clone(&feature_publisher);
    let (feature_producer, feature_consumer) =
        RingBuffer::<FeatureSnapshot>::new(data_plane::FEATURE_RING_CAPACITY);
    let audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>> =
        Arc::new(ArcSwap::from_pointee(None));
    let audio_info_publisher_for_thread = Arc::clone(&audio_info_publisher);
    let music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>> =
        Arc::new(ArcSwap::from_pointee(None));
    let music_info_publisher_for_thread = Arc::clone(&music_info_publisher);
    let inspect_shared = InspectShared::new();
    let inspect_for_thread = Arc::clone(&inspect_shared);

    let tx_cmd_for_thread = tx_cmd.clone();
    let control_thread = thread::Builder::new()
        .name("app-coach-control".into())
        .spawn(move || {
            ControlPlane::new(
                deps,
                outbound_for_thread,
                tx_cmd_for_thread,
                rx_cmd,
                FeaturePublishers {
                    latest: feature_publisher_for_thread,
                    history: feature_producer,
                },
                audio_info_publisher_for_thread,
                music_info_publisher_for_thread,
                inspect_for_thread,
            )
            .run();
        })
        .expect("spawn control-plane thread");

    let coach = CoachImpl {
        tx_cmd: Mutex::new(Some(tx_cmd)),
        outbound,
        feature_publisher,
        feature_consumer: RefCell::new(feature_consumer),
        audio_info_publisher,
        music_info_publisher,
        control_thread: Mutex::new(Some(control_thread)),
        shut_down: Mutex::new(false),
        thread_affinity: PhantomData,
    };
    let inspect: Arc<dyn EngineInspect> = Arc::new(EngineInspectImpl {
        shared: inspect_shared,
    });
    (coach, inspect)
}

// ---------------------------------------------------------------------
// Public impl
// ---------------------------------------------------------------------

struct CoachImpl {
    /// Send channel to the control plane. `Option` so [`shutdown`]
    /// can drop it (closing the channel) after sending `Quit`, which
    /// makes any late `send_command` a no-op (the receiver is gone).
    tx_cmd: Mutex<Option<Sender<Input>>>,
    outbound: Arc<Mutex<OutboundQueue>>,
    /// Lock-free snapshot of the latest per-hop feature values. The
    /// data plane publishes; heads read via [`latest_features`]. `None`
    /// before the first snapshot lands (no session running, first
    /// window still filling, etc.).
    feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    /// Ordered retained feature hops. `AppCoach` is thread-affine, so
    /// single-thread interior mutability is sufficient for the consumer's
    /// `&mut self` pop API without adding a lock.
    feature_consumer: RefCell<Consumer<FeatureSnapshot>>,
    /// Lock-free snapshot of the negotiated session parameters. Written
    /// by the control plane before emitting `AudioSessionStateChanged(Running)`
    /// and cleared before the next transition out — see [`AudioInfo`]
    /// for the ordering contract.
    audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>>,
    /// Lock-free sticky snapshot of the musical frame of reference.
    /// Written by the control plane in the configure handler (before
    /// the `MusicSessionConfigured` event); survives start/stop. See
    /// [`MusicInfo`].
    music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>>,
    control_thread: Mutex<Option<JoinHandle<()>>>,
    shut_down: Mutex<bool>,
    /// Make the concrete opaque return type honor the port's `!Send`
    /// thread-affinity contract on every platform.
    thread_affinity: PhantomData<Rc<()>>,
}

impl AppCoach for CoachImpl {
    fn send_command(&self, cmd: Command) {
        let tx = self.tx_cmd.lock().unwrap();
        if let Some(tx) = tx.as_ref() {
            // Channel closed = control plane already exited; silently
            // drop. send_command makes no promise of delivery.
            let _ = tx.send(Input::FromHead(cmd));
        }
    }

    fn poll_events(&self, out: &mut Vec<CoachEvent>) {
        let mut q = self.outbound.lock().unwrap();
        q.drain_into(out);
    }

    fn shutdown(&self, timeout: Duration) -> ShutdownResult {
        let mut guard = self.shut_down.lock().unwrap();
        if *guard {
            return ShutdownResult::AlreadyShutDown;
        }
        *guard = true;
        drop(guard);

        // Send Quit and then drop the sender so the control plane's
        // recv loop sees a clean shutdown signal even if Quit somehow
        // races a drop.
        if let Some(tx) = self.tx_cmd.lock().unwrap().take() {
            let _ = tx.send(Input::Quit);
        }

        let handle = self.control_thread.lock().unwrap().take();
        match handle {
            Some(h) => join_with_timeout(h, timeout),
            None => ShutdownResult::Clean,
        }
    }

    fn latest_features(&self) -> Option<FeatureSnapshot> {
        **self.feature_publisher.load()
    }

    fn drain_features(&self, out: &mut Vec<FeatureSnapshot>) {
        let mut consumer = self.feature_consumer.borrow_mut();
        drain_feature_consumer(&mut consumer, out);
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        (**self.audio_info_publisher.load()).clone()
    }

    fn music_info(&self) -> Option<MusicInfo> {
        **self.music_info_publisher.load()
    }
}

fn drain_feature_consumer(
    consumer: &mut Consumer<FeatureSnapshot>,
    out: &mut Vec<FeatureSnapshot>,
) {
    while let Ok(snapshot) = consumer.pop() {
        out.push(snapshot);
    }
}

impl Drop for CoachImpl {
    fn drop(&mut self) {
        // Best-effort: only act if shutdown wasn't called.
        let already = *self.shut_down.lock().unwrap();
        if already {
            return;
        }
        // Force shutdown with zero timeout — head forgot. Resources
        // may leak; the control plane logs the leak via telemetry
        // when its TimedOut path fires.
        let _ = self.shutdown(Duration::ZERO);
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::app_coach::{AudioConfig, AudioSessionErrorKind, AudioSessionState};
    use domain_ports::audio_capture::{
        AudioCapture, CaptureCallback, CaptureConfig, CaptureError, CaptureSession, LifecycleEvent,
        LifecycleSink,
    };
    use domain_ports::audio_devices::{
        AudioDevices, DeviceId, InputDevice, InputStream, SampleRateSupport, StreamHandle,
        Transport,
    };
    use domain_ports::audio_driver::{
        AudioDriver, AudioInitError, AudioInitStatus, AudioPermissionSink,
    };
    use domain_ports::clock::{Clock, TestClock};
    use domain_ports::pitch::PitchLog2;
    use domain_ports::scale::{Scale, ScaleIntervals};
    use domain_ports::telemetry::TestTelemetry;
    use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};
    use std::sync::atomic::{AtomicU32, Ordering};

    // ---- fakes ----

    struct FakeDevices {
        devices: Vec<InputDevice>,
        default: Option<InputStream>,
    }

    impl AudioDevices for FakeDevices {
        fn list_devices(&self) -> Vec<InputDevice> {
            // Hand back a deep-cloned list. InputDevice isn't Clone
            // by default; rebuild via a helper.
            self.devices.iter().map(clone_input_device).collect()
        }
        fn default_input(&self) -> Option<InputStream> {
            self.default.as_ref().map(clone_input_stream)
        }
    }

    fn clone_input_device(d: &InputDevice) -> InputDevice {
        InputDevice {
            persistent_id: d.persistent_id.clone(),
            name: d.name.clone(),
            transport: d.transport,
            streams: d.streams.iter().map(clone_input_stream).collect(),
        }
    }

    fn clone_input_stream(s: &InputStream) -> InputStream {
        InputStream {
            handle: s.handle.clone(),
            name: s.name.clone(),
            channels: s.channels,
            sample_rates: clone_rates(&s.sample_rates),
        }
    }

    fn clone_rates(r: &SampleRateSupport) -> SampleRateSupport {
        match r {
            SampleRateSupport::List(v) => SampleRateSupport::List(v.clone()),
            SampleRateSupport::Ranges(v) => SampleRateSupport::Ranges(v.clone()),
            SampleRateSupport::ProbeOnly => SampleRateSupport::ProbeOnly,
        }
    }

    /// An audio driver that always reports `Granted` and returns the
    /// supplied `FakeDevices` from `new_devices()`. Used by the existing
    /// tests that don't care about the permission flow.
    struct GrantedAudioDriver {
        devices: FakeDevices,
    }

    impl AudioDriver for GrantedAudioDriver {
        fn init_status(&self) -> AudioInitStatus {
            AudioInitStatus::Granted
        }
        fn request(&self, sink: AudioPermissionSink) {
            sink.signal();
        }
        fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError> {
            // Clone the inner FakeDevices for each call (devices are cloned on list).
            Ok(Box::new(FakeDevices {
                devices: self.devices.list_devices(),
                default: self.devices.default_input(),
            }))
        }
    }

    struct FakeCapture {
        opens: Arc<AtomicU32>,
        outcome: FakeOutcome,
        /// What `negotiate()` returns. `PassThrough` means return the requested
        /// config verbatim (normal path); `Return(cfg)` returns that config
        /// (simulating a device that negotiates a different rate); `Fail` makes
        /// negotiate fail.
        negotiate_result: NegotiateResult,
        /// Config passed to each `open()` call, in order.
        opened_configs: Arc<Mutex<Vec<CaptureConfig>>>,
        /// The lifecycle sink the control plane handed to the *most recent*
        /// successful `open()`. The 1.6.1c event-injection hook: a test pulls
        /// this and fires a `LifecycleEvent` through it (simulating an OS
        /// interruption / route change / backend error). `None` until the
        /// first successful open. A shared handle is held by `LifecycleFirer`.
        last_sink: Arc<Mutex<Option<LifecycleSink>>>,
    }

    /// Test handle that fires lifecycle events through the sink stored by the
    /// most recent successful `FakeCapture::open`. Cloneable + `Send` so a
    /// test can keep it while the coach owns the capture port.
    #[derive(Clone)]
    struct LifecycleFirer {
        sink: Arc<Mutex<Option<LifecycleSink>>>,
    }

    impl LifecycleFirer {
        /// Fire one lifecycle event through the *current* stored sink. Panics
        /// if no session has opened yet (a test bug — fire only after Running).
        fn fire(&self, event: LifecycleEvent) {
            let guard = self.sink.lock().unwrap();
            let sink = guard
                .as_ref()
                .expect("no lifecycle sink stored — fire only after a session reached Running");
            sink(event);
        }

        /// Take the *current* stored sink out, so a test can hold it across a
        /// later reopen (recovery) and fire it as a now-stale old-stream sink.
        /// `LifecycleSink` is `Box<dyn Fn>` (not cloneable), so taking is the
        /// only way to retain a specific generation's sink; the next
        /// successful `open()` installs a fresh sink in the vacated slot.
        fn take_sink(&self) -> LifecycleSink {
            self.sink
                .lock()
                .unwrap()
                .take()
                .expect("no lifecycle sink stored — take only after a session reached Running")
        }
    }

    #[derive(Clone)]
    enum FakeOutcome {
        Ok,
        FailUnsupported,
        FailOnceThenOk,
        /// The first open succeeds (session reaches Running); every reopen
        /// after that fails. Drives the recovery retry-budget test: a
        /// reconcile keeps failing until the budget trips to Error.
        OkThenFail,
    }

    /// Injection point for `negotiate()` in tests.
    #[derive(Clone)]
    enum NegotiateResult {
        /// Return the requested config verbatim (normal happy path).
        PassThrough,
        /// Return this specific config (simulates a device that negotiates
        /// a different rate from what was requested).
        Return(CaptureConfig),
        /// Return this error (simulates a device that rejects the config).
        Fail(u32, u16), // sample_rate and channels to use in the error
    }

    impl AudioCapture for FakeCapture {
        fn negotiate(
            &self,
            _handle: &StreamHandle,
            requested: &CaptureConfig,
        ) -> Result<CaptureConfig, CaptureError> {
            match &self.negotiate_result {
                NegotiateResult::PassThrough => Ok(CaptureConfig {
                    sample_rate: requested.sample_rate,
                    channels: requested.channels,
                    buffer_frames: requested.buffer_frames,
                }),
                NegotiateResult::Return(cfg) => Ok(CaptureConfig {
                    sample_rate: cfg.sample_rate,
                    channels: cfg.channels,
                    buffer_frames: cfg.buffer_frames,
                }),
                NegotiateResult::Fail(wanted_rate, wanted_channels) => {
                    Err(CaptureError::UnsupportedConfig {
                        wanted: CaptureConfig {
                            sample_rate: *wanted_rate,
                            channels: *wanted_channels,
                            buffer_frames: None,
                        },
                        actual: None,
                    })
                }
            }
        }

        fn open(
            &self,
            _handle: StreamHandle,
            cfg: CaptureConfig,
            _on_frame: CaptureCallback,
            on_event: LifecycleSink,
        ) -> Result<CaptureSession, CaptureError> {
            self.opened_configs.lock().unwrap().push(CaptureConfig {
                sample_rate: cfg.sample_rate,
                channels: cfg.channels,
                buffer_frames: cfg.buffer_frames,
            });
            let open_index = self.opens.fetch_add(1, Ordering::SeqCst);
            match (&self.outcome, open_index) {
                (FakeOutcome::Ok, _)
                | (FakeOutcome::FailOnceThenOk, 1..)
                | (FakeOutcome::OkThenFail, 0) => {
                    // Store the sink so a test can fire lifecycle events at the
                    // now-Running session. Replaces any prior sink — a reopen
                    // (recovery) supersedes the dead session's sink.
                    *self.last_sink.lock().unwrap() = Some(on_event);
                    Ok(CaptureSession::new(|| {}))
                }
                (
                    FakeOutcome::FailUnsupported
                    | FakeOutcome::FailOnceThenOk
                    | FakeOutcome::OkThenFail,
                    _,
                ) => {
                    // Failed open: the sink dies here (the control plane drops
                    // it on its side too), so do not store it.
                    drop(on_event);
                    Err(CaptureError::UnsupportedConfig {
                        wanted: cfg,
                        actual: None,
                    })
                }
            }
        }
    }

    // ---- deps builder ----

    fn deps_with(
        outcome: FakeOutcome,
        opens: Arc<AtomicU32>,
    ) -> (AppCoachDeps, Arc<TestTelemetry>) {
        let (deps, tel, _) = deps_with_negotiate(outcome, opens, NegotiateResult::PassThrough);
        (deps, tel)
    }

    fn deps_with_negotiate(
        outcome: FakeOutcome,
        opens: Arc<AtomicU32>,
        negotiate_result: NegotiateResult,
    ) -> (
        AppCoachDeps,
        Arc<TestTelemetry>,
        Arc<Mutex<Vec<CaptureConfig>>>,
    ) {
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        let telemetry = Arc::new(TestTelemetry::new(Arc::clone(&clock)));

        let stream = InputStream {
            handle: StreamHandle(Arc::new(())),
            name: "fake mic".into(),
            channels: 1,
            sample_rates: SampleRateSupport::Ranges(vec![(48_000, 48_000)]),
        };
        let device = InputDevice {
            persistent_id: Some(DeviceId("fake-id".into())),
            name: "fake mic".into(),
            transport: Transport::BuiltIn,
            streams: vec![clone_input_stream(&stream)],
        };
        let session_port: Arc<dyn domain_ports::audio_driver::AudioDriver> =
            Arc::new(GrantedAudioDriver {
                devices: FakeDevices {
                    devices: vec![device],
                    default: Some(stream),
                },
            });
        let opened_configs: Arc<Mutex<Vec<CaptureConfig>>> = Arc::new(Mutex::new(Vec::new()));
        let capture_port: Arc<dyn AudioCapture> = Arc::new(FakeCapture {
            opens,
            outcome,
            negotiate_result,
            opened_configs: Arc::clone(&opened_configs),
            last_sink: Arc::new(Mutex::new(None)),
        });
        (
            AppCoachDeps {
                clock,
                telemetry: telemetry.clone(),
                audio_driver: session_port,
                audio_capture: capture_port,
                host_version: "test",
            },
            telemetry,
            opened_configs,
        )
    }

    /// Deps for the lifecycle-seam tests. Returns the coach deps plus a
    /// [`LifecycleFirer`] (to inject events) and the [`TestClock`] (to drive
    /// the deadline-aware loop's timers deterministically).
    fn deps_for_lifecycle(
        outcome: FakeOutcome,
        opens: Arc<AtomicU32>,
    ) -> (AppCoachDeps, LifecycleFirer, Arc<TestClock>) {
        let clock = Arc::new(TestClock::new(0));
        let clock_dyn: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
        let telemetry = Arc::new(TestTelemetry::new(Arc::clone(&clock_dyn)));

        let stream = InputStream {
            handle: StreamHandle(Arc::new(())),
            name: "fake mic".into(),
            channels: 1,
            sample_rates: SampleRateSupport::Ranges(vec![(48_000, 48_000)]),
        };
        let device = InputDevice {
            persistent_id: Some(DeviceId("fake-id".into())),
            name: "fake mic".into(),
            transport: Transport::BuiltIn,
            streams: vec![clone_input_stream(&stream)],
        };
        let session_port: Arc<dyn domain_ports::audio_driver::AudioDriver> =
            Arc::new(GrantedAudioDriver {
                devices: FakeDevices {
                    devices: vec![device],
                    default: Some(stream),
                },
            });
        let last_sink: Arc<Mutex<Option<LifecycleSink>>> = Arc::new(Mutex::new(None));
        let capture_port: Arc<dyn AudioCapture> = Arc::new(FakeCapture {
            opens,
            outcome,
            negotiate_result: NegotiateResult::PassThrough,
            opened_configs: Arc::new(Mutex::new(Vec::new())),
            last_sink: Arc::clone(&last_sink),
        });
        let firer = LifecycleFirer { sink: last_sink };
        (
            AppCoachDeps {
                clock: clock_dyn,
                telemetry,
                audio_driver: session_port,
                audio_capture: capture_port,
                host_version: "test",
            },
            firer,
            clock,
        )
    }

    /// Start a session and block until it reaches `Running`.
    fn start_to_running(coach: &impl AppCoach) {
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
    }

    fn poll_until<F: Fn(&[CoachEvent]) -> bool>(coach: &impl AppCoach, pred: F) -> Vec<CoachEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut acc = Vec::new();
        let mut buf = Vec::new();
        while std::time::Instant::now() < deadline {
            coach.poll_events(&mut buf);
            acc.append(&mut buf);
            if pred(&acc) {
                return acc;
            }
            thread::sleep(Duration::from_millis(5));
        }
        acc
    }

    // ---- tests ----

    fn feature(hop_index: u64, t_ms: u64) -> FeatureSnapshot {
        FeatureSnapshot {
            hop_index,
            f0_hz: 440.0,
            confidence: 1.0,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_depth: 0.0,
            t_ms,
        }
    }

    #[test]
    fn drain_features_appends_every_pending_snapshot_in_order() {
        let (mut producer, mut consumer) = RingBuffer::new(4);
        producer.push(feature(0, 7)).unwrap();
        producer.push(feature(1, 7)).unwrap();
        let prefix = feature(99, 1);
        let mut out = vec![prefix];

        drain_feature_consumer(&mut consumer, &mut out);

        assert_eq!(out, vec![prefix, feature(0, 7), feature(1, 7)]);
        drain_feature_consumer(&mut consumer, &mut out);
        assert_eq!(
            out,
            vec![prefix, feature(0, 7), feature(1, 7)],
            "a second drain appends nothing when no snapshots are pending"
        );
    }

    #[test]
    fn list_devices_round_trip() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioListDevices);
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioDevicesListed { .. }))
        });

        let listed = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioDevicesListed { devices } => Some(devices),
                _ => None,
            })
            .expect("AudioDevicesListed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "fake mic");

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn list_scales_round_trip() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // MusicListScales requires a configured tuning to know which octave
        // division is active. Without MusicConfigureSession first it returns empty.
        coach.send_command(Command::MusicListScales);
        let events_before = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::MusicScalesListed { .. }))
        });
        let shapes_before = events_before
            .iter()
            .find_map(|e| match e {
                CoachEvent::MusicScalesListed { shapes } => Some(shapes),
                _ => None,
            })
            .expect("MusicScalesListed (before configure)");
        assert!(
            shapes_before.is_empty(),
            "before MusicConfigureSession, MusicScalesListed must be empty (no tuning known)"
        );

        // Configure a 12-TET session; now MusicListScales returns 15 shapes.
        coach.send_command(Command::MusicConfigureSession {
            scale: bilawal_scale(0),
        });
        coach.send_command(Command::MusicListScales);
        let events_after = poll_until(&coach, |evs| {
            // Wait for a second MusicScalesListed (the non-empty one).
            evs.iter()
                .filter(|e| matches!(e, CoachEvent::MusicScalesListed { .. }))
                .count()
                >= 2
        });
        let shapes_after = events_after
            .iter()
            .filter_map(|e| match e {
                CoachEvent::MusicScalesListed { shapes } => Some(shapes),
                _ => None,
            })
            .next_back()
            .expect("second MusicScalesListed (after configure)");
        assert_eq!(
            shapes_after.len(),
            15,
            "after 12-TET MusicConfigureSession, catalogue must have 15 shapes"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn start_then_stop_runs_through_state_machine() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let after_start = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
        let states: Vec<AudioSessionState> = after_start
            .iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![AudioSessionState::Starting, AudioSessionState::Running],
            "start should pass through Starting → Running"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        coach.send_command(Command::AudioStopSession);
        let after_stop = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });
        let stop_states: Vec<AudioSessionState> = after_stop
            .iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            stop_states,
            vec![AudioSessionState::Stopping, AudioSessionState::Idle]
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// A 12-TET, A=440 Bilawal scale (Sa Re Ga Ma Pa Dha Ni) with Sa
    /// rotated `shift` slots up from the A=440 reference line — the default
    /// musical frame for these tests. `shift = 0` puts Sa on the reference;
    /// `shift = 2` puts it two semitones up (Sa on B).
    fn bilawal_scale(shift: usize) -> Scale {
        let tuning = TuningAbsolute::at_reference(
            TuningKind::TwelveTet.intervals(),
            PitchLog2::from_hz(440.0),
        );
        let intervals = ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]);
        // Sa sits on the reference's pitch class; the integer octave is the
        // helix floor that carries 440 (ORIGIN = 1 Hz keeps only the class).
        let octave = (440f32.log2()).floor() as i32;
        Scale::new(intervals, tuning.shift_up(shift), octave)
    }

    /// Drain the coach once past a known reply (a `AudioDevicesListed` from a
    /// `AudioListDevices` we send) so we can assert what did *not* arrive
    /// before it. Used to prove `MusicConfigureSession` emits no event: a
    /// configure can't be polled *for* (it's silent), so we sandwich it
    /// before a command that does reply and check the accumulated events.
    fn drain_past_list_devices(coach: &impl AppCoach) -> Vec<CoachEvent> {
        coach.send_command(Command::AudioListDevices);
        poll_until(coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioDevicesListed { .. }))
        })
    }

    fn no_state_changes(events: &[CoachEvent]) -> bool {
        !events
            .iter()
            .any(|e| matches!(e, CoachEvent::AudioSessionStateChanged { .. }))
    }

    #[test]
    fn configure_in_idle_publishes_snapshot_and_event_no_state_change() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Before any configure, the snapshot is None.
        assert!(coach.music_info().is_none());

        let scale = bilawal_scale(0);
        coach.send_command(Command::MusicConfigureSession { scale });

        // Configure causes no *audio* state change, but it does emit a
        // MusicSessionConfigured event. Sandwich it before a AudioListDevices and
        // inspect the accumulated events.
        let events = drain_past_list_devices(&coach);
        assert!(
            no_state_changes(&events),
            "MusicConfigureSession in Idle must not change session state",
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. })),
            "MusicConfigureSession must not error",
        );
        // It DID emit the configure event with the payload we sent.
        assert!(
            events.iter().any(|e| matches!(
                e,
                CoachEvent::MusicSessionConfigured { scale: s } if *s == scale
            )),
            "MusicConfigureSession must emit MusicSessionConfigured with the new scale",
        );
        // And published the sticky snapshot — readable now, in Idle.
        let info = coach.music_info().expect("snapshot set after configure");
        assert_eq!(info.scale, scale);
        // It also must not have opened any audio device.
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn configure_while_running_keeps_running_and_snapshot_is_sticky() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Start audio → Running.
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });

        // Reconfigure mid-session. Decoupled: no *audio* state change,
        // but it does emit MusicSessionConfigured + update the snapshot.
        // A different tonic (Sa two slots up), to make the swap meaningful.
        let scale = bilawal_scale(2);
        coach.send_command(Command::MusicConfigureSession { scale });

        let events = drain_past_list_devices(&coach);
        assert!(
            no_state_changes(&events),
            "MusicConfigureSession while Running must not change session state",
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                CoachEvent::MusicSessionConfigured { scale: s } if *s == scale
            )),
            "reconfigure must emit MusicSessionConfigured with the new tonic",
        );
        // Audio was opened exactly once (the start); configure didn't
        // touch the audio lifecycle.
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(
            coach.audio_info().is_some(),
            "still Running after reconfigure",
        );
        // Snapshot reflects the latest configure.
        assert_eq!(coach.music_info().unwrap().scale, scale);

        coach.send_command(Command::AudioStopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });

        // Sticky: the musical snapshot survives the audio stop, unlike
        // audio_info which clears to None.
        assert!(coach.audio_info().is_none(), "audio_info clears on stop",);
        assert_eq!(
            coach.music_info().unwrap().scale,
            scale,
            "music_info is sticky across stop",
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn audio_info_is_some_only_while_running() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        assert!(
            coach.audio_info().is_none(),
            "Idle should have no session info"
        );

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
        let info = coach
            .audio_info()
            .expect("Running should expose session info");
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 1);

        coach.send_command(Command::AudioStopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });
        assert!(
            coach.audio_info().is_none(),
            "Idle (after stop) should have no session info"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn open_failure_lands_in_error_state_with_unsupported_config() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::FailUnsupported, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        let kind = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioSessionError { kind, .. } => Some(*kind),
                _ => None,
            })
            .expect("AudioSessionError");
        assert_eq!(kind, AudioSessionErrorKind::UnsupportedConfig);

        // State should have reached Error.
        let saw_error = events.iter().any(|e| {
            matches!(
                e,
                CoachEvent::AudioSessionStateChanged {
                    new_state: AudioSessionState::Error
                }
            )
        });
        assert!(
            saw_error,
            "should have emitted AudioSessionStateChanged(Error)"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn capture_open_failure_returns_feature_producer_for_retry() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::FailOnceThenOk, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        coach.send_command(Command::AudioStopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
        assert!(
            events.iter().all(|e| !matches!(
                e,
                CoachEvent::AudioSessionError {
                    kind: AudioSessionErrorKind::Other,
                    ..
                }
            )),
            "retry must not fail because the feature producer was lost"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 2);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn shutdown_is_idempotent() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
        assert_eq!(
            coach.shutdown(Duration::from_millis(1)),
            ShutdownResult::AlreadyShutDown
        );
    }

    #[test]
    fn start_while_running_is_silent_no_op() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Reach Running.
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
        let opens_before = opens.load(Ordering::SeqCst);

        // Second Start: should be ignored. No new state event, no
        // new open call.
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        // Give it a beat, then assert.
        thread::sleep(Duration::from_millis(50));
        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        assert!(
            buf.iter().all(|e| !matches!(
                e,
                CoachEvent::AudioSessionStateChanged {
                    new_state: AudioSessionState::Starting
                }
            )),
            "ignored Start must not emit a state change"
        );
        assert_eq!(opens.load(Ordering::SeqCst), opens_before);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn stale_device_id_fails_with_device_unavailable() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: Some(DeviceId("does-not-exist".into())),
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        let (kind, _reason) = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioSessionError { kind, reason } => Some((*kind, reason.clone())),
                _ => None,
            })
            .expect("AudioSessionError");
        assert_eq!(kind, AudioSessionErrorKind::DeviceUnavailable);
        // Capture must not have been opened for a stale id.
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn stop_while_idle_is_silent_no_op() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioStopSession);
        // Give the control plane a beat to (not) emit anything.
        thread::sleep(Duration::from_millis(50));

        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        let saw_state_change = buf
            .iter()
            .any(|e| matches!(e, CoachEvent::AudioSessionStateChanged { .. }));
        assert!(
            !saw_state_change,
            "Stop while Idle must not emit a state change"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn start_stop_start_round_trip_resets_cleanly() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Cycle 1: Start → Running.
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        // Stop → Idle.
        coach.send_command(Command::AudioStopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });

        // Cycle 2: Start again — must reopen capture and reach Running.
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let after = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });
        let states: Vec<AudioSessionState> = after
            .iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![AudioSessionState::Starting, AudioSessionState::Running],
            "second Start must transition Idle → Starting → Running"
        );
        assert_eq!(
            opens.load(Ordering::SeqCst),
            2,
            "capture must be reopened on the second Start"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// Test A: negotiate() returns a sample_rate DIFFERENT from what was
    /// requested. The engine and AudioInfo must be built for the NEGOTIATED
    /// rate, not the requested guess.
    #[test]
    fn negotiate_different_rate_engine_built_for_negotiated_rate() {
        let opens = Arc::new(AtomicU32::new(0));
        // The fake device says it will deliver 44100 instead of 48000.
        let negotiated_cfg = CaptureConfig {
            sample_rate: 44_100,
            channels: 1,
            buffer_frames: Some(441),
        };
        let (deps, _tel, opened_configs) = deps_with_negotiate(
            FakeOutcome::Ok,
            Arc::clone(&opens),
            NegotiateResult::Return(negotiated_cfg),
        );
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None, // would default to 48000 via preferred_sample_rate
            buffer_frames: None,
            session_label: None,
        }));

        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });

        // AudioInfo must reflect the NEGOTIATED rate and buffer, not the requested defaults.
        let info = coach.audio_info().expect("Running should have AudioInfo");
        assert_eq!(
            info.sample_rate, 44_100,
            "AudioInfo.sample_rate must be the negotiated rate"
        );
        assert_eq!(
            info.buffer_frames,
            Some(441),
            "AudioInfo.buffer_frames must be the negotiated buffer_frames"
        );

        // open() must have been called with the NEGOTIATED config, not the requested defaults.
        let configs = opened_configs.lock().unwrap();
        assert_eq!(
            configs.len(),
            1,
            "open() must have been called exactly once"
        );
        assert_eq!(
            configs[0].sample_rate, 44_100,
            "open() must receive the negotiated sample_rate"
        );
        assert_eq!(
            configs[0].buffer_frames,
            Some(441),
            "open() must receive the negotiated buffer_frames"
        );
        assert_eq!(
            configs[0].channels, 1,
            "open() must receive the negotiated channels"
        );
        drop(configs);

        // capture was opened exactly once.
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// Test B: negotiate() returns Err(UnsupportedConfig). The session must
    /// land in Error with AudioSessionErrorKind::UnsupportedConfig and open() must
    /// NEVER be called.
    #[test]
    fn negotiate_failure_lands_in_error_before_open() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel, _opened_configs) = deps_with_negotiate(
            FakeOutcome::Ok, // open() would succeed — but must never be called
            Arc::clone(&opens),
            NegotiateResult::Fail(48_000, 1),
        );
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));

        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        // Must have errored with UnsupportedConfig.
        let kind = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioSessionError { kind, .. } => Some(*kind),
                _ => None,
            })
            .expect("AudioSessionError must be emitted");
        assert_eq!(
            kind,
            AudioSessionErrorKind::UnsupportedConfig,
            "negotiate failure must map to UnsupportedConfig"
        );

        // State must be Error.
        let saw_error = events.iter().any(|e| {
            matches!(
                e,
                CoachEvent::AudioSessionStateChanged {
                    new_state: AudioSessionState::Error
                }
            )
        });
        assert!(saw_error, "state must reach Error on negotiate failure");

        // open() must never have been called — the error came before DataPlane::start.
        assert_eq!(
            opens.load(Ordering::SeqCst),
            0,
            "open() must not be called when negotiate() fails"
        );

        // AudioInfo must be None (no session was opened).
        assert!(
            coach.audio_info().is_none(),
            "audio_info must be None when negotiate() failed"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn zero_timeout_shutdown_respects_contract() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Reach Running so there's actual state to tear down.
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });

        // shutdown(ZERO) is inherently a race between Quit landing
        // and the control plane returning. Both outcomes are
        // contractually valid; what's NOT valid is panicking or
        // returning AlreadyShutDown on the first call.
        let first = coach.shutdown(Duration::ZERO);
        assert!(
            matches!(first, ShutdownResult::Clean | ShutdownResult::TimedOut),
            "first shutdown(ZERO) must be Clean or TimedOut, got {first:?}"
        );

        // Subsequent shutdown is always AlreadyShutDown, regardless
        // of whether the thread had already finished.
        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::AlreadyShutDown
        );
    }

    // ---- permission state machine tests (use FakeAudioDriver) ----

    use domain_ports::audio_driver::{AudioInitStatus as Status, FakeAudioDriver};

    /// Build `AppCoachDeps` with a `FakeAudioDriver` (for permission tests)
    /// and a `FakeCapture` whose outcome is `Ok`. Returns the provider handle so
    /// the test can drive it.
    fn deps_with_provider(
        initial_status: Status,
    ) -> (AppCoachDeps, Arc<FakeAudioDriver>, Arc<AtomicU32>) {
        let clock: Arc<dyn domain_ports::clock::Clock> = Arc::new(TestClock::new(0));
        let telemetry = Arc::new(TestTelemetry::new(Arc::clone(&clock)));

        let stream = InputStream {
            handle: StreamHandle(Arc::new(())),
            name: "fake mic".into(),
            channels: 1,
            sample_rates: SampleRateSupport::Ranges(vec![(48_000, 48_000)]),
        };
        let device = InputDevice {
            persistent_id: Some(DeviceId("fake-id".into())),
            name: "fake mic".into(),
            transport: Transport::BuiltIn,
            streams: vec![clone_input_stream(&stream)],
        };

        let provider = Arc::new(FakeAudioDriver::new(initial_status));
        // Pre-load the provider with a FakeDevices result for when new_devices() is called.
        // We set this up as a granted devices source wrapping our stream/device.
        // The provider returns EmptyDevices by default, which causes DeviceUnavailable.
        // For tests that reach new_devices(), we must set devices_result beforehand.
        // Tests that DON'T reach new_devices() (Denied/Undetermined start) are fine with empty.
        // Tests that DO reach new_devices() (Granted start) call set_devices_result before.
        //
        // For simplicity, we use GrantedAudioDriver for those tests and only use
        // provider for permission-flow-only tests.
        let _ = device; // only used by set_devices_result callers
        let _ = stream;

        let opens = Arc::new(AtomicU32::new(0));
        let capture_port: Arc<dyn domain_ports::audio_capture::AudioCapture> =
            Arc::new(FakeCapture {
                opens: Arc::clone(&opens),
                outcome: FakeOutcome::Ok,
                negotiate_result: NegotiateResult::PassThrough,
                opened_configs: Arc::new(Mutex::new(Vec::new())),
                last_sink: Arc::new(Mutex::new(None)),
            });

        let deps = AppCoachDeps {
            clock,
            telemetry: telemetry.clone(),
            audio_driver: Arc::clone(&provider) as Arc<dyn domain_ports::audio_driver::AudioDriver>,
            audio_capture: capture_port,
            host_version: "test",
        };
        (deps, provider, opens)
    }

    #[test]
    fn permission_query_returns_current_status_undetermined() {
        let (deps, _provider, _opens) = deps_with_provider(Status::Undetermined);
        let coach = new(deps);

        coach.send_command(Command::AudioPermissionQuery);
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioPermissionStatus { .. }))
        });

        let status = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioPermissionStatus { status } => Some(*status),
                _ => None,
            })
            .expect("AudioPermissionStatus reply");
        assert_eq!(status, Status::Undetermined);
        // No audio state change.
        assert!(!events
            .iter()
            .any(|e| matches!(e, CoachEvent::AudioSessionStateChanged { .. })));

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn permission_request_parks_then_resolves_to_granted() {
        let (deps, provider, _opens) = deps_with_provider(Status::Undetermined);
        let coach = new(deps);

        coach.send_command(Command::AudioPermissionRequest);

        // Give the control plane a beat to receive the command and store the sink.
        thread::sleep(Duration::from_millis(30));
        assert!(
            provider.has_pending_request(),
            "control plane must have stored the sink"
        );

        // Flip status and resolve the sink.
        provider.set_status(Status::Granted);
        provider.resolve();

        // Now we should get AudioPermissionStatus { Granted }.
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioPermissionStatus { .. }))
        });
        let status = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioPermissionStatus { status } => Some(*status),
                _ => None,
            })
            .expect("AudioPermissionStatus reply");
        assert_eq!(status, Status::Granted);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn start_with_granted_reaches_running_no_permission_event() {
        // Use the GrantedAudioDriver (wraps FakeDevices) so new_devices() works.
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });

        // Must NOT have emitted AudioPermissionStatus (no prompting on a start).
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoachEvent::AudioPermissionStatus { .. })),
            "start with Granted must not emit AudioPermissionStatus"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn start_with_denied_errors_with_permission_denied_no_prompt() {
        let (deps, _provider, opens) = deps_with_provider(Status::Denied);
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        let kind = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioSessionError { kind, .. } => Some(*kind),
                _ => None,
            })
            .expect("AudioSessionError");
        assert_eq!(kind, AudioSessionErrorKind::PermissionDenied);
        // Must NOT have emitted AudioPermissionStatus (no auto-prompt).
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoachEvent::AudioPermissionStatus { .. })),
            "start with Denied must not emit AudioPermissionStatus"
        );
        // Must not have opened capture.
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn start_with_undetermined_errors_no_auto_prompt() {
        let (deps, _provider, opens) = deps_with_provider(Status::Undetermined);
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        // Must have errored (PermissionDenied covers Undetermined too).
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. })),
            "start with Undetermined must produce an error"
        );
        // Must NOT have emitted AudioPermissionStatus (no auto-prompt).
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoachEvent::AudioPermissionStatus { .. })),
            "start with Undetermined must not emit AudioPermissionStatus (no auto-prompt)"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn stale_permission_resolved_after_generation_bump_is_dropped() {
        // Scenario: permission request captures generation N; a subsequent
        // start+stop bumps the generation to N+2; the old sink fires → dropped.
        // Scenario: request at gen=0; start bumps gen to 1; old sink fires → dropped.
        let (dep2, provider2, _) = deps_with_provider(Status::Undetermined);
        let coach = new(dep2);

        // 1) Issue permission request at generation 0; sink captures gen=0.
        coach.send_command(Command::AudioPermissionRequest);
        thread::sleep(Duration::from_millis(30));
        assert!(provider2.has_pending_request(), "sink must be pending");

        // 2) Flip to Granted; start — this bumps generation to 1.
        //    EmptyDevices causes DeviceUnavailable, but generation is still bumped.
        provider2.set_status(Status::Granted);
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });
        // gen=1. Old sink still holds gen=0.

        // 3) Resolve the stale sink (gen=0 ≠ current gen=1) → dropped.
        provider2.resolve();

        thread::sleep(Duration::from_millis(30));
        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        let has_permission_status = buf
            .iter()
            .any(|e| matches!(e, CoachEvent::AudioPermissionStatus { .. }));
        assert!(
            !has_permission_status,
            "stale AudioPermissionResolved (gen=0, current gen=1) must be dropped"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    #[test]
    fn activation_failed_produces_error() {
        use domain_ports::audio_driver::fakes::{FakeActivationError, FakeDevicesResult};

        let (deps, provider, _opens) = deps_with_provider(Status::Granted);
        // Configure new_devices() to fail with ActivationFailed.
        provider.set_devices_result(FakeDevicesResult::Err(
            FakeActivationError::ActivationFailed("hw failure".to_string()),
        ));
        let coach = new(deps);

        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });

        let kind = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioSessionError { kind, .. } => Some(*kind),
                _ => None,
            })
            .expect("AudioSessionError");
        // ActivationFailed maps to Other (it's a generic start failure, not a permission error).
        assert_eq!(kind, AudioSessionErrorKind::Other);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    // ---- lifecycle seam tests (Phase 1.6.1c) ----
    //
    // These inject `LifecycleEvent`s through the sink the control plane handed
    // to the fake capture's `open()`, and assert the resulting state machine
    // transitions per Decision 3's taxonomy.

    /// Collect the state-change sequence observed so far (best-effort drain).
    fn collect_states(coach: &impl AppCoach) -> Vec<AudioSessionState> {
        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        buf.iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect()
    }

    /// Each terminal `LifecycleEvent` drives the session to `Error` with the
    /// classified kind, and clears `AudioInfo`.
    #[test]
    fn terminal_lifecycle_events_drive_error_with_classified_kind() {
        for (event, expected) in [
            (
                LifecycleEvent::BackendError {
                    reason: "boom".into(),
                },
                AudioSessionErrorKind::MidStreamFailure,
            ),
            (
                LifecycleEvent::MediaServicesReset,
                AudioSessionErrorKind::MidStreamFailure,
            ),
            (
                LifecycleEvent::PermissionDenied,
                AudioSessionErrorKind::PermissionDenied,
            ),
        ] {
            let opens = Arc::new(AtomicU32::new(0));
            let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
            let coach = new(deps);
            start_to_running(&coach);

            firer.fire(event.clone());

            let events = poll_until(&coach, |evs| {
                evs.iter()
                    .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
            });
            let kind = events
                .iter()
                .find_map(|e| match e {
                    CoachEvent::AudioSessionError { kind, .. } => Some(*kind),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("AudioSessionError expected for {event:?}"));
            assert_eq!(kind, expected, "wrong kind for {event:?}");
            assert!(
                coach.audio_info().is_none(),
                "AudioInfo must clear on terminal {event:?}"
            );

            assert_eq!(
                coach.shutdown(Duration::from_secs(1)),
                ShutdownResult::Clean
            );
        }
    }

    /// `Interrupted` stops capture and clears `AudioInfo`, transitioning
    /// Running → Stopping → Idle (same shape as a user stop; the coach state
    /// does not encode *why*).
    #[test]
    fn interrupted_stops_and_clears_audio_info() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);
        // Drain the start events so we only see what the interruption produces.
        let _ = collect_states(&coach);

        firer.fire(LifecycleEvent::Interrupted);

        let events = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });
        let states: Vec<AudioSessionState> = events
            .iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![AudioSessionState::Stopping, AudioSessionState::Idle],
            "Interrupted must drive Running → Stopping → Idle"
        );
        assert!(
            coach.audio_info().is_none(),
            "AudioInfo must be cleared on interruption"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// A lifecycle event whose generation is stale (the session was stopped
    /// and restarted, bumping the generation) is dropped — no transition.
    #[test]
    fn stale_generation_lifecycle_event_is_dropped() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Reach Running (generation now N), capture the sink for that session.
        start_to_running(&coach);
        // Stop (bumps generation) then start again (bumps again) — the sink
        // captured above now carries a stale generation.
        coach.send_command(Command::AudioStopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });
        // NOTE: a fresh start would overwrite last_sink, so hold the OLD sink
        // by firing through `firer` *before* the next start. Instead, we keep
        // the stale-sink semantics by firing now (post-stop): generation has
        // bumped, the session is Idle, and the event must be ignored.
        let _ = collect_states(&coach);
        firer.fire(LifecycleEvent::BackendError {
            reason: "late".into(),
        });

        thread::sleep(Duration::from_millis(60));
        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        assert!(
            !buf.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. })),
            "stale-generation lifecycle event must be dropped (no Error)"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// Duplicate/coalesced terminal events are idempotent: firing the same
    /// event twice lands in the same end state (the second is stale once the
    /// first tore the session down).
    #[test]
    fn duplicate_terminal_event_is_idempotent() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);

        firer.fire(LifecycleEvent::BackendError {
            reason: "first".into(),
        });
        // Fire the same event again — the session is already terminal; the
        // second is harmless.
        firer.fire(LifecycleEvent::BackendError {
            reason: "second".into(),
        });

        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });
        let error_count = events
            .iter()
            .filter(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
            .count();
        assert_eq!(
            error_count, 1,
            "duplicate terminal event must produce exactly one Error transition"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// Retry budget: `DeviceUnavailable` with a default-input intent triggers
    /// reconcile, whose reopen keeps failing; after RETRY_BUDGET consecutive
    /// failures the session goes terminal `Error`.
    #[test]
    fn retry_budget_trips_to_error_after_failed_reopens() {
        let opens = Arc::new(AtomicU32::new(0));
        // First open succeeds (reaches Running); every reopen fails.
        let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::OkThenFail, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);

        // Default intent → DeviceUnavailable triggers reconcile → reopen fails
        // → retry → ... → Error once the budget is spent.
        firer.fire(LifecycleEvent::DeviceUnavailable);

        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. })),
            "exhausted retry budget must land in Error"
        );
        // First open + RETRY_BUDGET failed reopens.
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1 + 3,
            "one successful open plus RETRY_BUDGET (3) failed reopens"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// The interruption timeout fires when `InterruptionEnded` never arrives,
    /// resolving the paused state without wedging (stays Idle, not auto-resumed).
    #[test]
    fn interruption_timeout_fires_and_does_not_wedge() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);

        firer.fire(LifecycleEvent::Interrupted);
        // Session should reach Idle (capture stopped).
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });

        // Advance the fake clock past INTERRUPTION_TIMEOUT_MS; the deadline
        // loop's timeout branch should fire it. The session stays Idle (no
        // auto-resume) and remains responsive (a subsequent stop is clean).
        clock.advance_ns(10_000 * 1_000_000); // 10s > 8s timeout
        thread::sleep(Duration::from_millis(60));

        // No spurious transition out of Idle (no auto-resume to Running).
        let states = collect_states(&coach);
        assert!(
            !states.contains(&AudioSessionState::Running),
            "interruption timeout must NOT auto-resume to Running"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// `DeviceUnavailable` with a *specific* device intent is terminal (the
    /// named device is gone; there is nothing to re-select).
    #[test]
    fn device_unavailable_specific_intent_is_terminal() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Start with a SPECIFIC device id (the fake device's id).
        coach.send_command(Command::AudioStartSession(AudioConfig {
            device_id: Some(DeviceId("fake-id".into())),
            sample_rate: None,
            buffer_frames: None,
            session_label: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Running
                    }
                )
            })
        });

        firer.fire(LifecycleEvent::DeviceUnavailable);

        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. }))
        });
        let kind = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::AudioSessionError { kind, .. } => Some(*kind),
                _ => None,
            })
            .expect("AudioSessionError expected for specific-device loss");
        assert_eq!(kind, AudioSessionErrorKind::MidStreamFailure);
        // Exactly one open: the start. No reconcile reopen for a specific device.
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1,
            "no reopen for specific device"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// `DeviceUnavailable` with a default-input intent re-selects (reconcile):
    /// the reopen succeeds and the session returns to Running.
    #[test]
    fn device_unavailable_default_intent_reselects_and_recovers() {
        let opens = Arc::new(AtomicU32::new(0));
        // Every open succeeds, so the reconcile reopen recovers cleanly.
        let (deps, firer, _clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);
        let _ = collect_states(&coach);

        firer.fire(LifecycleEvent::DeviceUnavailable);

        // Recovery cycle: Stopping → Starting → Running.
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .filter(|e| {
                    matches!(
                        e,
                        CoachEvent::AudioSessionStateChanged {
                            new_state: AudioSessionState::Running
                        }
                    )
                })
                .count()
                >= 1
        });
        let states: Vec<AudioSessionState> = events
            .iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![
                AudioSessionState::Stopping,
                AudioSessionState::Starting,
                AudioSessionState::Running,
            ],
            "default-intent device loss must reconcile back to Running"
        );
        assert!(
            coach.audio_info().is_some(),
            "AudioInfo republished after recovery"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 2, "start + one reopen");

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// `InterruptionEnded` clears the interruption timeout but does NOT
    /// auto-resume — the coach stays Idle (the head's Pause screen owns the
    /// resume decision).
    #[test]
    fn interruption_ended_does_not_auto_resume() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);

        firer.fire(LifecycleEvent::Interrupted);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });
        let _ = collect_states(&coach);

        // `InterruptionEnded` arrives through the SAME sink as `Interrupted`
        // (same generation — interruption does not bump). It must be processed
        // (not gated as stale): it clears the interruption timeout but does
        // NOT auto-resume.
        firer.fire(LifecycleEvent::InterruptionEnded {
            should_resume: true,
        });
        thread::sleep(Duration::from_millis(60));

        let states = collect_states(&coach);
        assert!(
            !states.contains(&AudioSessionState::Running),
            "InterruptionEnded must NOT auto-resume to Running"
        );
        assert!(coach.audio_info().is_none(), "still stopped after ended");

        // The timeout was cleared by InterruptionEnded — advancing past it
        // must do nothing (no state churn, no wedge).
        clock.advance_ns(20_000 * 1_000_000); // 20s ≫ 8s timeout
        thread::sleep(Duration::from_millis(60));
        let late = collect_states(&coach);
        assert!(
            late.is_empty(),
            "cleared interruption timeout must not fire after InterruptionEnded"
        );
        // Only the initial start opened the device.
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// `RouteChanged` is debounced: the reconcile fires only after the
    /// debounce window elapses (driven by advancing the fake clock), and the
    /// reopen returns the session to Running.
    #[test]
    fn route_changed_debounces_then_reconciles_to_running() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);
        let _ = collect_states(&coach);

        // A burst of route changes — all coalesced into one reconcile.
        firer.fire(LifecycleEvent::RouteChanged);
        firer.fire(LifecycleEvent::RouteChanged);

        // Before the debounce elapses, no reopen has happened.
        thread::sleep(Duration::from_millis(40));
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1,
            "route change must not reconcile before the debounce window"
        );

        // Advance past the debounce window; the loop's timeout branch fires
        // the deadline → one reconcile → back to Running.
        clock.advance_ns(1_000 * 1_000_000); // 1s ≫ 150ms debounce
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .filter(|e| {
                    matches!(
                        e,
                        CoachEvent::AudioSessionStateChanged {
                            new_state: AudioSessionState::Running
                        }
                    )
                })
                .count()
                >= 1
        });
        let states: Vec<AudioSessionState> = events
            .iter()
            .filter_map(|e| match e {
                CoachEvent::AudioSessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![
                AudioSessionState::Stopping,
                AudioSessionState::Starting,
                AudioSessionState::Running,
            ],
            "debounced route change must reconcile Stopping → Starting → Running"
        );
        // start + exactly one reopen (the burst coalesced).
        assert_eq!(
            opens.load(Ordering::SeqCst),
            2,
            "coalesced route-change burst must reopen exactly once"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// A route-debounce armed before an interruption must not survive to fire
    /// a reconcile against a later user-started session.
    #[test]
    fn route_debounce_does_not_survive_interruption_into_next_session() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);
        let _ = collect_states(&coach);

        // Arm a route debounce, then immediately interrupt (before it fires).
        firer.fire(LifecycleEvent::RouteChanged);
        firer.fire(LifecycleEvent::Interrupted);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::AudioSessionStateChanged {
                        new_state: AudioSessionState::Idle
                    }
                )
            })
        });
        let _ = collect_states(&coach);

        // User starts a fresh session.
        start_to_running(&coach);
        let _ = collect_states(&coach);
        let opens_after_restart = opens.load(Ordering::SeqCst);

        // Advance the clock well past any debounce/timeout; the stale
        // pre-interruption debounce must NOT fire a reconcile on this session.
        clock.advance_ns(60_000 * 1_000_000); // 60s
        thread::sleep(Duration::from_millis(80));

        let churn = collect_states(&coach);
        assert!(
            churn.is_empty(),
            "stale route debounce must not reconcile the fresh session: {churn:?}"
        );
        assert_eq!(
            opens.load(Ordering::SeqCst),
            opens_after_restart,
            "no spurious reopen from a stale debounce"
        );
        assert!(coach.audio_info().is_some(), "fresh session stays Running");

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// A late event fired through the *old* stream's sink after a recovery
    /// reopen is dropped (recovery bumps the generation), so it cannot disturb
    /// the new, healthy session.
    #[test]
    fn old_stream_sink_is_stale_after_recovery_reopen() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, firer, clock) = deps_for_lifecycle(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);
        start_to_running(&coach);

        // Drain the initial start events so the Running we wait for below is
        // the *recovery* one, not the start's.
        let _ = collect_states(&coach);

        // Hold the FIRST session's sink (generation N).
        let old_sink = firer.take_sink();

        // Trigger a recovery via a debounced route change → reconcile reopens
        // (generation bumps) and installs a fresh sink. Fire through the held
        // old sink (still the current generation at this point). Let the
        // control thread arm the debounce (against fake-clock 0) *before*
        // advancing the clock past the window — otherwise the deadline would
        // be armed relative to the advanced time and never fire.
        old_sink(LifecycleEvent::RouteChanged);
        thread::sleep(Duration::from_millis(40));
        clock.advance_ns(1_000 * 1_000_000);
        let _ = poll_until(&coach, |evs| {
            evs.iter()
                .filter(|e| {
                    matches!(
                        e,
                        CoachEvent::AudioSessionStateChanged {
                            new_state: AudioSessionState::Running
                        }
                    )
                })
                .count()
                >= 1
        });
        assert_eq!(opens.load(Ordering::SeqCst), 2, "reconcile reopened once");
        let _ = collect_states(&coach);

        // Now fire a TERMINAL event through the OLD (generation-N) sink. It is
        // stale — the reconcile bumped the generation — so it must be dropped.
        old_sink(LifecycleEvent::BackendError {
            reason: "late from dead stream".into(),
        });

        thread::sleep(Duration::from_millis(60));
        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        assert!(
            !buf.iter()
                .any(|e| matches!(e, CoachEvent::AudioSessionError { .. })),
            "old-stream sink after recovery must be stale and dropped (no Error)"
        );
        // Session is still healthy.
        assert!(
            coach.audio_info().is_some(),
            "session must remain Running — the stale event did not tear it down"
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }
}
