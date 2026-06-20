//! The control plane: a single owned thread that drains [`Input`]s and
//! owns the session state machine.
//!
//! Every mutation of [`AudioSessionState`] happens on this thread — there is
//! no `Mutex` around the state, no race, no ordering question. The
//! [`Input`] enum unifies head commands (delivered via
//! [`AppCoach::send_command`]) and (in Phase 2) internal acks from the
//! audio callback.

use crate::data_plane::{push_samples, DataPlane, DataPlaneDeps};
use crate::helpers::{classify_lifecycle_event, classify_open_error, preferred_sample_rate};
use crate::inspect::InspectShared;
use crate::outbound::OutboundQueue;
use arc_swap::ArcSwap;
use domain_ports::app_coach::{
    AppCoachDeps, AudioConfig, AudioInfo, AudioSessionErrorKind, AudioSessionState, CoachEvent,
    Command, FeatureSnapshot, InterruptionPhase, MusicInfo,
};
use domain_ports::audio_capture::{
    CaptureCallback, CaptureConfig, CaptureFrame, CaptureSession, LifecycleEvent, LifecycleSink,
};
use domain_ports::audio_devices::{AudioDevices, DeviceId, InputStream};
use domain_ports::audio_driver::{AudioInitError, AudioInitStatus, AudioPermissionSink};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::Tuning;
use domain_ports::{tel_debug, tel_info, tel_warn};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// After this many *consecutive* failed reopens during recovery, give up
/// and go terminal (`Error(MidStreamFailure)`). Reset to zero on a clean
/// Running. Guards the reopen→fail→reopen spin both reviewers flagged.
const RETRY_BUDGET: usize = 3;

/// Route changes arrive in bursts while the OS settles a new route. Coalesce
/// every route change seen within this window into a single reconcile.
const ROUTE_DEBOUNCE_MS: u64 = 150;

/// Bounded wait for `InterruptionEnded` to arrive (Apple drops it in some
/// backgrounding cases). iOS says interruptions last 5–30s; 8s gives buffer
/// without wedging forever. On timeout we leave the session stopped — never
/// auto-resume.
const INTERRUPTION_TIMEOUT_MS: u64 = 8_000;

/// Upper bound on a single `recv_timeout` wait. With no pending deadline the
/// loop still wakes this often (hygiene, matching the prior fixed 500ms); with
/// a nearer deadline the wait shrinks to it.
const MAX_WAIT_MS: u64 = 500;

/// What the head asked for at start, retained for recovery. Recovery
/// re-runs the start sequence reading *live* device truth plus this stored
/// intent, so it knows whether a lost device should be re-selected (the
/// "default input" policy) or is terminal (a specific device).
#[derive(Clone)]
struct StartIntent {
    /// The original [`AudioConfig`] from the head (rate/buffer prefs, label).
    cfg: AudioConfig,
    /// The device policy distilled from `cfg.device_id`.
    device: DevicePolicy,
}

/// Whether the session targets the system default input or one specific
/// device. Drives the `DeviceUnavailable` verdict: a default can be
/// re-selected; a specific device that vanishes is terminal.
#[derive(Clone, PartialEq)]
enum DevicePolicy {
    Default,
    Specific(DeviceId),
}

/// The bring-up sequence failed. Carries the classified error so the caller
/// (start / reconcile) decides what to do next — `bring_up_capture` itself
/// does **not** transition to `Error`, because a recovery retry must stay in
/// `Starting` and only the *final* exhausted attempt becomes terminal.
struct BringUpError {
    kind: AudioSessionErrorKind,
    reason: String,
}

/// Everything the control plane processes. v1 sources:
///
/// - [`Input::FromHead`]: head commands arrived via `send_command`.
/// - [`Input::Quit`]: shutdown signal. Synthesised by
///   [`AppCoach::shutdown`] / `Drop`.
/// - [`Input::AudioPermissionResolved`]: fired by the `AudioPermissionSink`
///   callback on an arbitrary thread when the OS permission state changes.
///   Carries the generation that was current when the request was issued;
///   stale generations are dropped.
///
/// - [`Input::Lifecycle`]: a mid-stream [`LifecycleEvent`] marshalled onto
///   this thread by the `LifecycleSink` the control plane handed to `open()`.
///   Carries the generation that was current at `open()` time; a stale
///   generation (a late notification from an already-dropped session) is
///   dropped.
pub(crate) enum Input {
    FromHead(Command),
    /// OS permission callback arrived. `generation` must match the control
    /// plane's current generation or the event is dropped.
    AudioPermissionResolved {
        generation: u64,
    },
    /// A mid-stream lifecycle signal from the capture adapter. `generation`
    /// is the value frozen into the sink closure at `open()` time; it must
    /// match the control plane's current generation or the event is dropped
    /// (it came from a session we have since torn down).
    Lifecycle {
        event: LifecycleEvent,
        generation: u64,
    },
    Quit,
}

/// A pending time-based action the deadline-aware loop must service. Each
/// holds an absolute fire time in `clock.now_ms()` units. The loop computes
/// its per-iteration wait as the gap to the nearest of these, and fires any
/// that have come due in the timeout branch.
struct Deadlines {
    /// Set when an interruption begins; fires if `InterruptionEnded` never
    /// arrives. Firing leaves the session stopped (no auto-resume).
    interruption_timeout: Option<u64>,
    /// Set/extended on each `RouteChanged`; firing triggers one reconcile
    /// for the whole coalesced burst.
    route_debounce: Option<u64>,
}

impl Deadlines {
    fn new() -> Self {
        Self {
            interruption_timeout: None,
            route_debounce: None,
        }
    }

    /// The nearest pending fire time, if any.
    fn nearest(&self) -> Option<u64> {
        [self.interruption_timeout, self.route_debounce]
            .into_iter()
            .flatten()
            .min()
    }
}

