//! adapter-app-coach: the canonical [`AppCoach`] implementation.
//!
//! # Architecture (v1)
//!
//! The implementation follows the two-plane shape from
//! `docs/SPEC-AppCoach.md` §7, with only the control plane realised in
//! v1 (no pitch worker, no ring, no `ArcSwap` snapshot).
//!
//! - **Control plane**: a single owned thread that drains an MPSC of
//!   [`Input`] values. Every session-state mutation happens here —
//!   no `Mutex` around the state, no race, no ordering question. The
//!   `Input` enum unifies head commands (delivered via
//!   [`AppCoach::send_command`]) and internal acks from the audio
//!   callback (errors arriving on the RT thread, sent back via a
//!   `Sender<Input>` clone).
//! - **Data plane** in v1 is just the open
//!   [`CaptureSession`](domain_ports::audio_capture::CaptureSession),
//!   held by the control-plane thread (it's `!Send` on macOS, so it
//!   cannot live anywhere else). The cpal callback runs on its own
//!   RT thread but only writes a `[DEBUG]` log line per frame for
//!   parity with today's `coach-cli capture` subcommand. Phase 2
//!   replaces the log with a ring write + pitch worker.
//!
//! # Outbound events
//!
//! Events the head consumes via [`AppCoach::poll_events`] live in a
//! bounded `VecDeque<CoachEvent>` behind a `Mutex`. The control plane
//! pushes; the head drains. On overflow the control plane drops the
//! *oldest* event(s) and coalesces a single
//! [`CoachEvent::EventsDropped`] when space frees up.
//!
//! # Shutdown
//!
//! `shutdown(timeout)` sends [`Input::Quit`], waits up to `timeout`
//! for the control-plane thread to join. On timeout the thread is
//! detached and the result is [`ShutdownResult::TimedOut`]. `Drop`
//! calls `shutdown(Duration::ZERO)` as belt-and-suspenders.

use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, CoachEvent, Command, SessionConfig, SessionErrorKind, SessionState,
    ShutdownResult,
};
use domain_ports::audio_capture::{
    CaptureCallback, CaptureConfig, CaptureError, CaptureFrame, CaptureSession,
};
use domain_ports::audio_devices::{DeviceId, InputStream, SampleRateSupport};
use domain_ports::{tel_debug, tel_info, tel_warn};
use std::collections::VecDeque;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
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
    let outbound = Arc::new(Mutex::new(OutboundQueue::new(OUTBOUND_QUEUE_CAP)));
    let (tx_cmd, rx_cmd) = mpsc::channel::<Input>();
    let outbound_for_thread = Arc::clone(&outbound);

    let control_thread = thread::Builder::new()
        .name("app-coach-control".into())
        .spawn(move || {
            ControlPlane::new(deps, outbound_for_thread, rx_cmd).run();
        })
        .expect("spawn control-plane thread");

    CoachImpl {
        tx_cmd: Mutex::new(Some(tx_cmd)),
        outbound,
        control_thread: Mutex::new(Some(control_thread)),
        shut_down: Mutex::new(false),
    }
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

