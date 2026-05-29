//! AppCoach port: the singing-coach product's headless behaviour.
//!
//! The coach is the *product*, not a host. It owns a coaching session
//! (open a mic, run the pipeline, surface state changes) and is driven
//! from the outside via a flat command/event boundary. Hosts
//! (`coach-cli`, future `coach-mac`, `coach-watch`) wire the
//! peripheral adapters into [`AppCoachDeps`] and then talk to the
//! coach only through [`AppCoach`].
//!
//! # The boundary
//!
//! Three operations:
//!
//! - [`AppCoach::send_command`] — enqueue an intent (non-blocking).
//! - [`AppCoach::poll_events`] — drain the outbound event queue.
//! - [`AppCoach::shutdown`] — synchronously tear down with a timeout.
//!
//! The boundary is deliberately FFI-friendly. Phase 2 will add
//! `latest_pitch()` as a snapshot read for the firehose of pitch
//! readings; v1 does no pitch detection so that method does not exist
//! yet.
//!
//! See `docs/SPEC-AppCoach.md` for the full design context and the
//! deferred Phase 2 scope.
//!
//! # Sealed subsystem
//!
//! Once constructed, the coach is sealed: it owns its internal
//! threading and the lifecycle of any open audio stream. Hosts never
//! touch its internals and never re-enter the adapters they passed
//! in. This is the load-bearing constraint of the design.

use crate::audio_capture::AudioCapture;
use crate::audio_devices::{AudioDevices, DeviceId, InputDevice};
use crate::clock::Clock;
use crate::telemetry::Telemetry;
use std::sync::Arc;
use std::time::Duration;

/// Everything the coach needs to run, supplied by its host.
///
/// Hosts build this once after wiring peripheral adapters, hand it to
/// [`AppCoach::new`]-style constructors in the adapter crate, and
/// never touch the contained ports again.
pub struct AppCoachDeps {
    pub clock: Arc<dyn Clock>,
    pub telemetry: Arc<dyn Telemetry>,
    pub audio_devices: Arc<dyn AudioDevices>,
    pub audio_capture: Arc<dyn AudioCapture>,
    /// `CARGO_PKG_VERSION` of the *host* binary. Stamped on lifecycle
    /// telemetry so warehouse data attributes behaviour to the version
    /// the user is actually running, not the adapter's own version.
    pub host_version: &'static str,
}

// ---------------------------------------------------------------------
// Commands (head → coach)
// ---------------------------------------------------------------------

/// Intents the host sends to the coach. Async by construction: each
/// command is enqueued and processed by the coach's control plane;
/// the resulting state change surfaces as a [`CoachEvent`].
///
/// Commands in unexpected states (e.g. [`Command::StartSession`] while
/// already running) are silent no-ops — the coach logs at `Debug`
/// level via telemetry and discards. Heads do not need to track which
/// commands are legal in which states.
pub enum Command {
    /// Enumerate input devices. The coach replies with
    /// [`CoachEvent::DevicesListed`].
    ListDevices,

    /// Open the selected device and start a session.
    /// `Idle → Starting → Running` on success;
    /// `Idle → Starting → Error` on failure.
    StartSession(SessionConfig),

    /// Tear down the current session.
    /// `Running → Stopping → Idle`.
    ///
    /// Issued while `Starting` (before the data plane has acked
    /// `CaptureStarted`): cancels the in-flight start and transitions
    /// `Starting → Stopping → Idle` with no spurious `Running` flash.
    StopSession,
}

/// What the host wants to capture from.
pub struct SessionConfig {
    /// Identifies the device to open. `None` requests the system's
    /// multimedia-role default input.
    ///
    /// Heads pass through a [`DeviceId`] they obtained from a prior
    /// [`CoachEvent::DevicesListed`] or
    /// [`CoachEvent::DefaultInputChanged`] — they cannot fabricate one.
    pub device_id: Option<DeviceId>,

    /// `None` lets the coach pick (today: prefer 48000 if supported,
    /// else the lowest supported rate).
    pub sample_rate: Option<u32>,

    /// `None` lets the adapter pick. Smaller values lower IO callback
    /// latency at the cost of more frequent wakeups; sizes outside the
    /// device's supported range surface as
    /// [`CoachEvent::SessionError`] with
    /// [`SessionErrorKind::UnsupportedConfig`].
    pub buffer_frames: Option<u32>,
}

// ---------------------------------------------------------------------
// Events (coach → head)
// ---------------------------------------------------------------------

/// Notifications the coach pushes to its outbound queue. Heads drain
/// with [`AppCoach::poll_events`] on their own cadence (the queue is
/// bounded — see [`CoachEvent::EventsDropped`]).
pub enum CoachEvent {
    /// Reply to [`Command::ListDevices`].
    DevicesListed { devices: Vec<InputDevice> },

    /// The session state machine moved.
    SessionStateChanged { new_state: SessionState },

    /// Accompanies an `→ Error` transition with detail. `kind` lets
    /// heads branch / localize; `reason` is free-form for logs.
    SessionError {
        kind: SessionErrorKind,
        reason: String,
    },

