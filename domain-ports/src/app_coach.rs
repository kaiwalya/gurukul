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
//! Plus two reads for the firehose of per-hop feature values (pitch,
//! onset, breath, vibrato), kept out of the event queue because their
//! ~85Hz rate would saturate it:
//!
//! - [`AppCoach::latest_features`] — the most recent snapshot, for
//!   instantaneous UI such as the note dial.
//! - [`AppCoach::drain_features`] — every retained hop in producer
//!   order, for history UI such as the scrolling time graph.
//!
//! The boundary is deliberately FFI-friendly.
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
use crate::scale::{Scale, ScaleIntervals};
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
/// Commands in unexpected states (e.g. [`Command::AudioStartSession`] while
/// already running) are silent no-ops — the coach logs at `Debug`
/// level via telemetry and discards. Heads do not need to track which
/// commands are legal in which states.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Command {
    /// Enumerate input devices. The coach replies with
    /// [`CoachEvent::AudioDevicesListed`].
    AudioListDevices,

    /// Enumerate the built-in scale shapes the coach can coach against.
    /// The coach replies with [`CoachEvent::MusicScalesListed`].
    ///
    /// The response is a flat catalogue of [`ScaleIntervals`] — tooth
    /// patterns only, no names. Names are the deferred note-system axis
    /// (see `docs/MUSIC_MODEL.md`); the coach is vocabulary-free.
    MusicListScales,

    /// Open the selected device and start a session.
    /// `Idle → Starting → Running` on success;
    /// `Idle → Starting → Error` on failure.
    AudioStartSession(AudioConfig),

    /// Tear down the current session.
    /// `Running → Stopping → Idle`.
    ///
    /// Issued while `Starting` (before the data plane has acked
    /// `CaptureStarted`): cancels the in-flight start and transitions
    /// `Starting → Stopping → Idle` with no spurious `Running` flash.
    AudioStopSession,

    /// Set the *musical* frame of reference: the [`Scale`] the singer is
    /// in — a tooth pattern dropped onto a concrete tuning at a concrete
    /// register, both motions of the tonic resolved. The coach holds it as
    /// the reference for judging pitch.
    ///
    /// The whole frame is one flat [`Scale`] value: it owns the
    /// [`ScaleIntervals`] (which grooves are degrees), the rotated tuning
    /// (which groove is Sa, and the reference-pitch rotation under it), and
    /// the octave (Sa's register). There is no separate tuning/tonality
    /// split — the geometry layer folds both into `Scale`.
    ///
    /// **Decoupled from the audio lifecycle.** Valid in *any* state and
    /// causes **no** [`AudioSessionState`] change: the musical lifecycle
    /// (configure) is independent of the audio lifecycle (start/stop).
    /// Reconfiguring mid-session just swaps the frame of reference.
    /// Heads may configure before, after, or without ever starting
    /// audio.
    ///
    /// **Published as state.** Each configure updates the sticky
    /// [`MusicInfo`] snapshot ([`AppCoach::music_info`]) *and* emits a
    /// [`CoachEvent::MusicSessionConfigured`] carrying the new config — the
    /// snapshot/event pair of one transition (snapshot written first).
    /// A head can read current state via the snapshot or fold the event
    /// stream to reconstruct it; the two never drift.
    MusicConfigureSession { scale: Scale },
}

/// Negotiated parameters of the currently-running session.
///
/// `AudioConfig` is what the head *asked for*; `AudioInfo` is what
/// the device actually agreed to and what the data plane is feeding the
/// engine right now. Heads use it for any widget that's sample-rate-
/// dependent (oscilloscopes, envelopes, downsampling math).
///
/// **Lifecycle invariant:** [`AppCoach::audio_info`] returns
/// `Some(_)` if and only if the most recent
/// [`CoachEvent::AudioSessionStateChanged`] reported
/// [`AudioSessionState::Running`]. It is `None` in `Idle`, `Starting`,
/// `Stopping`, and `Error` — including before the `Running` event has
/// landed. The adapter publishes a fresh `AudioInfo` *before*
/// emitting the `Running` transition and clears it *before* emitting
/// the next transition, so a head reacting to the event will see
/// coherent state.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AudioInfo {
    /// Negotiated capture sample rate, in Hz.
    pub sample_rate: u32,
    /// Negotiated channel count.
    pub channels: u16,
    /// The device the session is reading from. `None` when the session
    /// opened the OS default and no persistent id is available.
    pub device_id: Option<DeviceId>,
    /// Negotiated capture buffer size, in frames. `None` when the
    /// adapter cannot know the actual buffer size before frames
    /// arrive (e.g. cpal with `BufferSize::Default`). Heads that
    /// need a concrete value must wait for the first callback.
    pub buffer_frames: Option<u32>,
}