pub(crate) struct FeaturePublishers {
    pub(crate) latest: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    pub(crate) history: rtrb::Producer<FeatureSnapshot>,
}

pub(crate) struct ControlPlane {
    deps: AppCoachDeps,
    outbound: Arc<Mutex<OutboundQueue>>,
    rx: mpsc::Receiver<Input>,
    /// Cloned sender for this control plane's own input channel. Used to
    /// build `AudioPermissionSink` closures that enqueue
    /// `Input::AudioPermissionResolved` from the OS callback thread.
    tx: Sender<Input>,
    feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    /// Holds the negotiated `AudioInfo` while a session is `Running`,
    /// `None` otherwise. The control plane writes it *before* emitting
    /// `AudioSessionStateChanged(Running)` and clears it *before* emitting
    /// the next transition out, so a head reacting to the state event
    /// observes coherent info.
    audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>>,
    /// The sticky snapshot face of [`Command::MusicConfigureSession`]: the
    /// current [`MusicInfo`] (tuning spec + tonality), `None` until the
    /// first configure. Written *before* emitting
    /// [`CoachEvent::MusicSessionConfigured`] so a head reacting to the event
    /// reads coherent state. Never cleared by start/stop — the musical
    /// config is decoupled from the audio lifecycle.
    music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>>,
    /// Shared state behind the [`EngineInspect`](domain_ports::engine_inspect::EngineInspect)
    /// port: selection slot + tap snapshot publisher + node-port list.
    /// Cloned and handed to each new data-plane worker.
    inspect: Arc<InspectShared>,

    state: AudioSessionState,
    /// Monotonically increasing generation counter. Bumped whenever a
    /// start is accepted and whenever a stop is accepted. Sinks built for
    /// `AudioPermissionRequest` capture the snapshot; a late
    /// `AudioPermissionResolved` carrying a mismatched generation is dropped.
    generation: u64,
    /// The musical frame of reference — the [`Scale`] the singer is in —
    /// set by [`Command::MusicConfigureSession`]. `None` until the head first
    /// configures. Decoupled from the audio lifecycle: it is *not*
    /// cleared by start/stop, only by shutdown. The data plane does not
    /// consume it yet (pitch *scoring* against the scale is a later
    /// phase); today it is held so the head's reads stay coherent and
    /// the seam exists for scoring to plug into.
    session_model: Option<Scale>,
    /// Set when an [`AudioConfig`] has been accepted and the cpal
    /// stream is open. `None` in `Idle` / `Error`. Lives on this
    /// thread — `CaptureSession` is `!Send`.
    capture: Option<CaptureSession>,
    /// Set when a session is running. Owns the worker thread and
    /// the SPSC ring's worker-side consumer. The producer half lives
    /// inside the capture callback.
    data_plane: Option<DataPlane>,
    /// Persistent producer for the ordered feature queue. Loaned to the
    /// active data-plane worker and returned when that worker joins.
    feature_producer: Option<rtrb::Producer<FeatureSnapshot>>,
    /// Live `AudioDevices` handle for the current session. Produced by
    /// `AudioDriver::new_devices()` at session start and dropped
    /// at session stop/shutdown. `None` when no session is active.
    live_devices: Option<Box<dyn AudioDevices>>,
    /// What the head asked for at the last successful start. Retained so
    /// recovery (reconcile) can re-run the start sequence reading live
    /// device truth plus this intent. `None` when no session is/has been
    /// running, cleared on a clean user stop.
    start_intent: Option<StartIntent>,
    /// Pending time-based actions (interruption timeout, route debounce).
    /// The `run()` loop computes its wait from the nearest of these.
    deadlines: Deadlines,
    /// Consecutive failed reopens during recovery. Reset to 0 on a clean
    /// Running; trips to `Error` at [`RETRY_BUDGET`].
    retry_count: usize,
}