/// Join the control-plane thread with a deadline. On timeout the
/// thread is detached (its `JoinHandle` is dropped), which on Unix
/// leaves it running — but the head will exit shortly anyway, so the
/// OS reaps. Caller has already taken the handle out of `self`.
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> ShutdownResult {
    if timeout.is_zero() {
        // Zero-timeout shortcut: try a non-blocking is_finished probe
        // a few times so cooperative quick teardowns still report
        // Clean, but don't sit and wait.
        if handle.is_finished() {
            let _ = handle.join();
            return ShutdownResult::Clean;
        }
        return ShutdownResult::TimedOut;
    }

    // Poll: cheap on a teardown path, and avoids dragging in
    // crossbeam_utils just for a join-with-timeout primitive. The
    // control plane sets is_finished as soon as it returns from run().
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if handle.is_finished() {
            let _ = handle.join();
            return ShutdownResult::Clean;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if handle.is_finished() {
        let _ = handle.join();
        ShutdownResult::Clean
    } else {
        // Detach: drop the handle without joining.
        drop(handle);
        ShutdownResult::TimedOut
    }
}

// ---------------------------------------------------------------------
// Internal: the unified input the control plane drains
// ---------------------------------------------------------------------

/// Everything the control plane processes. v1 sources:
///
/// - [`Input::FromHead`]: head commands arrived via `send_command`.
/// - [`Input::Quit`]: shutdown signal. Synthesised by
///   [`AppCoach::shutdown`] / `Drop`.
///
/// Phase 2 will add a `CaptureFailedMidStream` variant once the
/// audio-capture port grows an error-reporting channel out of the RT
/// callback. Today the cpal adapter has no way to surface mid-stream
/// errors, so the variant would be dead.
enum Input {
    FromHead(Command),
    Quit,
}

// ---------------------------------------------------------------------
// Internal: the bounded outbound queue
// ---------------------------------------------------------------------

struct OutboundQueue {
    cap: usize,
    inner: VecDeque<CoachEvent>,
    pending_dropped: u32,
}

impl OutboundQueue {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: VecDeque::with_capacity(cap),
            pending_dropped: 0,
        }
    }

    /// Push an event. If the queue is at capacity, drop the *oldest*
    /// event to make room and count it. The dropped count surfaces
    /// later as a coalesced [`CoachEvent::EventsDropped`] (see
    /// [`Self::flush_dropped_marker`]).
    fn push(&mut self, ev: CoachEvent) {
        if self.inner.len() >= self.cap {
            self.inner.pop_front();
            self.pending_dropped = self.pending_dropped.saturating_add(1);
        }
        self.inner.push_back(ev);
    }

    /// Drain into `out`. If we had pending drops since the last flush,
    /// emit a single [`CoachEvent::EventsDropped`] at the head of the
    /// drained sequence so the consumer learns about it before the
    /// subsequent events.
    fn drain_into(&mut self, out: &mut Vec<CoachEvent>) {
        if self.pending_dropped > 0 {
            out.push(CoachEvent::EventsDropped {
                count: self.pending_dropped,
            });
            self.pending_dropped = 0;
        }
        out.extend(self.inner.drain(..));
    }
}

// ---------------------------------------------------------------------
// Internal: the control plane
// ---------------------------------------------------------------------

struct ControlPlane {
    deps: AppCoachDeps,
    outbound: Arc<Mutex<OutboundQueue>>,
    rx: mpsc::Receiver<Input>,

    state: SessionState,
    /// Set when a [`SessionConfig`] has been accepted and the cpal
    /// stream is open. `None` in `Idle` / `Error`. Lives on this
    /// thread — `CaptureSession` is `!Send`.
    capture: Option<CaptureSession>,
}

impl ControlPlane {
    fn new(
        deps: AppCoachDeps,
        outbound: Arc<Mutex<OutboundQueue>>,
        rx: mpsc::Receiver<Input>,
    ) -> Self {
        Self {
            deps,
            outbound,
            rx,
            state: SessionState::Idle,
            capture: None,
        }
    }

    fn run(mut self) {
        tel_info!(
            &*self.deps.telemetry,
            "app-coach: control plane up",
            host_version = self.deps.host_version,
            t_ms = self.deps.clock.now_ms(),
        );

        loop {
            // Bounded wait so a slow stream of RT errors doesn't
            // starve out the chance to inspect state from a future
            // watchdog. Today the timeout is just hygiene.
            match self.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Input::Quit) => break,
                Ok(input) => self.apply(input),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // Drop the open capture (if any) on this thread.
        if let Some(session) = self.capture.take() {
            drop(session);
        }
        tel_info!(
            &*self.deps.telemetry,
            "app-coach: control plane down",
            t_ms = self.deps.clock.now_ms(),
        );
    }