/// What the host wants to capture from — the *audio* parameters of a
/// session (device + stream format). Distinct from the *musical*
/// configuration of a session (the [`Scale`]), which is carried by
/// [`Command::MusicConfigureSession`] and is decoupled from the audio
/// lifecycle.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AudioConfig {
    /// Identifies the device to open. `None` requests the system's
    /// multimedia-role default input.
    ///
    /// Heads pass through a [`DeviceId`] they obtained from a prior
    /// [`CoachEvent::AudioDevicesListed`] or
    /// [`CoachEvent::AudioDefaultInputChanged`] — they cannot fabricate one.
    pub device_id: Option<DeviceId>,

    /// `None` lets the coach pick (today: prefer 48000 if supported,
    /// else the lowest supported rate).
    pub sample_rate: Option<u32>,

    /// `None` lets the adapter pick. Smaller values lower IO callback
    /// latency at the cost of more frequent wakeups; sizes outside the
    /// device's supported range surface as
    /// [`CoachEvent::AudioSessionError`] with
    /// [`AudioSessionErrorKind::UnsupportedConfig`].
    pub buffer_frames: Option<u32>,

    /// When `Some`, the engine records this session's input audio + per-hop
    /// features + manifest under this path prefix (the recorder appends
    /// `.wav` / `.features.jsonl` / `.manifest.json`).
    /// The HEAD owns naming and uniqueness — for repeated sessions in one
    /// run it must supply a FRESH prefix each `AudioStartSession`. `None`
    /// disables recording.
    pub session_label: Option<std::path::PathBuf>,
}

/// The coach's current *musical* frame of reference — the snapshot
/// face of [`Command::MusicConfigureSession`]. Carries the configured
/// [`Scale`] directly.
///
/// This is the materialized read-cache of the musical config: a head
/// that just wants "what scale is the coach holding right now?" reads
/// [`AppCoach::music_info`]; a head doing event-sourcing folds the
/// [`CoachEvent::MusicSessionConfigured`] stream to the same value. Both are
/// written in the same place (the configure handler), snapshot before
/// event, so they cannot drift.
///
/// **Lifecycle:** `None` only before the first `MusicConfigureSession`.
/// Once set it is **sticky** — it persists across start/stop/error,
/// because the musical configuration is decoupled from the audio
/// lifecycle (unlike [`AudioInfo`], which clears on stop). Each
/// configure overwrites it last-write-wins.
///
/// Flat and `Copy` ([`Scale`] is itself flat), so the snapshot stays
/// FFI-friendly: the dial reads ticks, needle, and lit degrees straight
/// off the `Scale` with no rebuild.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MusicInfo {
    pub scale: Scale,
}

// ---------------------------------------------------------------------
// Events (coach → head)
// ---------------------------------------------------------------------

