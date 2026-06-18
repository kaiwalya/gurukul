//! AudioCapture port: open an input stream and receive PCM frames.
//!
//! Captures pair with the [`AudioDevices`](crate::audio_devices)
//! port: callers enumerate, pick a stream, and pass its
//! [`StreamHandle`] here together with a desired
//! [`CaptureConfig`].
//!
//! # Delivery model — callback
//!
//! Frames arrive on whatever thread the adapter chooses (typically a
//! real-time audio thread). The supplied callback must be:
//!
//! - **allocation-free and lock-free** on the audio thread, *or*
//! - hand off to a non-RT thread immediately (e.g. push into a
//!   pre-allocated ring buffer the UI tick drains).
//!
//! Adapters guarantee the callback is invoked sequentially — never
//! concurrently with itself — but make no guarantee about *which*
//! thread it runs on, and the thread may change across runs (HAL
//! IO proc vs AVAudioEngine tap on macOS, for instance).
//!
//! # Sample format
//!
//! Frames are `f32` PCM in the range `[-1.0, 1.0]`. Adapters convert
//! from the device's native format (Int16, Int32, Float32 at various
//! interleave layouts). If the device exposes only one channel the
//! per-frame layout is just one sample; with >1 channel samples are
//! **interleaved** (`L, R, L, R, …`).
//!
//! # Lifecycle
//!
//! The normal flow is `negotiate() → open(verbatim)`:
//!
//! 1. Call [`AudioCapture::negotiate`] with the desired config to learn
//!    the exact format the device will deliver. This must happen before
//!    building the engine (so the engine is built for the right rate).
//! 2. Pass the returned [`CaptureConfig`] verbatim to
//!    [`AudioCapture::open`]. `open()` trusts the config it is given and
//!    fails only on genuine runtime errors (device unplugged, busy).
//!
//! [`AudioCapture::open`] returns a [`CaptureSession`] guard. The
//! stream stops when the session is dropped — there is no explicit
//! `stop()`. This is intentional: forgetting to stop is a real bug
//! on every platform (CoreAudio leaks IO procs, WASAPI leaks
//! endpoints), and RAII makes "I forgot" structurally impossible.

use crate::audio_devices::StreamHandle;

/// Frames-per-second and channel count for the open call. Adapters
/// validate this against the stream's
/// [`SampleRateSupport`](crate::audio_devices::SampleRateSupport) —
/// callers should pick a value that's actually supported. If the
/// stream is `ProbeOnly`, "validate" means "try the open and see
/// what happens."
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// Requested IO buffer size in frames. `None` means "let the
    /// adapter / driver pick" (cpal's `BufferSize::Default`). A
    /// smaller value lowers callback latency at the cost of more
    /// frequent wakeups; the device may reject sizes outside its
    /// supported range, surfacing as
    /// [`CaptureError::UnsupportedConfig`].
    pub buffer_frames: Option<u32>,
}

/// One callback invocation worth of PCM samples.
///
/// `samples` is borrowed from adapter-internal scratch and is valid
/// only for the duration of the callback. Copy out anything you need
/// to persist before returning.
pub struct CaptureFrame<'a> {
    /// Interleaved PCM samples. Length is `frames * channels`.
    pub samples: &'a [f32],

    /// Number of audio frames (one sample per channel = one frame).
    pub frames: usize,

    /// Monotonic timestamp from the adapter's clock, capturing the
    /// approximate instant the first sample in this frame was
    /// recorded. Adapter-defined epoch (typically the moment of
    /// [`AudioCapture::open`]).
    pub t_ms: u64,
}

