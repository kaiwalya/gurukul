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
pub struct CaptureConfig {
    pub sample_rate: u32,
    pub channels: u16,
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

/// Failure modes for [`AudioCapture::open`].
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
    /// The stream cannot satisfy the requested
    /// [`CaptureConfig`] (rate, channels). The reason is
    /// adapter-defined but should be specific enough for a log line.
    UnsupportedConfig {
        reason: String,
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
            Self::UnsupportedConfig { reason } => write!(f, "unsupported config: {reason}"),
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
    /// Open the stream identified by `handle` with the given
    /// `cfg`, delivering frames to `on_frame` until the returned
    /// [`CaptureSession`] is dropped.
    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        on_frame: CaptureCallback,
    ) -> Result<CaptureSession, CaptureError>;
}
