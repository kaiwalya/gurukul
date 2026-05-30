//! The control plane: a single owned thread that drains [`Input`]s and
//! owns the session state machine.
//!
//! Every mutation of [`SessionState`] happens on this thread — there is
//! no `Mutex` around the state, no race, no ordering question. The
//! [`Input`] enum unifies head commands (delivered via
//! [`AppCoach::send_command`]) and (in Phase 2) internal acks from the
//! audio callback.

use crate::data_plane::{push_samples, DataPlane, DataPlaneDeps};
use crate::helpers::{classify_open_error, preferred_sample_rate};
use crate::outbound::OutboundQueue;
use arc_swap::ArcSwap;
use domain_ports::app_coach::{
    AppCoachDeps, CoachEvent, Command, FeatureSnapshot, SessionConfig, SessionErrorKind,
    SessionState,
};
use domain_ports::audio_capture::{CaptureCallback, CaptureConfig, CaptureFrame, CaptureSession};
use domain_ports::audio_devices::{DeviceId, InputStream};
use domain_ports::{tel_debug, tel_info, tel_warn};
use rtrb::Consumer;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Slot the control plane uses to hand the head-side raw-audio
/// consumer to [`CoachImpl::drain_audio`]. Lives in an `Arc<Mutex<…>>`
/// so it can be shared without lifetime gymnastics; the mutex is
/// uncontended in practice — the control plane writes it on
/// session start / stop, the head reads it at the UI tick.
pub(crate) type HeadAudioSlot = Arc<Mutex<Option<Consumer<f32>>>>;

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
pub(crate) enum Input {
    FromHead(Command),
    Quit,
}

pub(crate) struct ControlPlane {
    deps: AppCoachDeps,
    outbound: Arc<Mutex<OutboundQueue>>,
    rx: mpsc::Receiver<Input>,
    feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    head_audio_slot: HeadAudioSlot,

    state: SessionState,
    /// Set when a [`SessionConfig`] has been accepted and the cpal
    /// stream is open. `None` in `Idle` / `Error`. Lives on this
    /// thread — `CaptureSession` is `!Send`.
    capture: Option<CaptureSession>,
    /// Set when a session is running. Owns the worker thread and
    /// the SPSC ring's worker-side consumer. The producer half lives
    /// inside the capture callback.
    data_plane: Option<DataPlane>,
}

impl ControlPlane {
    pub(crate) fn new(
        deps: AppCoachDeps,
        outbound: Arc<Mutex<OutboundQueue>>,
        rx: mpsc::Receiver<Input>,
        feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
        head_audio_slot: HeadAudioSlot,
    ) -> Self {
        Self {
            deps,
            outbound,
            rx,
            feature_publisher,
            head_audio_slot,
            state: SessionState::Idle,
            capture: None,
            data_plane: None,
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

        // Drop the open capture (if any) on this thread; this halts
        // the cpal RT callback so no more samples are pushed into the
        // ring. Then stop the worker — it sees its consumer go empty
        // / producer dropped and exits its loop on the next tick.
        if let Some(session) = self.capture.take() {
            drop(session);
        }
        if let Some(dp) = self.data_plane.take() {
            dp.stop(&*self.deps.telemetry);
        }
        // Clear any stale reading so a head polling after shutdown
        // sees `None` instead of the last-known f0.
        self.feature_publisher.store(Arc::new(None));
        *self.head_audio_slot.lock().unwrap() = None;
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

        // Spawn the data plane first so its ring producer is in hand
        // before cpal can fire the callback. If engine build / thread
        // spawn fails we surface as Other and skip opening the device.
        let startup = match DataPlane::start(DataPlaneDeps {
            sample_rate,
            feature_publisher: Arc::clone(&self.feature_publisher),
            clock: Arc::clone(&self.deps.clock),
            telemetry: Arc::clone(&self.deps.telemetry),
        }) {
            Ok(t) => t,
            Err(e) => {
                self.fail(SessionErrorKind::Other, e.to_string());
                return;
            }
        };
        let crate::data_plane::DataPlaneStartup {
            data_plane,
            producer,
            samples_dropped: dropped_for_cb,
            head_audio_consumer,
        } = startup;

        // Publish the head-side consumer so coach.drain_audio() finds
        // it. Cleared again in teardown_data_path on session stop.
        *self.head_audio_slot.lock().unwrap() = Some(head_audio_consumer);

        let callback = self.build_frame_callback(channels, producer, dropped_for_cb);

        match self
            .deps
            .audio_capture
            .open(stream_info.handle.clone(), capture_cfg, callback)
        {
            Ok(session) => {
                self.capture = Some(session);
                self.data_plane = Some(data_plane);
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
                // Capture refused. The data plane is still spinning
                // (with no producer left, since `producer` moved into
                // the callback which we've now dropped). Stop it so
                // its worker thread joins before we surface the error.
                data_plane.stop(&*self.deps.telemetry);
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
                self.teardown_data_path();
                self.transition(SessionState::Idle);
            }
            SessionState::Error => {
                // Idempotent cleanup.
                self.teardown_data_path();
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
            dp.stop(&*self.deps.telemetry);
        }
        // Clear the head-side raw-audio consumer. A late
        // coach.drain_audio() now sees `None` instead of a
        // disconnected ring; symmetric with the feature publisher.
        *self.head_audio_slot.lock().unwrap() = None;
        self.feature_publisher.store(Arc::new(None));
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