impl ControlPlane {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        deps: AppCoachDeps,
        outbound: Arc<Mutex<OutboundQueue>>,
        tx: Sender<Input>,
        rx: mpsc::Receiver<Input>,
        feature_publishers: FeaturePublishers,
        audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>>,
        music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>>,
        inspect: Arc<InspectShared>,
    ) -> Self {
        Self {
            deps,
            outbound,
            rx,
            tx,
            feature_publisher: feature_publishers.latest,
            audio_info_publisher,
            music_info_publisher,
            inspect,
            state: AudioSessionState::Idle,
            generation: 0,
            session_model: None,
            capture: None,
            data_plane: None,
            feature_producer: Some(feature_publishers.history),
            live_devices: None,
            start_intent: None,
            deadlines: Deadlines::new(),
            retry_count: 0,
        }
    }

    pub(crate) fn run(mut self) {
        tel_info!(
            &*self.deps.telemetry,
            "app-coach: control plane up",
            host_version = self.deps.host_version,
            t_ms = self.deps.clock.now_ms(),
        );

        loop {
            // Deadline-aware wait: block only until the nearest pending
            // deadline (interruption timeout / route debounce), capped at
            // MAX_WAIT_MS as hygiene. Deadlines are tracked in
            // `clock.now_ms()` units so tests with a fake clock drive them
            // deterministically; the wall-clock `recv_timeout` only bounds
            // *when* we re-check, never *whether* a deadline is due (the
            // timeout branch re-reads the clock and fires what has come due).
            let wait = self.next_wait_ms();
            match self.rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(Input::Quit) => break,
                Ok(input) => {
                    self.apply(input);
                    // Service deadlines after every input too, not only on a
                    // bare timeout: a steady stream of inputs must not starve
                    // a due interruption-timeout / route-debounce.
                    self.service_deadlines();
                }
                Err(RecvTimeoutError::Timeout) => self.service_deadlines(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // Drop the open capture (if any) on this thread; this halts
        // the cpal RT callback so no more samples are pushed into the
        // ring. Then stop the worker — it sees its consumer go empty
        // / producer dropped and exits its loop on the next tick.
        if let Some(session) = self.capture.take() {
            drop(session);
        }
        if let Some(dp) = self.data_plane.take() {
            let _ = dp.stop(&*self.deps.telemetry);
        }
        // Drop the live devices handle (deactivates the OS audio session).
        self.live_devices = None;
        self.start_intent = None;
        self.deadlines = Deadlines::new();
        // Clear any stale reading so a head polling after shutdown
        // sees `None` instead of the last-known f0 / session info.
        self.feature_publisher.store(Arc::new(None));
        self.audio_info_publisher.store(Arc::new(None));
        self.inspect.clear();
        // The musical frame of reference is decoupled from the audio
        // lifecycle, so it survives start/stop — but shutdown is the end
        // of the coach, so drop it here (both the held model and the
        // sticky snapshot, so a post-shutdown poll sees `None`).
        self.session_model = None;
        self.music_info_publisher.store(Arc::new(None));
        tel_info!(
            &*self.deps.telemetry,
            "app-coach: control plane down",
            t_ms = self.deps.clock.now_ms(),
        );
    }

    fn apply(&mut self, input: Input) {
        match input {
            Input::Quit => { /* handled in run() */ }
            Input::FromHead(Command::AudioListDevices) => self.do_list_devices(),
            Input::FromHead(Command::MusicListScales) => self.do_list_scales(),
            Input::FromHead(Command::AudioStartSession(cfg)) => self.do_start_session(cfg),
            Input::FromHead(Command::AudioStopSession) => self.do_stop_session(),
            Input::FromHead(Command::MusicConfigureSession { scale }) => {
                self.do_configure_session(scale)
            }
            Input::FromHead(Command::AudioPermissionQuery) => {
                let status = self.deps.audio_driver.init_status();
                self.push_event(CoachEvent::AudioPermissionStatus { status });
            }
            Input::FromHead(Command::AudioPermissionRequest) => {
                let generation = self.generation;
                let tx = self.tx.clone();
                let sink = AudioPermissionSink(Box::new(move || {
                    // Enqueue on the control thread; if the channel is closed
                    // (shutdown in flight) the send fails silently.
                    let _ = tx.send(Input::AudioPermissionResolved { generation });
                }));
                self.deps.audio_driver.request(sink);
            }
            Input::AudioPermissionResolved { generation } => {
                if generation == self.generation {
                    let status = self.deps.audio_driver.init_status();
                    self.push_event(CoachEvent::AudioPermissionStatus { status });
                }
                // Stale generation — drop silently.
            }
            Input::Lifecycle { event, generation } => self.do_lifecycle(event, generation),
        }
    }

    /// Handle a mid-stream [`LifecycleEvent`]. The generation gate runs first:
    /// a late notification from an already-dropped session (carrying a stale
    /// generation) is dropped — it cannot drive a transition on a session the
    /// user has since stopped or that we have already torn down for recovery.
    fn do_lifecycle(&mut self, event: LifecycleEvent, generation: u64) {
        if generation != self.generation {
            tel_debug!(
                &*self.deps.telemetry,
                "app-coach: stale lifecycle event dropped",
                event = format!("{event:?}"),
                event_generation = generation,
                current_generation = self.generation,
            );
            return;
        }

        tel_debug!(
            &*self.deps.telemetry,
            "app-coach: lifecycle event",
            event = format!("{event:?}"),
            state = format!("{:?}", self.state),
        );

        match event {
            LifecycleEvent::Interrupted => self.on_interrupted(),
            LifecycleEvent::InterruptionEnded { should_resume } => {
                self.on_interruption_ended(should_resume)
            }
            LifecycleEvent::RouteChanged => self.arm_route_debounce(),
            LifecycleEvent::DeviceUnavailable => {
                // Context-sensitive: a lost *default* device re-selects (the
                // new default may already be live); a lost *specific* device
                // is terminal.
                match self.start_intent.as_ref().map(|i| i.device.clone()) {
                    Some(DevicePolicy::Default) => self.reconcile(),
                    _ => self.fail_lifecycle(&LifecycleEvent::DeviceUnavailable),
                }
            }
            // The rest are terminal in Phase 1.
            terminal => self.fail_lifecycle(&terminal),
        }
    }

    /// An interruption began: stop capture + clear `AudioInfo`
    /// (`Running → Stopping → Idle`, the same transition as a user stop —
    /// the coach state does not encode *why*; the head's Pause screen does),
    /// and arm the interruption timeout so a missing `InterruptionEnded`
    /// can't wedge us.
    fn on_interrupted(&mut self) {
        if self.state != AudioSessionState::Running {
            return;
        }
        // NOTE: we deliberately do NOT bump the generation here. `Interrupted`
        // and its paired `InterruptionEnded` arrive through the *same* sink
        // (same OS session, same generation); bumping would gate the paired
        // `InterruptionEnded` out as stale and the resume hint would be lost.
        // Stale-event safety still holds: this leaves the session Idle, and a
        // late *terminal* event is dropped by `fail_lifecycle`'s Idle guard.
        self.audio_info_publisher.store(Arc::new(None));
        self.transition(AudioSessionState::Stopping);
        self.teardown_data_path();
        self.transition(AudioSessionState::Idle);
        // Interruption ends the running capture context: drop any pending
        // route-debounce (it belonged to the now-dead session; otherwise it
        // could fire a reconcile on a later session). Then arm the
        // interruption timeout.
        self.deadlines.route_debounce = None;
        self.deadlines.interruption_timeout =
            Some(self.deps.clock.now_ms() + INTERRUPTION_TIMEOUT_MS);
        // Emit the interruption signal so the head can lock its Resume
        // action. This rides alongside the AudioSessionStateChanged events
        // already emitted above — the state changes drive the stop; this
        // event tells the head *why*.
        self.push_event(CoachEvent::AudioInterruption {
            phase: InterruptionPhase::Began,
        });
    }

    /// An interruption ended. Clear the interruption timeout. We never
    /// auto-resume — `should_resume` is the OS hint the head uses to unlock
    /// its Resume action (Decision 4), surfaced as an event for the head to
    /// observe. The coach itself stays Idle.
    fn on_interruption_ended(&mut self, should_resume: bool) {
        self.deadlines.interruption_timeout = None;
        tel_debug!(
            &*self.deps.telemetry,
            "app-coach: interruption ended",
            should_resume = should_resume,
        );
        // Emit the Ended signal so the head can unlock (or keep locked)
        // its Resume action per Decision 4.
        self.push_event(CoachEvent::AudioInterruption {
            phase: InterruptionPhase::Ended { should_resume },
        });
    }

    /// Set/extend the route-change debounce deadline. Coalesces a burst of
    /// route changes into one reconcile after the window settles.
    ///
    /// Only meaningful while `Running`: a route change during an interruption
    /// (Idle) or after a terminal error must not trigger a reconcile, which
    /// would auto-resume a session the user/OS has paused. Such an event is
    /// dropped — when the session resumes it reads live truth anyway.
    fn arm_route_debounce(&mut self) {
        if self.state != AudioSessionState::Running {
            return;
        }
        self.deadlines.route_debounce = Some(self.deps.clock.now_ms() + ROUTE_DEBOUNCE_MS);
    }

    /// Terminal verdict from a lifecycle event: classify, clear `AudioInfo`,
    /// tear down the data path, and go to `Error(kind)`.
    ///
    /// Idempotent against a duplicate / coalesced burst: if the session is
    /// already terminal (`Error`) or idle, a second terminal event is dropped
    /// — the first already tore everything down, so re-emitting the error
    /// would be noise.
    fn fail_lifecycle(&mut self, event: &LifecycleEvent) {
        if matches!(
            self.state,
            AudioSessionState::Error | AudioSessionState::Idle
        ) {
            return;
        }
        let Some((kind, reason)) = classify_lifecycle_event(event) else {
            // Recoverable events never reach here.
            return;
        };
        // Bump generation so the dropped session's late notifications are
        // invalidated, then tear down and go terminal.
        self.generation = self.generation.wrapping_add(1);
        self.teardown_data_path();
        self.start_intent = None;
        self.deadlines = Deadlines::new();
        self.fail(kind, reason);
    }

    fn do_list_devices(&mut self) {
        // Never prompt — listing is passive. If permission is not yet granted,
        // return an empty list rather than blocking or auto-prompting.
        if self.deps.audio_driver.init_status() != AudioInitStatus::Granted {
            self.push_event(CoachEvent::AudioDevicesListed { devices: vec![] });
            return;
        }
        let devices = match self.deps.audio_driver.new_devices() {
            Ok(d) => d.list_devices(),
            Err(_) => vec![],
        };
        self.push_event(CoachEvent::AudioDevicesListed { devices });
    }

    fn do_list_scales(&mut self) {
        self.push_event(CoachEvent::MusicScalesListed {
            shapes: scales_for(self.session_model.as_ref().map(|s| s.tuning().len())),
        });
    }

    fn do_start_session(&mut self, cfg: AudioConfig) {
        if self.state != AudioSessionState::Idle {
            tel_debug!(
                &*self.deps.telemetry,
                "app-coach: AudioStartSession ignored (state not Idle)",
                state = format!("{:?}", self.state),
            );
            return;
        }

        // Bump generation on every accepted start (whether it succeeds or fails)
        // to invalidate any in-flight permission sinks from before this command.
        self.generation = self.generation.wrapping_add(1);
        // Clear any session-scoped deadlines / retry budget left over from a
        // prior session (e.g. an interruption-paused session the user never
        // explicitly stopped) so they can't fire against this fresh start.
        self.deadlines = Deadlines::new();
        self.retry_count = 0;

        // Check permission. Do NOT auto-prompt — the head owns prompting via
        // `AudioPermissionRequest`.
        match self.deps.audio_driver.init_status() {
            AudioInitStatus::Granted => {
                // Fall through to start the session.
            }
            AudioInitStatus::Denied => {
                // Terminal — user must enable in OS Settings.
                self.transition(AudioSessionState::Starting);
                self.fail(
                    AudioSessionErrorKind::PermissionDenied,
                    "microphone permission denied".to_string(),
                );
                return;
            }
            AudioInitStatus::Undetermined => {
                // Not yet decided — the head should call AudioPermissionRequest
                // first. Do not auto-prompt.
                self.transition(AudioSessionState::Starting);
                self.fail(
                    AudioSessionErrorKind::PermissionDenied,
                    "microphone permission not yet determined; request permission first"
                        .to_string(),
                );
                return;
            }
        }

        // Distill the head's request into a retained start intent — recovery
        // reads it later (device policy decides terminal vs re-select).
        let intent = StartIntent {
            cfg: cfg.clone(),
            device: match cfg.device_id.clone() {
                None => DevicePolicy::Default,
                Some(id) => DevicePolicy::Specific(id),
            },
        };

        // → Starting
        self.transition(AudioSessionState::Starting);

        // Run the bring-up sequence. On success retain the intent + reset the
        // retry budget; on failure go terminal (a start failure is terminal —
        // recovery only applies once a session has been Running).
        match self.bring_up_capture(&intent) {
            Ok(()) => {
                self.start_intent = Some(intent);
                self.retry_count = 0;
            }
            Err(BringUpError { kind, reason }) => self.fail(kind, reason),
        }
    }

    /// The shared start sequence: activate session → enumerate → resolve the
    /// stream for the intent's device policy → negotiate → build the data
    /// plane → open the stream (handing it a generation-stamped lifecycle
    /// sink) → publish `AudioInfo` → `Running`.
    ///
    /// Used by both the head's start and recovery's reconcile. It reads
    /// **live** device truth every time, so on reconcile a "default" intent
    /// re-selects the current default. On any failure it transitions to
    /// `Error` (via [`fail`]) and returns `Err(BringUpError)`; the caller
    /// decides what to do next (start gives up; reconcile counts a retry).
    ///
    /// Precondition: state is `Starting` and the data path is already torn
    /// down (no live capture / data plane). On failure it returns the
    /// classified error **without** transitioning to `Error` — the caller
    /// owns that decision.
    fn bring_up_capture(&mut self, intent: &StartIntent) -> Result<(), BringUpError> {
        // Bring up the OS audio session and get a live devices handle.
        let devices = match self.deps.audio_driver.new_devices() {
            Ok(d) => d,
            Err(AudioInitError::ActivationFailed(msg)) => {
                return Err(BringUpError {
                    kind: AudioSessionErrorKind::Other,
                    reason: format!("session activation failed: {msg}"),
                });
            }
            Err(AudioInitError::Denied) => {
                return Err(BringUpError {
                    kind: AudioSessionErrorKind::PermissionDenied,
                    reason: "microphone permission denied".to_string(),
                });
            }
            Err(AudioInitError::Undetermined) => {
                return Err(BringUpError {
                    kind: AudioSessionErrorKind::PermissionDenied,
                    reason: "microphone permission not yet determined".to_string(),
                });
            }
        };

        // Pick the stream for the intent's device policy.
        let want = match &intent.device {
            DevicePolicy::Default => None,
            DevicePolicy::Specific(id) => Some(id),
        };
        let stream_info = match Self::resolve_stream_from(&*devices, want) {
            Some(s) => s,
            None => {
                return Err(BringUpError {
                    kind: AudioSessionErrorKind::DeviceUnavailable,
                    reason: "no matching device".to_string(),
                });
            }
        };

        let sample_rate = intent
            .cfg
            .sample_rate
            .unwrap_or_else(|| preferred_sample_rate(&stream_info.sample_rates));
        let channels = stream_info.channels;
        // Honor the caller's buffer request as-is. Do NOT fabricate a fixed
        // buffer when the caller passes None: the head passes None so each
        // platform's backend picks a buffer via cpal::BufferSize::Default. A
        // real iOS audio unit rejects BufferSize::Fixed (cpal reports it as a
        // misleading DeviceNotAvailable); macOS/sim tolerate it, which masked
        // this. The data plane re-chunks variable cpal frames into BLOCK_FRAMES
        // anyway, so a pinned capture buffer was never needed. Callers that
        // pass Some(n) explicitly (WAV-replay feeders, tests) still get Fixed.
        let buffer_frames = intent.cfg.buffer_frames;

        let requested_cfg = CaptureConfig {
            sample_rate,
            channels,
            buffer_frames,
        };

        // Negotiate the exact format before building the engine. A mismatch
        // surfaces here as UnsupportedConfig — before any data plane is
        // started, so there is nothing to clean up.
        let negotiated_cfg = match self
            .deps
            .audio_capture
            .negotiate(&stream_info.handle, &requested_cfg)
        {
            Ok(cfg) => cfg,
            Err(e) => {
                let (kind, reason) = classify_open_error(e);
                return Err(BringUpError { kind, reason });
            }
        };

        // Spawn the data plane first so its ring producer is in hand
        // before cpal can fire the callback. If engine build / thread
        // spawn fails we surface as Other and skip opening the device.
        // Build for the NEGOTIATED sample rate, not the requested guess.
        let startup = match DataPlane::start(
            DataPlaneDeps {
                sample_rate: negotiated_cfg.sample_rate,
                feature_publisher: Arc::clone(&self.feature_publisher),
                clock: Arc::clone(&self.deps.clock),
                telemetry: Arc::clone(&self.deps.telemetry),
                inspect: Arc::clone(&self.inspect),
                session_prefix: intent.cfg.session_label.clone(),
            },
            &mut self.feature_producer,
        ) {
            Ok(t) => t,
            Err(e) => {
                return Err(BringUpError {
                    kind: AudioSessionErrorKind::Other,
                    reason: e.to_string(),
                });
            }
        };
        let crate::data_plane::DataPlaneStartup {
            data_plane,
            producer,
            samples_dropped: dropped_for_cb,
        } = startup;

        let callback = self.build_frame_callback(negotiated_cfg.channels, producer, dropped_for_cb);
        let on_event = self.build_lifecycle_sink();

        match self.deps.audio_capture.open(
            stream_info.handle.clone(),
            negotiated_cfg.clone(),
            callback,
            on_event,
        ) {
            Ok(session) => {
                self.capture = Some(session);
                self.data_plane = Some(data_plane);
                // Store the live devices handle; dropping it would deactivate
                // the session while capture is still running.
                self.live_devices = Some(devices);
                tel_info!(
                    &*self.deps.telemetry,
                    "app-coach: capture started",
                    device = stream_info.name.clone(),
                    sample_rate = negotiated_cfg.sample_rate,
                    channels = negotiated_cfg.channels as u32,
                    buffer_frames = format!("{:?}", negotiated_cfg.buffer_frames),
                );
                // Publish negotiated session info *before* emitting the
                // Running transition so any head reacting to the event
                // sees Some(info) (not None or stale).
                self.audio_info_publisher.store(Arc::new(Some(AudioInfo {
                    sample_rate: negotiated_cfg.sample_rate,
                    channels: negotiated_cfg.channels,
                    device_id: intent.cfg.device_id.clone(),
                    buffer_frames: negotiated_cfg.buffer_frames,
                })));
                self.transition(AudioSessionState::Running);
                Ok(())
            }
            Err(e) => {
                // Capture refused. The data plane is still spinning
                // (with no producer left, since `producer` moved into
                // the callback which we've now dropped). Stop it so
                // its worker thread joins before we surface the error.
                self.feature_producer = data_plane.stop(&*self.deps.telemetry);
                // Drop devices (deactivate session) before failing.
                drop(devices);
                let (kind, reason) = classify_open_error(e);
                Err(BringUpError { kind, reason })
            }
        }
    }

    /// Build the [`LifecycleSink`] handed to `open()`. Mirrors the permission
    /// sink: clone `tx`, freeze the **current** generation into the closure,
    /// and enqueue `Input::Lifecycle` carrying that fixed generation. The sink
    /// only enqueues — never mutates state or touches the `!Send` session — so
    /// it is safe to fire from any OS-notification thread. A late event from a
    /// dropped session carries the stale generation and is dropped on receipt.
    fn build_lifecycle_sink(&self) -> LifecycleSink {
        let tx = self.tx.clone();
        let generation = self.generation;
        Box::new(move |event| {
            let _ = tx.send(Input::Lifecycle { event, generation });
        })
    }

    /// Recovery: re-run the start sequence reading live device truth + the
    /// stored intent. Drives `Running → Stopping → Starting → Running` (the
    /// head observes a recovery cycle it did not command).
    ///
    /// **Generation.** Each reopen bumps the generation before building the
    /// new sink (the same "stop also bumps" rule the spec applies to user
    /// stops): the dropped stream's late notifications carry the old
    /// generation and are dropped, so they cannot disturb the new stream.
    ///
    /// **Retries stay in `Starting`.** An intermediate failed reopen does
    /// **not** emit a public `Error` — only the final attempt, once the retry
    /// budget is spent, transitions to terminal `Error`. This keeps the
    /// closed-enum contract ("`Error` is terminal until `AudioStopSession`")
    /// intact: we never bounce out of `Error` on our own.
    fn reconcile(&mut self) {
        // Only reconcile a *live* session. From Idle (interruption pause) or
        // Error (terminal) a reconcile would auto-resume a session the user or
        // OS has paused — never do that. A retry loop below re-enters from
        // Starting, which is also fine.
        if !matches!(
            self.state,
            AudioSessionState::Running | AudioSessionState::Starting
        ) {
            return;
        }
        let Some(intent) = self.start_intent.clone() else {
            // Nothing to reconcile against — no session was ever running.
            return;
        };

        // Tear down the current (possibly half-dead) data path, then walk
        // Stopping → Starting. Bump the generation so the old session's sink
        // is invalidated.
        self.audio_info_publisher.store(Arc::new(None));
        self.transition(AudioSessionState::Stopping);
        self.teardown_data_path();
        self.generation = self.generation.wrapping_add(1);
        self.transition(AudioSessionState::Starting);

        // Retry in-place: each attempt reads live truth and bumps the
        // generation, so a transient failure may clear on the next try. Stay
        // in `Starting` between attempts — no public `Error` until exhaustion.
        loop {
            match self.bring_up_capture(&intent) {
                Ok(()) => {
                    self.retry_count = 0;
                    return;
                }
                Err(BringUpError { kind, reason }) => {
                    self.retry_count += 1;
                    if self.retry_count >= RETRY_BUDGET {
                        tel_warn!(
                            &*self.deps.telemetry,
                            "app-coach: recovery exhausted retry budget",
                            retry_count = self.retry_count as u64,
                        );
                        self.start_intent = None;
                        self.deadlines = Deadlines::new();
                        self.fail(kind, reason);
                        return;
                    }
                    // Bump the generation for the next reopen's sink, then
                    // retry. Stay in `Starting`.
                    self.generation = self.generation.wrapping_add(1);
                }
            }
        }
    }

    /// Per-iteration `recv_timeout` duration: the gap to the nearest pending
    /// deadline (clamped to ≥1ms so a just-passed deadline still wakes us),
    /// capped at [`MAX_WAIT_MS`]. With no deadline pending, the cap is used.
    fn next_wait_ms(&self) -> u64 {
        match self.deadlines.nearest() {
            None => MAX_WAIT_MS,
            Some(deadline) => {
                let now = self.deps.clock.now_ms();
                deadline.saturating_sub(now).clamp(1, MAX_WAIT_MS)
            }
        }
    }

    /// Fire whichever deadlines have come due (reading the clock fresh).
    /// Called from the loop's timeout branch.
    fn service_deadlines(&mut self) {
        let now = self.deps.clock.now_ms();

        if let Some(deadline) = self.deadlines.interruption_timeout {
            if now >= deadline {
                // InterruptionEnded never arrived. Leave the session stopped
                // (already Idle from on_interrupted) — never auto-resume.
                self.deadlines.interruption_timeout = None;
                tel_warn!(
                    &*self.deps.telemetry,
                    "app-coach: interruption timeout fired (no InterruptionEnded)",
                );
            }
        }

        if let Some(deadline) = self.deadlines.route_debounce {
            if now >= deadline {
                self.deadlines.route_debounce = None;
                // Reconcile only if a session is (or was) live; reconcile()
                // no-ops without a stored intent.
                self.reconcile();
            }
        }
    }

    fn do_stop_session(&mut self) {
        match self.state {
            AudioSessionState::Running | AudioSessionState::Starting => {
                // Both "stop a running capture" and "cancel an
                // in-flight Starting" land here. In v1 Start is
                // synchronous on the control thread, so we never
                // actually see Starting at message-arrival time —
                // but the spec says Stop-during-Start cancels, so
                // we model it: if capture was opened, drop it.
                //
                // Bump generation to invalidate any in-flight permission /
                // lifecycle sinks that referenced this session.
                self.generation = self.generation.wrapping_add(1);
                // A user stop ends the session intentionally — drop the
                // recovery intent, pending deadlines, and retry budget so no
                // reconcile fires after the user walked away.
                self.start_intent = None;
                self.deadlines = Deadlines::new();
                self.retry_count = 0;
                // Clear audio_info *before* the transition so a head
                // reacting to Stopping observes `None`, not stale info.
                self.audio_info_publisher.store(Arc::new(None));
                self.transition(AudioSessionState::Stopping);
                self.teardown_data_path();
                self.transition(AudioSessionState::Idle);
            }
            AudioSessionState::Error => {
                // Idempotent cleanup.
                self.start_intent = None;
                self.deadlines = Deadlines::new();
                self.retry_count = 0;
                self.teardown_data_path();
                self.transition(AudioSessionState::Idle);
            }
            AudioSessionState::Idle | AudioSessionState::Stopping => {
                tel_debug!(
                    &*self.deps.telemetry,
                    "app-coach: AudioStopSession ignored (state already terminal)",
                    state = format!("{:?}", self.state),
                );
            }
        }
    }

    /// Set the musical frame of reference: hold the [`Scale`] the head
    /// configured. Valid in any state; no [`AudioSessionState`] change — the
    /// musical lifecycle is decoupled from the audio one.
    ///
    /// The `Scale` arrives already placed (the head resolved both motions
    /// of the tonic), so there is nothing to build coach-side; the coach
    /// just stores it. Its degrees fit the tuning by construction — the
    /// head builds the mask against the same tuning's slot count.
    fn do_configure_session(&mut self, scale: Scale) {
        debug_assert!(
            scale
                .intervals()
                .degree_slots()
                .iter()
                .all(|&s| (s as usize) < scale.tuning().len()),
            "scale degrees {:?} exceed tuning slot count {}",
            scale.intervals().degree_slots(),
            scale.tuning().len(),
        );
        tel_debug!(
            &*self.deps.telemetry,
            "app-coach: session configured",
            slots = scale.tuning().len() as u32,
            degrees = scale.intervals().note_count() as u32,
        );
        self.session_model = Some(scale);
        // Publish the snapshot *before* the event so a head reacting to
        // `MusicSessionConfigured` reads a coherent `music_info()`.
        self.music_info_publisher
            .store(Arc::new(Some(MusicInfo { scale })));
        self.push_event(CoachEvent::MusicSessionConfigured { scale });
    }

    /// Drop the capture (stops RT callback, drops the ring producer),
    /// then join the data-plane worker and clear the pitch publisher.
    /// Ordering matters: capture must die before the worker is asked
    /// to stop, otherwise the worker may still be draining a final
    /// burst when we tear down its consumer.
    fn teardown_data_path(&mut self) {
        if let Some(session) = self.capture.take() {
            drop(session);
        }
        if let Some(dp) = self.data_plane.take() {
            self.feature_producer = dp.stop(&*self.deps.telemetry);
        }
        // Drop the live devices handle (deactivates the OS audio session).
        self.live_devices = None;
        self.feature_publisher.store(Arc::new(None));
        // Clear the inspect publishers so the head's debug pane sees
        // an empty node list + no taps until the next session starts.
        // The selection slot is intentionally preserved so the user's
        // pick survives a session restart.
        self.inspect.clear();
    }

    fn fail(&mut self, kind: AudioSessionErrorKind, reason: String) {
        // Always reachable from Starting (most common); also from
        // Running if a sync path ever invokes this.
        // Clear audio_info before the transition so a head reacting
        // to Error observes `None`. No-op when called from Starting.
        self.audio_info_publisher.store(Arc::new(None));
        self.transition(AudioSessionState::Error);
        self.push_event(CoachEvent::AudioSessionError {
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

    fn transition(&mut self, new_state: AudioSessionState) {
        if self.state == new_state {
            return;
        }
        self.state = new_state;
        self.push_event(CoachEvent::AudioSessionStateChanged { new_state });
    }

    fn push_event(&self, ev: CoachEvent) {
        self.outbound.lock().unwrap().push(ev);
    }

    fn resolve_stream_from(
        devices: &dyn AudioDevices,
        want: Option<&DeviceId>,
    ) -> Option<InputStream> {
        match want {
            None => devices.default_input(),
            Some(id) => devices
                .list_devices()
                .into_iter()
                .find(|d| d.persistent_id.as_ref() == Some(id))
                .and_then(|mut d| d.streams.pop()),
        }
    }

    /// Build the per-frame callback that runs on cpal's RT thread.
    /// Pushes the (mono) samples into the SPSC ring for the data-plane
    /// worker to consume. Multi-channel input is downmixed to mono by
    /// averaging interleaved channels — the engine expects mono.
    ///
    /// Realtime-safe: `push_samples` only touches lock-free atomics,
    /// the downmix is stack-only, the `Vec` capacity is reused across
    /// callbacks (allocated lazily on the first frame).
    fn build_frame_callback(
        &self,
        channels: u16,
        mut producer: rtrb::Producer<f32>,
        samples_dropped: Arc<std::sync::atomic::AtomicU64>,
    ) -> CaptureCallback {
        // Pre-sized scratch for downmix. Sized at first callback (cpal
        // doesn't tell us the actual buffer size up front). Capacity
        // is reused thereafter so the RT path stays alloc-free in
        // steady state.
        let mut mono_scratch: Vec<f32> = Vec::new();
        let channels = channels.max(1) as usize;
        Box::new(move |frame: CaptureFrame<'_>| {
            if channels == 1 {
                push_samples(&mut producer, frame.samples, &samples_dropped);
            } else {
                let frames = frame.frames;
                if mono_scratch.capacity() < frames {
                    mono_scratch.reserve(frames - mono_scratch.capacity());
                }
                mono_scratch.clear();
                let inv = 1.0_f32 / channels as f32;
                for f in 0..frames {
                    let base = f * channels;
                    let mut sum = 0.0_f32;
                    for c in 0..channels {
                        sum += frame.samples[base + c];
                    }
                    mono_scratch.push(sum * inv);
                }
                push_samples(&mut producer, &mono_scratch, &samples_dropped);
            }
        })
    }
}

/// Filter the scale catalogue by the tuning's slot count `n`.
///
/// `n = Some(slot_count)` — return every catalogue entry authored for an
/// `slot_count`-slot grid (its widths sum to `slot_count`), as a flat
/// [`ScaleIntervals`] mask.
///
/// `n = None` — no configured tuning yet, so the coach can't know which
/// octave division is active. Returns an empty `Vec` rather than guessing
/// 12: honest absence is preferable to silently assuming a 12-slot world
/// when the head has not yet sent `MusicConfigureSession`.
pub(crate) fn scales_for(n: Option<usize>) -> Vec<ScaleIntervals> {
    let Some(slot_count) = n else {
        return Vec::new();
    };
    scale_catalogue()
        .into_iter()
        .filter(|(_, grid)| *grid == slot_count as u32)
        .map(|(iv, _)| iv)
        .collect()
}

/// The built-in scale catalogue — 16 entries: 15 on the 12-slot grid
/// (the ten Hindustani thaats, Kirwani/harmonic-minor, two common
/// pentatonics, Locrian, and the 12-note chromatic) plus one on the
/// 22-shruti grid (Bilawal). Each entry is authored as tooth-widths —
/// semitones on the 12-grid, shrutis on the 22-grid — and paired with the
/// grid it closes (the widths' sum) so [`scales_for`] can filter by the
/// active tuning's slot count. Author-only labels are code comments;
/// [`ScaleIntervals`] has no name field — vocabulary is the deferred
/// note-system axis.
fn scale_catalogue() -> Vec<(ScaleIntervals, u32)> {
    // (tooth-widths, the grid N they close). from_widths drops the closing
    // width, so we keep N alongside to filter on the active tuning.
    const ENTRIES: &[&[u32]] = &[
        &[2, 2, 1, 2, 2, 2, 1], // Bilawal (= Major)
        &[2, 2, 1, 2, 2, 1, 2], // Khamaj
        &[2, 1, 2, 2, 2, 1, 2], // Kafi
        &[2, 1, 2, 2, 1, 2, 2], // Asavari
        &[1, 2, 2, 2, 1, 2, 2], // Bhairavi
        &[1, 3, 1, 2, 1, 3, 1], // Bhairav
        &[2, 2, 2, 1, 2, 2, 1], // Kalyan (= Lydian)
        &[1, 3, 2, 2, 1, 2, 1], // Marwa
        &[1, 3, 2, 1, 1, 3, 1], // Purvi
        &[1, 2, 3, 1, 1, 3, 1], // Todi
        &[1, 2, 2, 1, 2, 2, 2], // Locrian
        &[2, 1, 2, 2, 1, 3, 1], // Kirwani (= harmonic minor: komal Ga & Dha, shuddh Ni)
        &[2, 2, 3, 2, 3],       // Major pentatonic
        &[3, 2, 2, 3, 2],       // Minor pentatonic
        &[1; 12],               // Chromatic (all 12 slots lit)
        // Bilawal in shrutis (22-slot grid) — same swaras as 12-TET Bilawal.
        &[3, 2, 4, 4, 3, 2, 4],
    ];
    ENTRIES
        .iter()
        .map(|w| (ScaleIntervals::from_widths(w), w.iter().sum()))
        .collect()
}

#[cfg(test)]
mod catalogue_tests {
    use super::*;
    use domain_ports::tuning::{TuningKind, ORIGIN};

    #[test]
    fn catalogue_has_16_shapes() {
        assert_eq!(scale_catalogue().len(), 16);
    }

    #[test]
    fn catalogue_shapes_fit_their_respective_octave_divisions() {
        let cat = scale_catalogue();
        let count_12 = cat.iter().filter(|(_, n)| *n == 12).count();
        let count_22 = cat.iter().filter(|(_, n)| *n == 22).count();
        for (i, (_, n)) in cat.iter().enumerate() {
            assert!(
                *n == 12 || *n == 22,
                "entry {i} grid {n} is neither 12 nor 22"
            );
        }
        assert_eq!(count_12, 15, "expected 15 shapes on the 12-slot grid");
        assert_eq!(count_22, 1, "expected exactly 1 shape on the 22-slot grid");
    }

    #[test]
    fn catalogue_shapes_are_all_distinct() {
        // Distinct within a grid: two different grids may share a mask
        // (Bilawal's 12-mask differs from its 22-mask, but in general the
        // (mask, grid) pair is the identity).
        let cat = scale_catalogue();
        for i in 0..cat.len() {
            for j in (i + 1)..cat.len() {
                assert_ne!(cat[i], cat[j], "entries {i} and {j} are identical");
            }
        }
    }

    /// The slot count `n` a kind yields, via its intervals.
    fn n_of(kind: TuningKind) -> usize {
        kind.intervals().len()
    }

    #[test]
    fn scales_for_12_returns_15_shapes() {
        let shapes = scales_for(Some(12));
        assert_eq!(
            shapes.len(),
            15,
            "12-slot tuning must yield 15 catalogue shapes"
        );
    }

    #[test]
    fn scales_for_22_returns_1_shape() {
        let shapes = scales_for(Some(22));
        assert_eq!(
            shapes.len(),
            1,
            "22-slot tuning must yield exactly 1 catalogue shape"
        );
    }

    #[test]
    fn scales_for_none_returns_empty() {
        // Without a configured tuning the coach doesn't know which octave
        // division is active, so it returns nothing rather than guessing 12.
        let shapes = scales_for(None);
        assert!(shapes.is_empty(), "no configured tuning → empty scale list");
    }

    #[test]
    fn scales_for_reads_n_from_tuning() {
        // The kind → slot-count → scales_for path works for both grids.
        assert_eq!(scales_for(Some(n_of(TuningKind::TwelveTet))).len(), 15);
        assert_eq!(scales_for(Some(n_of(TuningKind::TwentyTwoShruti))).len(), 1);
        // Sanity: ORIGIN is reachable (keeps the import meaningful for the
        // resolve-based capture path the head exercises).
        let _ = ORIGIN;
    }
}