/// Notifications the coach pushes to its outbound queue. Heads drain
/// with [`AppCoach::poll_events`] on their own cadence (the queue is
/// bounded — see [`CoachEvent::EventsDropped`]).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CoachEvent {
    /// Reply to [`Command::AudioListDevices`].
    AudioDevicesListed { devices: Vec<InputDevice> },

    /// Reply to [`Command::MusicListScales`]. Carries the full built-in
    /// catalogue of [`ScaleIntervals`] — tooth patterns only, no names.
    /// Names are the deferred note-system axis (see
    /// `docs/MUSIC_MODEL.md`); the catalogue here is vocabulary-free
    /// and stable across any note-system choice.
    MusicScalesListed { shapes: Vec<ScaleIntervals> },

    /// The session state machine moved.
    AudioSessionStateChanged { new_state: AudioSessionState },

    /// The *musical* frame of reference was (re)configured via
    /// [`Command::MusicConfigureSession`]. Carries the full new
    /// configuration so a head can update from the payload alone —
    /// this is the log entry whose fold reconstructs
    /// [`AppCoach::music_info`]. Emitted on *every* configure (not just
    /// the first), independent of the audio lifecycle.
    MusicSessionConfigured { scale: Scale },

    /// Accompanies an `→ Error` transition with detail. `kind` lets
    /// heads branch / localize; `reason` is free-form for logs.
    AudioSessionError {
        kind: AudioSessionErrorKind,
        reason: String,
    },

    /// The OS-default input device changed (hotplug, user pref).
    /// Unsolicited; the host did not trigger it. Payload is the new
    /// default id; heads re-issue [`Command::AudioListDevices`] if they
    /// need the full updated list.
    ///
    /// **v1 note:** no device-listener port exists yet, so the coach
    /// never emits this in v1. The variant exists so heads can match
    /// exhaustively today and the Phase-2 emitter is non-breaking.
    AudioDefaultInputChanged { new_default: Option<DeviceId> },

    /// The outbound queue overflowed and the coach dropped events.
    /// Surfaced as a single event per drop-burst (not one per dropped
    /// event). Heads SHOULD drain at ≥10Hz to avoid this.
    EventsDropped { count: u32 },
}

/// Where the session is in its lifecycle. Heads render UI off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AudioSessionState {
    Idle,
    /// `AudioStartSession` accepted; data plane round-trip in flight.
    Starting,
    Running,
    /// `AudioStopSession` accepted; data plane teardown in flight.
    Stopping,
    /// Terminal error state. Recoverable only via `AudioStopSession` (→
    /// `Idle`) followed by a new `AudioStartSession`.
    Error,
}

/// Why the session entered [`AudioSessionState::Error`]. Closed set so
/// heads can match exhaustively; an accompanying `reason: String`
/// carries adapter-specific detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AudioSessionErrorKind {
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
// Feature snapshot (Phase 2)
// ---------------------------------------------------------------------