    fn apply(&mut self, input: Input) {
        match input {
            Input::Quit => { /* handled in run() */ }
            Input::FromHead(Command::ListDevices) => self.do_list_devices(),
            Input::FromHead(Command::StartSession(cfg)) => self.do_start_session(cfg),
            Input::FromHead(Command::StopSession) => self.do_stop_session(),
        }
    }

    fn do_list_devices(&mut self) {
        let devices = self.deps.audio_devices.list_devices();
        self.push_event(CoachEvent::DevicesListed { devices });
    }

    fn do_start_session(&mut self, cfg: SessionConfig) {
        if self.state != SessionState::Idle {
            tel_debug!(
                &*self.deps.telemetry,
                "app-coach: StartSession ignored (state not Idle)",
                state = format!("{:?}", self.state),
            );
            return;
        }

        // → Starting
        self.transition(SessionState::Starting);

        // Pick the stream for the requested device id.
        let stream_info = match self.resolve_stream(cfg.device_id.as_ref()) {
            Some(s) => s,
            None => {
                self.fail(
                    SessionErrorKind::DeviceUnavailable,
                    "no matching device".to_string(),
                );
                return;
            }
        };

        let sample_rate = cfg
            .sample_rate
            .unwrap_or_else(|| preferred_sample_rate(&stream_info.sample_rates));
        let channels = stream_info.channels;
        let buffer_frames = cfg.buffer_frames.or(Some(sample_rate / 100));

        let capture_cfg = CaptureConfig {
            sample_rate,
            channels,
            buffer_frames,
        };

        let callback = self.build_frame_callback(channels);

        match self
            .deps
            .audio_capture
            .open(stream_info.handle.clone(), capture_cfg, callback)
        {
            Ok(session) => {
                self.capture = Some(session);
                tel_info!(
                    &*self.deps.telemetry,
                    "app-coach: capture started",
                    device = stream_info.name.clone(),
                    sample_rate = sample_rate,
                    channels = channels as u32,
                    buffer_frames = buffer_frames.unwrap_or(0),
                );
                self.transition(SessionState::Running);
            }
            Err(e) => {
                let (kind, reason) = classify_open_error(e);
                self.fail(kind, reason);
            }
        }
    }

    fn do_stop_session(&mut self) {
        match self.state {
            SessionState::Running | SessionState::Starting => {
                // Both "stop a running capture" and "cancel an
                // in-flight Starting" land here. In v1 Start is
                // synchronous on the control thread, so we never
                // actually see Starting at message-arrival time —
                // but the spec says Stop-during-Start cancels, so
                // we model it: if capture was opened, drop it.
                self.transition(SessionState::Stopping);
                if let Some(session) = self.capture.take() {
                    drop(session);
                }
                self.transition(SessionState::Idle);
            }
            SessionState::Error => {
                // Idempotent cleanup.
                if let Some(session) = self.capture.take() {
                    drop(session);
                }
                self.transition(SessionState::Idle);
            }
            SessionState::Idle | SessionState::Stopping => {
                tel_debug!(
                    &*self.deps.telemetry,
                    "app-coach: StopSession ignored (state already terminal)",
                    state = format!("{:?}", self.state),
                );
            }
        }
    }

    fn fail(&mut self, kind: SessionErrorKind, reason: String) {
        // Always reachable from Starting (most common); also from
        // Running if a sync path ever invokes this.
        self.transition(SessionState::Error);
        self.push_event(CoachEvent::SessionError {
            kind,
            reason: reason.clone(),
        });
        tel_warn!(
            &*self.deps.telemetry,
            "app-coach: session error",
            kind = format!("{kind:?}"),
            reason = reason,
        );
    }

    fn transition(&mut self, new_state: SessionState) {
        if self.state == new_state {
            return;
        }
        self.state = new_state;
        self.push_event(CoachEvent::SessionStateChanged { new_state });
    }

    fn push_event(&self, ev: CoachEvent) {
        self.outbound.lock().unwrap().push(ev);
    }

