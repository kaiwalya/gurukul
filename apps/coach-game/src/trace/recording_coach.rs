//! Port-side recording decorator.
//!
//! [`RecordingCoach`] wraps the real `Box<dyn AppCoach>` and logs the
//! *outputs* of every read (`poll_events`, `latest_features`,
//! `drain_features`) and every sent `Command` into a shared [`TraceBuffer`].
//! `coach::drain_events` is the single reader of the handle and calls each
//! read once per frame, so buffer order aligns with frames naturally — the
//! Bevy side drains the buffer once per frame into `coach`/`cmd` records.
//!
//! Why a decorator and not a hook in `drain_events`: the drain stays
//! untouched (no `#[cfg]`, no trace-awareness), and the wrap happens at
//! construction (`spawn_coach`) so a non-recording build is byte-identical.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use domain_ports::app_coach::{
    AppCoach, AudioInfo, CoachEvent, Command, FeatureSnapshot, MusicInfo, ShutdownResult,
};

/// Shared per-frame scratch the decorator appends to and the Bevy drain
/// system empties. `!Send` (lives beside the `!Send` `Coach` handle on the
/// main thread), so `Rc<RefCell<…>>`, not `Arc<Mutex<…>>`.
///
/// The reads are recorded as the **port types** (`FeatureSnapshot`), not the
/// head's lifted [`Features`](crate::feature_types::Features): replay serves
/// these back verbatim so the real `f0_hz → PitchLog2` lift in `drain_events`
/// re-runs on identical input. Recording the lifted form would both drift on an
/// f32 round-trip and bypass the lift code replay exists to re-run. This is
/// the **port-types rule**: record upstream of everything you claim to replay.
#[derive(Default)]
pub struct TraceBuffer {
    /// Events `poll_events` handed back this frame.
    pub events: Vec<CoachEvent>,
    /// What `latest_features` returned this frame, as the raw port snapshot.
    pub latest: Option<FeatureSnapshot>,
    /// What `drain_features` returned this frame, in producer order.
    pub drained: Vec<FeatureSnapshot>,
    /// Commands sent this frame, in send order.
    pub commands: Vec<Command>,
}

impl TraceBuffer {
    pub fn take(&mut self) -> TraceBuffer {
        std::mem::take(self)
    }

    pub fn is_quiet(&self) -> bool {
        self.events.is_empty()
            && self.latest.is_none()
            && self.drained.is_empty()
            && self.commands.is_empty()
    }
}

/// Shared handle to the buffer. One clone lives inside [`RecordingCoach`], the
/// other is inserted as a Bevy `NonSend` resource for the drain system.
pub type TraceBufferHandle = Rc<RefCell<TraceBuffer>>;

/// Decorator over the real coach. Every read clones its output into the shared
/// buffer before returning it unchanged; `send_command` records the intent.
pub struct RecordingCoach {
    inner: Box<dyn AppCoach>,
    buffer: TraceBufferHandle,
}

impl RecordingCoach {
    pub fn new(inner: Box<dyn AppCoach>, buffer: TraceBufferHandle) -> Self {
        Self { inner, buffer }
    }
}

impl AppCoach for RecordingCoach {
    fn send_command(&self, cmd: Command) {
        // Record before forwarding: clone the command for the trace, send the
        // original on. `Command` is small (the heaviest variant is a `Scale`,
        // which is `Copy`); the `Vec<u32>`-free shape keeps this cheap.
        self.buffer.borrow_mut().commands.push(clone_command(&cmd));
        self.inner.send_command(cmd);
    }

    fn poll_events(&self, out: &mut Vec<CoachEvent>) {
        let before = out.len();
        self.inner.poll_events(out);
        // Record exactly what this poll produced (the tail just appended),
        // cloning so the caller still owns the originals.
        let mut buf = self.buffer.borrow_mut();
        for ev in &out[before..] {
            buf.events.push(clone_event(ev));
        }
    }

    fn shutdown(&self, timeout: Duration) -> ShutdownResult {
        self.inner.shutdown(timeout)
    }

    fn latest_features(&self) -> Option<FeatureSnapshot> {
        let snap = self.inner.latest_features();
        // Record the raw snapshot, not the lifted `Features` — replay re-runs
        // the lift on this exact value.
        self.buffer.borrow_mut().latest = snap;
        snap
    }

    fn drain_features(&self, out: &mut Vec<FeatureSnapshot>) {
        let before = out.len();
        self.inner.drain_features(out);
        let mut buf = self.buffer.borrow_mut();
        buf.drained.extend(out[before..].iter().copied());
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        self.inner.audio_info()
    }

    fn music_info(&self) -> Option<MusicInfo> {
        self.inner.music_info()
    }
}

/// `Command` and `CoachEvent` are not `Clone` (the port keeps them move-only —
/// a sent command is consumed, a polled event is owned by the drainer). The
/// trace needs a copy, so we reconstruct field-by-field. This is the one place
/// that pays for recording; it is a flat match, no allocation beyond what the
/// payloads already carry.
fn clone_command(cmd: &Command) -> Command {
    use domain_ports::app_coach::AudioConfig;
    match cmd {
        Command::AudioListDevices => Command::AudioListDevices,
        Command::MusicListScales => Command::MusicListScales,
        Command::AudioStartSession(cfg) => Command::AudioStartSession(AudioConfig {
            device_id: cfg.device_id.clone(),
            sample_rate: cfg.sample_rate,
            buffer_frames: cfg.buffer_frames,
            session_label: cfg.session_label.clone(),
        }),
        Command::AudioStopSession => Command::AudioStopSession,
        Command::MusicConfigureSession { scale } => {
            Command::MusicConfigureSession { scale: *scale }
        }
    }
}

fn clone_event(ev: &CoachEvent) -> CoachEvent {
    match ev {
        CoachEvent::AudioDevicesListed { devices } => CoachEvent::AudioDevicesListed {
            devices: devices.clone(),
        },
        CoachEvent::MusicScalesListed { shapes } => CoachEvent::MusicScalesListed {
            shapes: shapes.clone(),
        },
        CoachEvent::AudioSessionStateChanged { new_state } => {
            CoachEvent::AudioSessionStateChanged {
                new_state: *new_state,
            }
        }
        CoachEvent::MusicSessionConfigured { scale } => {
            CoachEvent::MusicSessionConfigured { scale: *scale }
        }
        CoachEvent::AudioSessionError { kind, reason } => CoachEvent::AudioSessionError {
            kind: *kind,
            reason: reason.clone(),
        },
        CoachEvent::AudioDefaultInputChanged { new_default } => {
            CoachEvent::AudioDefaultInputChanged {
                new_default: new_default.clone(),
            }
        }
        CoachEvent::EventsDropped { count } => CoachEvent::EventsDropped { count: *count },
    }
}