/// Coherent snapshot of every feature the data plane publishes per hop.
///
/// Heads read the latest value via [`AppCoach::latest_features`] or drain
/// ordered history via [`AppCoach::drain_features`]. The data plane
/// publishes a new snapshot every `hop` samples worth of audio —
/// typically ~85Hz at 48kHz with a hop of 512.
///
/// Voicedness is encoded as [`f0_hz == 0.0`](Self::f0_hz): a voiced
/// frame reports a positive Hz value; an unvoiced frame reports
/// `0.0`. This matches the YIN node's sentinel and keeps the read
/// path branch-free. The other features (`onset`, `breath`,
/// `vibrato_rate`, `vibrato_depth`) are always populated — their
/// detectors emit `0.0` during inactive frames rather than a
/// distinguished sentinel.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FeatureSnapshot {
    /// Session-local monotonic hop sequence. Assigned once for every
    /// produced feature snapshot, before that same snapshot is published
    /// to both the latest-value read and the drained-history queue.
    ///
    /// Resets to `0` when a new data-plane session starts and may wrap
    /// naturally. Consumers detect a retained-stream discontinuity when
    /// the next value is not `previous.wrapping_add(1)`. This is continuity
    /// metadata, not a timestamp: repeated [`t_ms`](Self::t_ms) values are
    /// legal when the clock is coarse.
    pub hop_index: u64,

    /// Estimated fundamental frequency, in Hz. `0.0` means unvoiced
    /// (silence, breath, noise — not a frequency the detector
    /// trusts).
    pub f0_hz: f32,

    /// YIN periodicity confidence for this hop, in `0.0..=1.0`
    /// (`1 - d'` at the chosen lag). Higher means the detector trusts
    /// the pitch more; low values flag noisy / weakly-voiced frames
    /// even when `f0_hz` is nonzero. Heads use it as a continuous
    /// certainty signal (e.g. needle brightness) rather than a hard
    /// voiced/unvoiced gate.
    pub confidence: f32,

    /// Onset detector output for this hop. Positive values mark a
    /// note attack; `0.0` between attacks. The exact magnitude
    /// encodes attack strength — heads can use it as a transient
    /// indicator or as a binary "did something happen here" flag.
    pub onset: f32,

    /// Breath / aspiration energy estimate for this hop. Roughly the
    /// high-frequency / total-energy ratio when the breath detector
    /// is engaged, `0.0` when it isn't.
    pub breath: f32,

    /// Vibrato rate in Hz over the most recent analysis window
    /// (typically ~1.5s). `0.0` when no stable vibrato is detected.
    /// Lags voicing by the window length — first voiced frames after
    /// silence will report `0.0` until the window fills.
    pub vibrato_rate: f32,

    /// Vibrato depth in cents over the most recent analysis window
    /// (the analyzer builds the f0 contour as `1200 × log2(f)` and
    /// reports half its peak-to-peak range). Pairs with `vibrato_rate`;
    /// both go to `0.0` together when vibrato detection is inactive.
    pub vibrato_depth: f32,

    /// Wall-clock milliseconds (from the coach's [`Clock`]) at which
    /// this snapshot was published. Useful for placing samples on a
    /// time axis, but not for continuity: coarse clocks may assign the
    /// same value to consecutive hops. Use [`hop_index`](Self::hop_index)
    /// to distinguish a repeated poll from a newly-produced hop.
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

    /// Snapshot the latest feature estimate from the data plane.
    ///
    /// Returns `None` before the data plane has published any
    /// snapshot (no session running, or session just started and the
    /// first window hasn't accumulated yet). Otherwise returns the
    /// most recent [`FeatureSnapshot`].
    ///
    /// Non-blocking, lock-free (the implementation uses an
    /// `ArcSwap`-style snapshot). Heads should poll this at their UI
    /// cadence — there is no event for feature updates because the
    /// rate (~85Hz) would saturate the bounded event queue.
    fn latest_features(&self) -> Option<FeatureSnapshot>;

    /// Drain retained feature snapshots into `out`, in producer order.
    ///
    /// Non-blocking and lock-free. Appends every snapshot currently
    /// pending and returns immediately. Repeated timestamps are preserved;
    /// consumers use [`FeatureSnapshot::hop_index`] rather than `t_ms` to
    /// detect discontinuities.
    ///
    /// The queue is bounded. If the consumer stalls long enough to fill it,
    /// newly produced history snapshots may be dropped while
    /// [`latest_features`](Self::latest_features) continues advancing.
    /// Undrained snapshots survive a session restart; the next session is
    /// visible when `hop_index` resets to `0`.
    fn drain_features(&self, out: &mut Vec<FeatureSnapshot>);

    /// Negotiated parameters of the currently-running session, or
    /// `None` when no session is running.
    ///
    /// Lifecycle: `Some(_)` iff state is [`AudioSessionState::Running`].
    /// `None` everywhere else (`Idle`, `Starting`, `Stopping`,
    /// `Error`). See [`AudioInfo`] for the ordering guarantees
    /// against `CoachEvent::AudioSessionStateChanged`.
    ///
    /// Non-blocking, lock-free. Heads poll this whenever they need a
    /// sample-rate-dependent constant (scope window math, downsampling,
    /// envelope step size). Cheap enough to call every frame.
    fn audio_info(&self) -> Option<AudioInfo>;

    /// The coach's current musical frame of reference (the [`Scale`]),
    /// or `None` before the first [`Command::MusicConfigureSession`].
    ///
    /// Unlike [`audio_info`](Self::audio_info), this is **sticky**: it
    /// survives start/stop/error because the musical config is
    /// decoupled from the audio lifecycle. See [`MusicInfo`].
    ///
    /// Non-blocking, lock-free. Heads paint the dial / HUD off this
    /// every frame.
    fn music_info(&self) -> Option<MusicInfo>;
}