    /// The OS-default input device changed (hotplug, user pref).
    /// Unsolicited; the host did not trigger it. Payload is the new
    /// default id; heads re-issue [`Command::ListDevices`] if they
    /// need the full updated list.
    ///
    /// **v1 note:** no device-listener port exists yet, so the coach
    /// never emits this in v1. The variant exists so heads can match
    /// exhaustively today and the Phase-2 emitter is non-breaking.
    DefaultInputChanged { new_default: Option<DeviceId> },

    /// The outbound queue overflowed and the coach dropped events.
    /// Surfaced as a single event per drop-burst (not one per dropped
    /// event). Heads SHOULD drain at ≥10Hz to avoid this.
    EventsDropped { count: u32 },
}

/// Where the session is in its lifecycle. Heads render UI off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    /// `StartSession` accepted; data plane round-trip in flight.
    Starting,
    Running,
    /// `StopSession` accepted; data plane teardown in flight.
    Stopping,
    /// Terminal error state. Recoverable only via `StopSession` (→
    /// `Idle`) followed by a new `StartSession`.
    Error,
}

/// Why the session entered [`SessionState::Error`]. Closed set so
/// heads can match exhaustively; an accompanying `reason: String`
/// carries adapter-specific detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorKind {
    /// Device gone / refused open / requested id is stale.
    DeviceUnavailable,
    /// Rate / channels / buffer rejected by the device.
    UnsupportedConfig,
    /// The audio stream failed mid-session (e.g. device unplugged).
    MidStreamFailure,
    Other,
    // `WorkerPanic` is Phase 2 — there is no worker in v1 to detect.
}

// ---------------------------------------------------------------------
// Pitch snapshot (Phase 2)
// ---------------------------------------------------------------------

/// Latest pitch estimate from the data plane.
///
/// Heads read this via [`AppCoach::latest_pitch`] on their own cadence
/// (UI frame rate, log timer, etc.). The data plane publishes a new
/// reading every `hop` samples worth of audio — typically ~85Hz at
/// 48kHz with a hop of 512 — so heads polling at 60Hz will see fresh
/// values most ticks and an occasional repeat.
///
/// Voicedness is encoded as [`f0_hz == 0.0`](Self::f0_hz): a voiced
/// frame reports a positive Hz value; an unvoiced frame reports
/// `0.0`. This matches the YIN node's sentinel and keeps the read
/// path branch-free.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchReading {
    /// Estimated fundamental frequency, in Hz. `0.0` means unvoiced
    /// (silence, breath, noise — not a frequency the detector
    /// trusts).
    pub f0_hz: f32,

    /// Wall-clock milliseconds (from the coach's [`Clock`]) at which
    /// this reading was published. Heads use this to detect staleness
    /// — if `t_ms` hasn't advanced between two polls, the data plane
    /// is stalled.
    pub t_ms: u64,
}

// ---------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------

/// Outcome of [`AppCoach::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownResult {
    /// Threads joined, audio stream closed, all clean.
    Clean,
    /// Timeout elapsed before the coach finished tearing down. The
    /// coach forcibly detaches whatever is still running and logs the
    /// leak via telemetry. Heads should treat this as a degraded exit.
    TimedOut,
    /// `shutdown` was called more than once. Idempotent.
    AlreadyShutDown,
}

// ---------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------

/// The coach's flat boundary.
///
/// **Not `Send`** to match the constraints of the underlying audio
/// streams ([`CaptureSession`](crate::audio_capture::CaptureSession)
/// is `!Send` on macOS). Heads construct, use, shut down, and drop on
/// the same thread.
pub trait AppCoach {
    /// Enqueue a command. Non-blocking. Commands in unexpected states
    /// are silently dropped (logged at `Debug`).
    fn send_command(&self, cmd: Command);

    /// Drain the outbound queue into `out`. Non-blocking; returns
    /// immediately with whatever events are pending.
    fn poll_events(&self, out: &mut Vec<CoachEvent>);

    /// Synchronously tear down, waiting up to `timeout` for clean
    /// teardown. On timeout the coach detaches whatever is still
    /// running and returns [`ShutdownResult::TimedOut`]. Subsequent
    /// calls return [`ShutdownResult::AlreadyShutDown`].
    ///
    /// `Drop` calls `shutdown(Duration::ZERO)` as a last-resort
    /// cleanup if the head forgot to shut down explicitly.
    fn shutdown(&self, timeout: Duration) -> ShutdownResult;

    /// Snapshot the latest pitch estimate from the data plane.
    ///
    /// Returns `None` before the data plane has published any
    /// reading (no session running, or session just started and the
    /// first window hasn't accumulated yet). Otherwise returns the
    /// most recent [`PitchReading`].
    ///
    /// Non-blocking, lock-free (the implementation uses an
    /// `ArcSwap`-style snapshot). Heads should poll this at their UI
    /// cadence — there is no event for pitch updates because the
    /// rate (~85Hz) would saturate the bounded event queue.
    ///
    /// **v1 returns `None` always** — the data plane lands in a
    /// subsequent PR. The method exists now so heads can match the
    /// final shape without further trait churn.
    fn latest_pitch(&self) -> Option<PitchReading>;
}
