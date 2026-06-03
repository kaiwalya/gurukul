//! adapter-app-coach: the canonical [`AppCoach`] implementation.
//!
//! # Architecture
//!
//! Implements the two-plane shape from `docs/SPEC-AppCoach.md` §7.
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
//!   `ArcSwap<Option<FeatureSnapshot>>`. Heads sample with
//!   [`AppCoach::latest_features`].
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
//! - [`data_plane`] — the worker thread, SPSC ring, and ArcSwap
//!   publisher.
//! - [`pitch_world`] — builds the dsp engine from the embedded
//!   `coach.json` world.
//! - [`outbound`] — the bounded outbound queue.
//! - [`shutdown`] — `join_with_timeout` helper.
//! - [`helpers`] — capture-error classification, sample-rate picker.
//! - This file — `pub fn new`, `CoachImpl`, the [`AppCoach`] impl, the
//!   `Drop` glue, and the `#[cfg(test)]` test module covering the
//!   end-to-end boundary.

mod control_plane;
mod data_plane;
mod helpers;
mod inspect;
mod outbound;
mod pitch_world;
mod shutdown;

use control_plane::{ControlPlane, Input};
use inspect::{EngineInspectImpl, InspectShared};
use outbound::OutboundQueue;
use shutdown::join_with_timeout;

