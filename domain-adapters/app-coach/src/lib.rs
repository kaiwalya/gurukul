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
        AudioCapture, CaptureCallback, CaptureConfig, CaptureError, CaptureSession,
    };
    use domain_ports::audio_devices::{
        AudioDevices, DeviceId, InputDevice, InputStream, SampleRateSupport, StreamHandle,
        Transport,
    };
    use domain_ports::audio_session::{
        AudioInitError, AudioInitStatus, AudioPermissionSink, AudioSessionProvider,
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

    /// A session provider that always reports `Granted` and returns the
    /// supplied `FakeDevices` from `new_devices()`. Used by the existing
    /// tests that don't care about the permission flow.
    struct GrantedSessionProvider {
        devices: FakeDevices,
    }

    impl AudioSessionProvider for GrantedSessionProvider {
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
    }

    #[derive(Clone)]
    enum FakeOutcome {
        Ok,
        FailUnsupported,
        FailOnceThenOk,
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
        ) -> Result<CaptureSession, CaptureError> {
            self.opened_configs.lock().unwrap().push(CaptureConfig {
                sample_rate: cfg.sample_rate,
                channels: cfg.channels,
                buffer_frames: cfg.buffer_frames,
            });
            let open_index = self.opens.fetch_add(1, Ordering::SeqCst);
            match (&self.outcome, open_index) {
                (FakeOutcome::Ok, _) | (FakeOutcome::FailOnceThenOk, 1..) => {
                    Ok(CaptureSession::new(|| {}))
                }
                (FakeOutcome::FailUnsupported | FakeOutcome::FailOnceThenOk, _) => {
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
        let session_port: Arc<dyn domain_ports::audio_session::AudioSessionProvider> =
            Arc::new(GrantedSessionProvider {
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
        });
        (
            AppCoachDeps {
                clock,
                telemetry: telemetry.clone(),
                audio_session: session_port,
                audio_capture: capture_port,
                host_version: "test",
            },
            telemetry,
            opened_configs,
        )
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

    // ---- permission state machine tests (use FakeAudioSessionProvider) ----

    use domain_ports::audio_session::{AudioInitStatus as Status, FakeAudioSessionProvider};

    /// Build `AppCoachDeps` with a `FakeAudioSessionProvider` (for permission tests)
    /// and a `FakeCapture` whose outcome is `Ok`. Returns the provider handle so
    /// the test can drive it.
    fn deps_with_provider(
        initial_status: Status,
    ) -> (AppCoachDeps, Arc<FakeAudioSessionProvider>, Arc<AtomicU32>) {
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

        let provider = Arc::new(FakeAudioSessionProvider::new(initial_status));
        // Pre-load the provider with a FakeDevices result for when new_devices() is called.
        // We set this up as a granted devices source wrapping our stream/device.
        // The provider returns EmptyDevices by default, which causes DeviceUnavailable.
        // For tests that reach new_devices(), we must set devices_result beforehand.
        // Tests that DON'T reach new_devices() (Denied/Undetermined start) are fine with empty.
        // Tests that DO reach new_devices() (Granted start) call set_devices_result before.
        //
        // For simplicity, we use GrantedSessionProvider for those tests and only use
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
            });

        let deps = AppCoachDeps {
            clock,
            telemetry: telemetry.clone(),
            audio_session: Arc::clone(&provider)
                as Arc<dyn domain_ports::audio_session::AudioSessionProvider>,
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
        // Use the GrantedSessionProvider (wraps FakeDevices) so new_devices() works.
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
        use domain_ports::audio_session::fakes::{FakeActivationError, FakeDevicesResult};

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
}