/// Frame callback. Boxed `FnMut` to match what every native audio
/// API expects (cpal, CoreAudio IO procs, WASAPI render callbacks)
/// and to let callers mutate per-frame state (running stats, ring
/// buffer cursors) without interior mutability.
pub type CaptureCallback = Box<dyn FnMut(CaptureFrame<'_>) + Send + 'static>;

/// A mid-stream lifecycle signal the adapter raises *after* [`open`]
/// succeeds — the seam for the OS taking the mic away.
///
/// These are the platform-neutral distillation of the events Apple's
/// `AVAudioSession`, Android's Camera2, and a Mac CoreAudio device
/// listener all surface in their own vocabularies. Apple's explicit
/// warning is honoured in the shape: "interruption ended" (a
/// *conditional* resume hint) is **not** collapsed with "route
/// changed" (an *unconditional* re-read) — different variants, different
/// recovery paths.
///
/// An event is a **trigger**, not a recovery instruction: the control
/// plane re-evaluates current truth (re-enumerate / renegotiate /
/// reopen) when it receives one, so a missed, duplicated, or coalesced
/// event is safe.
///
/// [`open`]: AudioCapture::open
#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    /// An interruption began (incoming call, Siri, another app grabbed
    /// the session). The stream is gone; do **not** reopen yet.
    Interrupted,
    /// An interruption ended. `should_resume` carries the OS hint
    /// (Apple's `.shouldResume` option): resume only if set. The
    /// control plane never auto-resumes — this merely unlocks resume.
    InterruptionEnded { should_resume: bool },
    /// The audio route changed (headphones plugged/unplugged, a device
    /// became the new default). Re-read config and keep running if the
    /// stored intent is still satisfiable.
    RouteChanged,
    /// The OS media services were reset — every audio object is now
    /// invalid. Phase-1 treats this as terminal (the recoverable
    /// rebuild lands in 1.6.2 on-device, where it is actually
    /// exercisable).
    MediaServicesReset,
    /// The capture device was lost / disconnected.
    DeviceUnavailable,
    /// Mic permission was revoked mid-session. Terminal; route the user
    /// to OS Settings.
    PermissionDenied,
    /// The backend's stream errored (cpal's error closure, formerly an
    /// `eprintln!` dead-end). Carries the backend's message.
    BackendError { reason: String },
}

/// The adapter→control-plane sink for [`LifecycleEvent`]s.
///
/// It is a plain `Send` enqueuer: every invocation merely pushes one
/// event onto the control plane's mailbox. It is `Fn` (not `FnMut` /
/// `FnOnce`) because it fires repeatedly over the life of a stream and
/// only enqueues — it owns no mutable state.
///
/// **Lifecycle.** It is born in [`open`](AudioCapture::open) and lives
/// as long as the adapter's notification source can fire. It must
/// capture **only `Send` things** (a cloned channel sender, a frozen
/// generation token) and **never the `!Send` [`CaptureSession`]** —
/// observer notifications fire on arbitrary OS threads, so the sink
/// must be safe to *move to* the notification thread.
///
/// On most adapters the sink dies with the [`CaptureSession`] (cpal's
/// error closure is owned by the stream). On a platform whose
/// interruption model spans the stream's life — iOS, where
/// `Interrupted` tears down the audio unit but the paired
/// `InterruptionEnded` must still be deliverable (1.6.2) — the observer
/// feed that owns the sink must outlive a single stream open/close, so
/// the adapter parks the sink on a longer-lived object than the cpal
/// stream. Either way the sink must not keep the control-plane channel
/// alive past coach shutdown.
///
/// `Send` (not `Sync`) is deliberate and sufficient for the adapters in
/// this commit: the sink is *moved into* a single notification closure
/// (cpal's stream-error callback), not shared across threads. If a
/// future platform (iOS, 1.6.2) needs to fan one sink out to several
/// observer threads concurrently, wrap it in an `Arc` adapter-side or
/// promote this alias to `Arc<dyn Fn(..) + Send + Sync>` then.
///
/// Note the sink takes only a [`LifecycleEvent`] — no generation. The
/// generation lives in the *control plane's* closure that constructs
/// this sink, exactly like the permission sink: the closure wraps each
/// event with the generation that was current at `open()` time before
/// enqueueing. The port type stays generation-free.
pub type LifecycleSink = Box<dyn Fn(LifecycleEvent) + Send + 'static>;