use arc_swap::ArcSwap;
use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, AudioInfo, CoachEvent, Command, FeatureSnapshot, MusicInfo,
    ShutdownResult,
};
use domain_ports::engine_inspect::EngineInspect;
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
    let audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>> =
        Arc::new(ArcSwap::from_pointee(None));
    let audio_info_publisher_for_thread = Arc::clone(&audio_info_publisher);
    let music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>> =
        Arc::new(ArcSwap::from_pointee(None));
    let music_info_publisher_for_thread = Arc::clone(&music_info_publisher);
    let inspect_shared = InspectShared::new();
    let inspect_for_thread = Arc::clone(&inspect_shared);

    let control_thread = thread::Builder::new()
        .name("app-coach-control".into())
        .spawn(move || {
            ControlPlane::new(
                deps,
                outbound_for_thread,
                rx_cmd,
                feature_publisher_for_thread,
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
        audio_info_publisher,
        music_info_publisher,
        control_thread: Mutex::new(Some(control_thread)),
        shut_down: Mutex::new(false),
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
    /// Lock-free snapshot of the negotiated session parameters. Written
    /// by the control plane before emitting `SessionStateChanged(Running)`
    /// and cleared before the next transition out — see [`AudioInfo`]
    /// for the ordering contract.
    audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>>,
    /// Lock-free sticky snapshot of the musical frame of reference.
    /// Written by the control plane in the configure handler (before
    /// the `SessionConfigured` event); survives start/stop. See
    /// [`MusicInfo`].
    music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>>,
    control_thread: Mutex<Option<JoinHandle<()>>>,
    shut_down: Mutex<bool>,
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

    fn audio_info(&self) -> Option<AudioInfo> {
        (**self.audio_info_publisher.load()).clone()
    }

    fn music_info(&self) -> Option<MusicInfo> {
        **self.music_info_publisher.load()
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
    use domain_ports::app_coach::{AudioConfig, SessionErrorKind, SessionState};
    use domain_ports::audio_capture::{
        AudioCapture, CaptureCallback, CaptureConfig, CaptureError, CaptureSession,
    };
    use domain_ports::audio_devices::{
        AudioDevices, DeviceId, InputDevice, InputStream, SampleRateSupport, StreamHandle,
        Transport,
    };
    use domain_ports::clock::{Clock, TestClock};
    use domain_ports::music::{harmonium_key, Tonality, TuningKind, TuningSpec};
    use domain_ports::telemetry::TestTelemetry;
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

    struct FakeCapture {
        opens: Arc<AtomicU32>,
        outcome: FakeOutcome,
    }

    #[derive(Clone)]
    enum FakeOutcome {
        Ok,
        FailUnsupported,
    }

    impl AudioCapture for FakeCapture {
        fn open(
            &self,
            _handle: StreamHandle,
            _cfg: CaptureConfig,
            _on_frame: CaptureCallback,
        ) -> Result<CaptureSession, CaptureError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                FakeOutcome::Ok => Ok(CaptureSession::new(|| {})),
                FakeOutcome::FailUnsupported => Err(CaptureError::UnsupportedConfig {
                    reason: "test".into(),
                }),
            }
        }
    }

    // ---- deps builder ----

    fn deps_with(
        outcome: FakeOutcome,
        opens: Arc<AtomicU32>,
    ) -> (AppCoachDeps, Arc<TestTelemetry>) {
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
        let devices_port: Arc<dyn AudioDevices> = Arc::new(FakeDevices {
            devices: vec![device],
            default: Some(stream),
        });
        let capture_port: Arc<dyn AudioCapture> = Arc::new(FakeCapture { opens, outcome });
        (
            AppCoachDeps {
                clock,
                telemetry: telemetry.clone(),
                audio_devices: devices_port,
                audio_capture: capture_port,
                host_version: "test",
            },
            telemetry,
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

    #[test]
    fn list_devices_round_trip() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::ListDevices);
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::DevicesListed { .. }))
        });

        let listed = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::DevicesListed { devices } => Some(devices),
                _ => None,
            })
            .expect("DevicesListed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "fake mic");

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

        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let after_start = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
                    }
                )
            })
        });
        let states: Vec<SessionState> = after_start
            .iter()
            .filter_map(|e| match e {
                CoachEvent::SessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![SessionState::Starting, SessionState::Running],
            "start should pass through Starting → Running"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        coach.send_command(Command::StopSession);
        let after_stop = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Idle
                    }
                )
            })
        });
        let stop_states: Vec<SessionState> = after_stop
            .iter()
            .filter_map(|e| match e {
                CoachEvent::SessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            stop_states,
            vec![SessionState::Stopping, SessionState::Idle]
        );

        assert_eq!(
            coach.shutdown(Duration::from_secs(1)),
            ShutdownResult::Clean
        );
    }

    /// A 12-TET, A=440 tuning with Bilawal (Sa Re Ga Ma Pa Dha Ni) on
    /// slot 0 — the default musical frame for these tests.
    fn bilawal_config() -> (TuningSpec, Tonality) {
        let tuning = TuningSpec {
            root_note_hz: 261.625_56, // C
            kind: TuningKind::TwelveTet,
            root: harmonium_key(0),
        };
        let tonality = Tonality::new(harmonium_key(0), &[2, 2, 1, 2, 2, 2, 1]);
        (tuning, tonality)
    }

    /// Drain the coach once past a known reply (a `DevicesListed` from a
    /// `ListDevices` we send) so we can assert what did *not* arrive
    /// before it. Used to prove `ConfigureSession` emits no event: a
    /// configure can't be polled *for* (it's silent), so we sandwich it
    /// before a command that does reply and check the accumulated events.
    fn drain_past_list_devices(coach: &impl AppCoach) -> Vec<CoachEvent> {
        coach.send_command(Command::ListDevices);
        poll_until(coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::DevicesListed { .. }))
        })
    }

    fn no_state_changes(events: &[CoachEvent]) -> bool {
        !events
            .iter()
            .any(|e| matches!(e, CoachEvent::SessionStateChanged { .. }))
    }

    #[test]
    fn configure_in_idle_publishes_snapshot_and_event_no_state_change() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Before any configure, the snapshot is None.
        assert!(coach.music_info().is_none());

        let (tuning, tonality) = bilawal_config();
        coach.send_command(Command::ConfigureSession { tuning, tonality });

        // Configure causes no *audio* state change, but it does emit a
        // SessionConfigured event. Sandwich it before a ListDevices and
        // inspect the accumulated events.
        let events = drain_past_list_devices(&coach);
        assert!(
            no_state_changes(&events),
            "ConfigureSession in Idle must not change session state",
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoachEvent::SessionError { .. })),
            "ConfigureSession must not error",
        );
        // It DID emit the configure event with the payload we sent.
        assert!(
            events.iter().any(|e| matches!(
                e,
                CoachEvent::SessionConfigured { tonality: t, .. } if *t == tonality
            )),
            "ConfigureSession must emit SessionConfigured with the new tonality",
        );
        // And published the sticky snapshot — readable now, in Idle.
        let info = coach.music_info().expect("snapshot set after configure");
        assert_eq!(info.tonality, tonality);
        assert_eq!(info.tuning, tuning);
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
        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
                    }
                )
            })
        });

        // Reconfigure mid-session. Decoupled: no *audio* state change,
        // but it does emit SessionConfigured + update the snapshot.
        let (tuning, _) = bilawal_config();
        // A different tonic, to make the swap meaningful.
        let tonality = Tonality {
            tonic: harmonium_key(2), // Sa on D
            ..bilawal_config().1
        };
        coach.send_command(Command::ConfigureSession { tuning, tonality });

        let events = drain_past_list_devices(&coach);
        assert!(
            no_state_changes(&events),
            "ConfigureSession while Running must not change session state",
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                CoachEvent::SessionConfigured { tonality: t, .. } if *t == tonality
            )),
            "reconfigure must emit SessionConfigured with the new tonic",
        );
        // Audio was opened exactly once (the start); configure didn't
        // touch the audio lifecycle.
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(
            coach.audio_info().is_some(),
            "still Running after reconfigure",
        );
        // Snapshot reflects the latest configure.
        assert_eq!(coach.music_info().unwrap().tonality, tonality);

        coach.send_command(Command::StopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Idle
                    }
                )
            })
        });

        // Sticky: the musical snapshot survives the audio stop, unlike
        // audio_info which clears to None.
        assert!(coach.audio_info().is_none(), "audio_info clears on stop",);
        assert_eq!(
            coach.music_info().unwrap().tonality,
            tonality,
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

        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
                    }
                )
            })
        });
        let info = coach
            .audio_info()
            .expect("Running should expose session info");
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 1);

        coach.send_command(Command::StopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Idle
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

        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::SessionError { .. }))
        });

        let kind = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::SessionError { kind, .. } => Some(*kind),
                _ => None,
            })
            .expect("SessionError");
        assert_eq!(kind, SessionErrorKind::UnsupportedConfig);

        // State should have reached Error.
        let saw_error = events.iter().any(|e| {
            matches!(
                e,
                CoachEvent::SessionStateChanged {
                    new_state: SessionState::Error
                }
            )
        });
        assert!(saw_error, "should have emitted SessionStateChanged(Error)");

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
        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
                    }
                )
            })
        });
        let opens_before = opens.load(Ordering::SeqCst);

        // Second Start: should be ignored. No new state event, no
        // new open call.
        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        // Give it a beat, then assert.
        thread::sleep(Duration::from_millis(50));
        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        assert!(
            buf.iter().all(|e| !matches!(
                e,
                CoachEvent::SessionStateChanged {
                    new_state: SessionState::Starting
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

        coach.send_command(Command::StartSession(AudioConfig {
            device_id: Some(DeviceId("does-not-exist".into())),
            sample_rate: None,
            buffer_frames: None,
        }));
        let events = poll_until(&coach, |evs| {
            evs.iter()
                .any(|e| matches!(e, CoachEvent::SessionError { .. }))
        });

        let (kind, _reason) = events
            .iter()
            .find_map(|e| match e {
                CoachEvent::SessionError { kind, reason } => Some((*kind, reason.clone())),
                _ => None,
            })
            .expect("SessionError");
        assert_eq!(kind, SessionErrorKind::DeviceUnavailable);
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

        coach.send_command(Command::StopSession);
        // Give the control plane a beat to (not) emit anything.
        thread::sleep(Duration::from_millis(50));

        let mut buf = Vec::new();
        coach.poll_events(&mut buf);
        let saw_state_change = buf
            .iter()
            .any(|e| matches!(e, CoachEvent::SessionStateChanged { .. }));
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
        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
                    }
                )
            })
        });
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        // Stop → Idle.
        coach.send_command(Command::StopSession);
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Idle
                    }
                )
            })
        });

        // Cycle 2: Start again — must reopen capture and reach Running.
        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let after = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
                    }
                )
            })
        });
        let states: Vec<SessionState> = after
            .iter()
            .filter_map(|e| match e {
                CoachEvent::SessionStateChanged { new_state } => Some(*new_state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![SessionState::Starting, SessionState::Running],
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

    #[test]
    fn zero_timeout_shutdown_respects_contract() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::Ok, Arc::clone(&opens));
        let coach = new(deps);

        // Reach Running so there's actual state to tear down.
        coach.send_command(Command::StartSession(AudioConfig {
            device_id: None,
            sample_rate: None,
            buffer_frames: None,
        }));
        let _ = poll_until(&coach, |evs| {
            evs.iter().any(|e| {
                matches!(
                    e,
                    CoachEvent::SessionStateChanged {
                        new_state: SessionState::Running
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
}
