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
    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        on_frame: CaptureCallback,
    ) -> Result<CaptureSession, CaptureError>;
}
