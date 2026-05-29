//! The control plane: a single owned thread that drains [`Input`]s and
//! owns the session state machine.
//!
//! Every mutation of [`SessionState`] happens on this thread — there is
//! no `Mutex` around the state, no race, no ordering question. The
//! [`Input`] enum unifies head commands (delivered via
//! [`AppCoach::send_command`]) and (in Phase 2) internal acks from the
//! audio callback.

use crate::helpers::{classify_open_error, preferred_sample_rate};
use crate::outbound::OutboundQueue;
use domain_ports::app_coach::{
    AppCoachDeps, CoachEvent, Command, SessionConfig, SessionErrorKind, SessionState,
};
use domain_ports::audio_capture::{CaptureCallback, CaptureConfig, CaptureFrame, CaptureSession};
use domain_ports::audio_devices::{DeviceId, InputStream};
use domain_ports::{tel_debug, tel_info, tel_warn};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

    state: SessionState,
    /// Set when a [`SessionConfig`] has been accepted and the cpal
    /// stream is open. `None` in `Idle` / `Error`. Lives on this
    /// thread — `CaptureSession` is `!Send`.
    capture: Option<CaptureSession>,
}

impl ControlPlane {
    pub(crate) fn new(
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