    fn resolve_stream(&self, want: Option<&DeviceId>) -> Option<InputStream> {
        match want {
            None => self.deps.audio_devices.default_input(),
            Some(id) => self
                .deps
                .audio_devices
                .list_devices()
                .into_iter()
                .find(|d| d.persistent_id.as_ref() == Some(id))
                .and_then(|mut d| d.streams.pop()),
        }
    }

    /// Build the per-frame callback that runs on cpal's RT thread.
    /// Logs at `[DEBUG]` for parity with today's `capture` subcommand.
    /// Owns clones of the telemetry sink and the self-sender (for
    /// reporting mid-stream errors — currently unused here, but the
    /// channel hop is the model future work will rely on).
    fn build_frame_callback(&self, _channels: u16) -> CaptureCallback {
        let telemetry = Arc::clone(&self.deps.telemetry);
        Box::new(move |frame: CaptureFrame<'_>| {
            let (min, max, sum_sq) = frame.samples.iter().fold(
                (f32::INFINITY, f32::NEG_INFINITY, 0.0_f64),
                |(mn, mx, ss), &s| (mn.min(s), mx.max(s), ss + (s as f64) * (s as f64)),
            );
            let vpp = max - min;
            let mid = (max + min) * 0.5;
            let rms = if frame.samples.is_empty() {
                0.0
            } else {
                (sum_sq / frame.samples.len() as f64).sqrt() as f32
            };
            tel_debug!(
                &*telemetry,
                "capture frame",
                t_ms = frame.t_ms,
                frames = frame.frames as u64,
                vpp = format!("{vpp:.4}"),
                mid = format!("{mid:+.4}"),
                rms = format!("{rms:.4}"),
            );
        })
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn classify_open_error(e: CaptureError) -> (SessionErrorKind, String) {
    match e {
        CaptureError::InvalidHandle => (SessionErrorKind::DeviceUnavailable, e.to_string()),
        CaptureError::DeviceUnavailable { .. } => {
            (SessionErrorKind::DeviceUnavailable, e.to_string())
        }
        CaptureError::UnsupportedConfig { .. } => {
            (SessionErrorKind::UnsupportedConfig, e.to_string())
        }
        CaptureError::Other(_) => (SessionErrorKind::Other, e.to_string()),
    }
}

/// Pick a sample rate to request from the stream. Prefer 48000 when
/// it falls in any reported range; else use the lowest range minimum;
/// else guess 48000 for `ProbeOnly`.
fn preferred_sample_rate(s: &SampleRateSupport) -> u32 {
    const PREFERRED: u32 = 48_000;
    match s {
        SampleRateSupport::List(rates) => {
            if rates.contains(&PREFERRED) {
                PREFERRED
            } else {
                rates.first().copied().unwrap_or(PREFERRED)
            }
        }
        SampleRateSupport::Ranges(ranges) => {
            for (lo, hi) in ranges {
                if (*lo..=*hi).contains(&PREFERRED) {
                    return PREFERRED;
                }
            }
            ranges.iter().map(|(lo, _)| *lo).min().unwrap_or(PREFERRED)
        }
        SampleRateSupport::ProbeOnly => PREFERRED,
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::audio_capture::AudioCapture;
    use domain_ports::audio_devices::{AudioDevices, InputDevice, StreamHandle, Transport};
    use domain_ports::clock::{Clock, TestClock};
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

        coach.send_command(Command::StartSession(SessionConfig {
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

    #[test]
    fn open_failure_lands_in_error_state_with_unsupported_config() {
        let opens = Arc::new(AtomicU32::new(0));
        let (deps, _tel) = deps_with(FakeOutcome::FailUnsupported, Arc::clone(&opens));
        let coach = new(deps);

        coach.send_command(Command::StartSession(SessionConfig {
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
        coach.send_command(Command::StartSession(SessionConfig {
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
        coach.send_command(Command::StartSession(SessionConfig {
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
}