/// Failure modes for [`AudioCapture::open`] and [`AudioCapture::negotiate`].
///
/// Closed set so callers can match exhaustively. `Other` is the
/// escape valve for adapter-specific failures that don't deserve a
/// shared variant.
#[derive(Debug)]
pub enum CaptureError {
    /// The [`StreamHandle`] was vended by a different adapter
    /// instance, or has been invalidated. Re-enumerate via
    /// [`AudioDevices::list_devices`](crate::audio_devices::AudioDevices::list_devices).
    InvalidHandle,
    /// The stream cannot satisfy the requested [`CaptureConfig`].
    /// `wanted` is the config that was requested; `actual` is the
    /// config the device supports, or `None` when the device cannot
    /// report what it supports (generic `ProbeOnly` stream).
    UnsupportedConfig {
        wanted: CaptureConfig,
        actual: Option<CaptureConfig>,
    },
    /// The device exists in enumeration but cannot be opened right
    /// now (busy, permission denied, just unplugged).
    DeviceUnavailable {
        reason: String,
    },
    Other(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHandle => write!(f, "invalid stream handle"),
            Self::UnsupportedConfig { wanted, actual } => {
                write!(
                    f,
                    "unsupported config: wanted sample_rate={} channels={} buffer_frames={:?}",
                    wanted.sample_rate, wanted.channels, wanted.buffer_frames,
                )?;
                if let Some(a) = actual {
                    write!(
                        f,
                        "; device supports sample_rate={} channels={} buffer_frames={:?}",
                        a.sample_rate, a.channels, a.buffer_frames,
                    )?;
                }
                Ok(())
            }
            Self::DeviceUnavailable { reason } => write!(f, "device unavailable: {reason}"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Active capture session. The stream runs until the session is
/// dropped, at which point the adapter performs a synchronous
/// teardown (stop the IO proc / unregister the callback / release
/// the device).
///
/// **Not `Send`.** Several native audio APIs — most importantly
/// CoreAudio via cpal on macOS — require the stream handle to be
/// dropped on the thread that opened it. Modelling the session as
/// thread-local is the only honest shape; callers run the session
/// on whatever thread opens it and drop it there.
pub struct CaptureSession {
    teardown: Option<Box<dyn FnOnce()>>,
}

impl CaptureSession {
    /// Construct a session from an adapter-supplied teardown.
    /// Adapter-side helper, not part of the trait's public surface
    /// to apps.
    pub fn new<F: FnOnce() + 'static>(teardown: F) -> Self {
        Self {
            teardown: Some(Box::new(teardown)),
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        if let Some(t) = self.teardown.take() {
            t();
        }
    }
}

pub trait AudioCapture: Send + Sync {
    /// Preflight that learns the exact format the device will deliver,
    /// **before** the engine is built.
    ///
    /// The control plane calls `negotiate` with the desired config,
    /// receives the exact [`CaptureConfig`] the device will honour, builds
    /// its engine for that rate, then passes the returned config verbatim
    /// to `open`. This ensures the engine is sized correctly the first time
    /// and that a format mismatch is detected before any frames flow.
    ///
    /// On a stream the adapter cannot characterise without actually opening
    /// it (generic `ProbeOnly`), returns
    /// `Err(UnsupportedConfig { wanted: requested.clone(), actual: None })`.
    ///
    /// On success the returned config represents the exact format `open`
    /// will deliver; pass it verbatim.
    fn negotiate(
        &self,
        handle: &StreamHandle,
        requested: &CaptureConfig,
    ) -> Result<CaptureConfig, CaptureError>;

    /// Open the stream identified by `handle` with the given
    /// `cfg`, delivering frames to `on_frame` until the returned
    /// [`CaptureSession`] is dropped.
    ///
    /// The control plane passes the config returned by [`negotiate`]
    /// verbatim. `open` trusts it and fails only on genuine runtime errors
    /// (device unplugged, busy) — format validation is `negotiate`'s job.
    ///
    /// `on_event` is the [`LifecycleSink`]: the adapter calls it once per
    /// mid-stream lifecycle signal (interruption, route change, backend
    /// error, …). The sink only enqueues onto the control plane's mailbox,
    /// so it is safe to call from any OS-notification thread. Adapters that
    /// cannot be interrupted (a WAV file source) accept it and ignore it.
    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        on_frame: CaptureCallback,
        on_event: LifecycleSink,
    ) -> Result<CaptureSession, CaptureError>;
}
