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
use crate::inspect::InspectShared;
use crate::outbound::OutboundQueue;
use arc_swap::ArcSwap;
use domain_ports::app_coach::{
    AppCoachDeps, AudioConfig, AudioInfo, CoachEvent, Command, FeatureSnapshot, MusicInfo,
    SessionErrorKind, SessionState,
};
use domain_ports::audio_capture::{CaptureCallback, CaptureConfig, CaptureFrame, CaptureSession};
use domain_ports::audio_devices::{DeviceId, InputStream};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::Tuning;
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

pub(crate) struct FeaturePublishers {
    pub(crate) latest: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    pub(crate) history: rtrb::Producer<FeatureSnapshot>,
}

pub(crate) struct ControlPlane {
    deps: AppCoachDeps,
    outbound: Arc<Mutex<OutboundQueue>>,
    rx: mpsc::Receiver<Input>,
    feature_publisher: Arc<ArcSwap<Option<FeatureSnapshot>>>,
    /// Holds the negotiated `AudioInfo` while a session is `Running`,
    /// `None` otherwise. The control plane writes it *before* emitting
    /// `SessionStateChanged(Running)` and clears it *before* emitting
    /// the next transition out, so a head reacting to the state event
    /// observes coherent info.
    audio_info_publisher: Arc<ArcSwap<Option<AudioInfo>>>,
    /// The sticky snapshot face of [`Command::ConfigureSession`]: the
    /// current [`MusicInfo`] (tuning spec + tonality), `None` until the
    /// first configure. Written *before* emitting
    /// [`CoachEvent::SessionConfigured`] so a head reacting to the event
    /// reads coherent state. Never cleared by start/stop — the musical
    /// config is decoupled from the audio lifecycle.
    music_info_publisher: Arc<ArcSwap<Option<MusicInfo>>>,
    /// Shared state behind the [`EngineInspect`](domain_ports::engine_inspect::EngineInspect)
    /// port: selection slot + tap snapshot publisher + node-port list.
    /// Cloned and handed to each new data-plane worker.
    inspect: Arc<InspectShared>,

    state: SessionState,
    /// The musical frame of reference — the [`Scale`] the singer is in —
    /// set by [`Command::ConfigureSession`]. `None` until the head first
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
}

impl ControlPlane {
    pub(crate) fn new(
        deps: AppCoachDeps,
        outbound: Arc<Mutex<OutboundQueue>>,
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
            feature_publisher: feature_publishers.latest,
            audio_info_publisher,
            music_info_publisher,
            inspect,
            state: SessionState::Idle,
            session_model: None,
            capture: None,
            data_plane: None,
            feature_producer: Some(feature_publishers.history),
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
            let _ = dp.stop(&*self.deps.telemetry);
        }
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
            Input::FromHead(Command::ListDevices) => self.do_list_devices(),
            Input::FromHead(Command::ListScales) => self.do_list_scales(),
            Input::FromHead(Command::StartSession(cfg)) => self.do_start_session(cfg),
            Input::FromHead(Command::StopSession) => self.do_stop_session(),
            Input::FromHead(Command::ConfigureSession { scale }) => {
                self.do_configure_session(scale)
            }
        }
    }

    fn do_list_devices(&mut self) {
        let devices = self.deps.audio_devices.list_devices();
        self.push_event(CoachEvent::DevicesListed { devices });
    }

    fn do_list_scales(&mut self) {
        self.push_event(CoachEvent::ScalesListed {
            shapes: scales_for(self.session_model.as_ref().map(|s| s.tuning().len())),
        });
    }

    fn do_start_session(&mut self, cfg: AudioConfig) {
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
                self.fail(kind, reason);
                return;
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
                session_prefix: cfg.session_label.clone(),
            },
            &mut self.feature_producer,
        ) {
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
        } = startup;

        let callback = self.build_frame_callback(negotiated_cfg.channels, producer, dropped_for_cb);

        match self.deps.audio_capture.open(
            stream_info.handle.clone(),
            negotiated_cfg.clone(),
            callback,
        ) {
            Ok(session) => {
                self.capture = Some(session);
                self.data_plane = Some(data_plane);
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
                    device_id: cfg.device_id.clone(),
                    buffer_frames: negotiated_cfg.buffer_frames,
                })));
                self.transition(SessionState::Running);
            }
            Err(e) => {
                // Capture refused. The data plane is still spinning
                // (with no producer left, since `producer` moved into
                // the callback which we've now dropped). Stop it so
                // its worker thread joins before we surface the error.
                self.feature_producer = data_plane.stop(&*self.deps.telemetry);
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
                //
                // Clear audio_info *before* the transition so a head
                // reacting to Stopping observes `None`, not stale info.
                self.audio_info_publisher.store(Arc::new(None));
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

    /// Set the musical frame of reference: hold the [`Scale`] the head
    /// configured. Valid in any state; no [`SessionState`] change — the
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
        // `SessionConfigured` reads a coherent `music_info()`.
        self.music_info_publisher
            .store(Arc::new(Some(MusicInfo { scale })));
        self.push_event(CoachEvent::SessionConfigured { scale });
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
        self.feature_publisher.store(Arc::new(None));
        // Clear the inspect publishers so the head's debug pane sees
        // an empty node list + no taps until the next session starts.
        // The selection slot is intentionally preserved so the user's
        // pick survives a session restart.
        self.inspect.clear();
    }

    fn fail(&mut self, kind: SessionErrorKind, reason: String) {
        // Always reachable from Starting (most common); also from
        // Running if a sync path ever invokes this.
        // Clear audio_info before the transition so a head reacting
        // to Error observes `None`. No-op when called from Starting.
        self.audio_info_publisher.store(Arc::new(None));
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

/// Filter the scale catalogue by the tuning's slot count `n`.
///
/// `n = Some(slot_count)` — return every catalogue entry authored for an
/// `slot_count`-slot grid (its widths sum to `slot_count`), as a flat
/// [`ScaleIntervals`] mask.
///
/// `n = None` — no configured tuning yet, so the coach can't know which
/// octave division is active. Returns an empty `Vec` rather than guessing
/// 12: honest absence is preferable to silently assuming a 12-slot world
/// when the head has not yet sent `ConfigureSession`.
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
