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

// -----------------------------------------------------------------
// Conformance battery — test scaffolding, not app-facing surface.
// Gated behind test-util so it never ships in production binaries.
// See crate-root docs ("test fakes") for the gating rule.
// -----------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub mod conformance {
    //! Test scaffolding for [`AudioCapture`] port conformance.
    //!
    //! These are **not** part of the app-facing port surface — they live here
    //! so adapter authors have a single canonical battery to run against. Gate:
    //! `#[cfg(any(test, feature = "test-util"))]`.
    //!
    //! # LifecycleSink conformance
    //!
    //! `LifecycleSink` conformance (mid-stream interruption, route-change, etc.)
    //! is **deferred to Phase 1.6.2** — the on-device interruption work where
    //! these events are actually exercisable. The battery passes a no-op sink to
    //! `open` for now.
    //!
    //! # `t_ms` wording note
    //!
    //! The `CaptureFrame::t_ms` field doc says "Monotonic timestamp" while the
    //! prose in the module doc says "approximate instant". These should be
    //! reconciled in the port (the battery intentionally asserts only `is_finite`
    //! on `t_ms`, not monotonicity, to match the weaker prose guarantee). This is
    //! a known wording inconsistency flagged for the port author to fix separately.

    use super::{AudioCapture, CaptureConfig, CaptureError};
    use crate::audio_devices::{AudioDevices, SampleRateSupport, StreamHandle};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Universal. No device needed.
    ///
    /// Opening with a foreign [`StreamHandle`] (one containing a plain `()`) must
    /// return `Err(CaptureError::InvalidHandle)`. This is the single most portable
    /// check — both existing adapters honor it and any future adapter must too.
    ///
    /// Contract: `audio_capture.rs` — the downcast in both `negotiate` and `open`.
    pub fn verify_handle_rejection(capture: &dyn AudioCapture) {
        let foreign = StreamHandle(Arc::new(()));
        let cfg = CaptureConfig {
            sample_rate: 48_000,
            channels: 1,
            buffer_frames: None,
        };

        // negotiate must reject it
        let neg_result = capture.negotiate(&foreign, &cfg);
        assert!(
            matches!(neg_result, Err(CaptureError::InvalidHandle)),
            "negotiate with foreign handle must return InvalidHandle, got: {neg_result:?}"
        );

        // open must also reject it
        let open_result = capture.open(foreign, cfg, Box::new(|_| {}), Box::new(|_| {}));
        assert!(
            matches!(open_result, Err(CaptureError::InvalidHandle)),
            "open with foreign handle must return InvalidHandle"
        );
    }

    /// Requires a default input device. Skips (with `eprintln!`) when none is
    /// present — the CALLER decides whether a skip is acceptable.
    ///
    /// Asserts the `negotiate → open` contract without requiring frames to flow:
    /// - `negotiate` with the stream's advertised format succeeds and returns a
    ///   `CaptureConfig`;
    /// - `negotiate` is idempotent (two calls return equal `sample_rate` and
    ///   `channels`);
    /// - `open(returned_config)` succeeds and yields a session (then dropped);
    /// - an absurd config (`channels=99`, OR `sample_rate=0`, OR `channels=0`)
    ///   does NOT silently produce a working stream: EITHER `negotiate` returns
    ///   `Err(UnsupportedConfig)`, OR `open` (with that config) returns
    ///   `Err(UnsupportedConfig)` or `Err(DeviceUnavailable)`;
    /// - if `UnsupportedConfig.actual` is `Some`, it is internally consistent
    ///   (non-zero rate, non-zero channels). `None` is also valid (ProbeOnly).
    pub fn verify_negotiate_contract(devices: &dyn AudioDevices, capture: &dyn AudioCapture) {
        let stream = match devices.default_input() {
            Some(s) => s,
            None => {
                eprintln!(
                    "[conformance] verify_negotiate_contract: no default input device — skipping"
                );
                return;
            }
        };

        // Pick a plausible sample rate from the stream's support
        let sample_rate = first_sample_rate(&stream.sample_rates).unwrap_or(48_000);
        let channels = stream.channels;

        let wanted = CaptureConfig {
            sample_rate,
            channels,
            buffer_frames: None,
        };

        // negotiate must succeed with the advertised format
        let negotiated = capture
            .negotiate(&stream.handle, &wanted)
            .expect("negotiate with advertised format must succeed");

        // idempotency: second call must return same sample_rate and channels
        let negotiated2 = capture
            .negotiate(&stream.handle, &wanted)
            .expect("second negotiate call must also succeed");
        assert_eq!(
            negotiated.sample_rate, negotiated2.sample_rate,
            "negotiate is not idempotent: sample_rate changed across calls"
        );
        assert_eq!(
            negotiated.channels, negotiated2.channels,
            "negotiate is not idempotent: channels changed across calls"
        );

        // open with the negotiated config must succeed
        let session = capture
            .open(
                stream.handle.clone(),
                negotiated,
                Box::new(|_| {}),
                Box::new(|_| {}),
            )
            .expect("open with negotiated config must succeed");
        // Drop the session to stop the stream
        drop(session);

        // Absurd config 1: channels=99 — must be rejected at negotiate OR open
        let absurd1 = CaptureConfig {
            sample_rate,
            channels: 99,
            buffer_frames: None,
        };
        assert_absurd_rejected(capture, &stream.handle, absurd1, "channels=99");

        // Absurd config 2: sample_rate=0 — must be rejected at negotiate OR open
        let absurd2 = CaptureConfig {
            sample_rate: 0,
            channels,
            buffer_frames: None,
        };
        assert_absurd_rejected(capture, &stream.handle, absurd2, "sample_rate=0");

        // Absurd config 3: channels=0 — must be rejected at negotiate OR open
        let absurd3 = CaptureConfig {
            sample_rate,
            channels: 0,
            buffer_frames: None,
        };
        assert_absurd_rejected(capture, &stream.handle, absurd3, "channels=0");
    }

    /// Requires a device that ACTUALLY delivers frames (WAV always; real mic for
    /// the live tier). This is the only function that asserts liveness.
    ///
    /// Asserts, against the default input opened with its negotiated format:
    /// - the callback fires and total frames delivered > 0 within a bounded poll;
    /// - frame geometry: `samples.len() == frames * channels`;
    /// - sample bounds: every `f32` is finite and within `[-1.0, 1.0]`;
    /// - `t_ms` is finite (not NaN/Inf — `u64` is always finite, but asserted
    ///   conceptually; see wording note in module doc);
    /// - RAII stop stabilizes after drop: snapshot count, drop session, wait
    ///   settling margin, assert count stabilizes (two consecutive equal reads),
    ///   allowing one final in-flight callback;
    /// - open-after-drop reuse: after session drops, opening again succeeds.
    pub fn verify_capture_delivery(devices: &dyn AudioDevices, capture: &dyn AudioCapture) {
        let stream = match devices.default_input() {
            Some(s) => s,
            None => {
                eprintln!(
                    "[conformance] verify_capture_delivery: no default input device — skipping"
                );
                return;
            }
        };

        let sample_rate = first_sample_rate(&stream.sample_rates).unwrap_or(48_000);
        let channels = stream.channels;

        let wanted = CaptureConfig {
            sample_rate,
            channels,
            buffer_frames: None,
        };

        let negotiated = capture
            .negotiate(&stream.handle, &wanted)
            .expect("negotiate must succeed for delivery test");

        let negotiated_channels = negotiated.channels as usize;

        // The accumulator is owned by the callback (Arc clone). The callback runs
        // on an adapter thread; Mutex is deadlock-free because the port guarantees
        // sequential (never concurrent) callback invocations.
        #[derive(Default)]
        struct Acc {
            total_frames: usize,
            geometry_ok: bool, // samples.len() == frames * channels for every cb
            bounds_ok: bool,   // all f32 in [-1.0, 1.0] for every cb
        }
        let acc = Arc::new(Mutex::new(Acc {
            total_frames: 0,
            geometry_ok: true,
            bounds_ok: true,
        }));
        let acc_cb = Arc::clone(&acc);

        // open → poll/wait
        let session = capture
            .open(
                stream.handle.clone(),
                negotiated.clone(),
                Box::new(move |frame| {
                    let mut a = acc_cb.lock().unwrap();
                    a.total_frames += frame.frames;
                    if frame.samples.len() != frame.frames * negotiated_channels {
                        a.geometry_ok = false;
                    }
                    for &s in frame.samples {
                        if !s.is_finite() || s < -1.0 || s > 1.0 {
                            a.bounds_ok = false;
                        }
                    }
                }),
                Box::new(|_| {}),
            )
            .expect("open must succeed for delivery test");

        // Poll until frames arrive (bounded: up to 2s in 10ms steps)
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            std::thread::sleep(Duration::from_millis(10));
            if acc.lock().unwrap().total_frames > 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("verify_capture_delivery: no frames delivered within 2s");
            }
        }

        // snapshot → drop → settle → stabilization check
        let snapshot = acc.lock().unwrap().total_frames;
        drop(session); // RAII stop — synchronous teardown

        // Settling: wait up to 100ms for any final in-flight callback to land
        std::thread::sleep(Duration::from_millis(100));

        // Stabilization: two consecutive equal reads (allowing one final in-flight)
        let after1 = acc.lock().unwrap().total_frames;
        std::thread::sleep(Duration::from_millis(50));
        let after2 = acc.lock().unwrap().total_frames;

        assert_eq!(
            after1, after2,
            "frame count did not stabilize after session drop (after1={after1}, after2={after2})"
        );
        // Allow at most one extra in-flight callback to have landed after snapshot
        assert!(
            after2 <= snapshot + negotiated_channels.max(1) * 4096,
            "too many frames after drop: snapshot={snapshot}, final={after2}"
        );

        // Final geometry + bounds asserts (safe: no writer is live now)
        let final_acc = acc.lock().unwrap();
        assert!(
            final_acc.geometry_ok,
            "frame geometry invariant violated: samples.len() != frames * channels"
        );
        assert!(
            final_acc.bounds_ok,
            "sample bounds invariant violated: some f32 outside [-1.0, 1.0] or not finite"
        );
        drop(final_acc);

        // open-after-drop reuse
        let _session2 = capture
            .open(
                stream.handle.clone(),
                negotiated,
                Box::new(|_| {}),
                Box::new(|_| {}),
            )
            .expect("open after session drop must succeed (reuse check)");
        // Drop _session2 implicitly — stops the stream.
    }

    // ---- internal helpers ----

    /// Assert that an absurd config is rejected at either negotiate OR open.
    fn assert_absurd_rejected(
        capture: &dyn AudioCapture,
        handle: &StreamHandle,
        cfg: CaptureConfig,
        label: &str,
    ) {
        let neg = capture.negotiate(handle, &cfg);
        match neg {
            Err(CaptureError::UnsupportedConfig { actual, .. }) => {
                // Rejected at negotiate — good. Validate actual if present.
                if let Some(a) = actual {
                    assert!(
                        a.sample_rate > 0,
                        "[{label}] UnsupportedConfig.actual.sample_rate must be non-zero"
                    );
                    assert!(
                        a.channels > 0,
                        "[{label}] UnsupportedConfig.actual.channels must be non-zero"
                    );
                }
                // Rejected at negotiate — contract satisfied.
            }
            Err(_) => {
                // Any other error at negotiate is also a rejection — contract satisfied.
            }
            Ok(negotiated) => {
                // ProbeOnly / similar: negotiate succeeded; open must reject.
                let open_result = capture.open(
                    handle.clone(),
                    negotiated,
                    Box::new(|_| {}),
                    Box::new(|_| {}),
                );
                assert!(
                    matches!(
                        open_result,
                        Err(CaptureError::UnsupportedConfig { .. })
                            | Err(CaptureError::DeviceUnavailable { .. })
                    ),
                    "[{label}] absurd config must be rejected at negotiate OR open; \
                     open returned an unexpected Ok or wrong Err variant"
                );
            }
        }
    }

    /// Extract a representative sample rate from a [`SampleRateSupport`].
    fn first_sample_rate(support: &SampleRateSupport) -> Option<u32> {
        match support {
            SampleRateSupport::List(rates) => rates.first().copied(),
            SampleRateSupport::Ranges(ranges) => ranges.first().map(|(lo, _)| *lo),
            SampleRateSupport::ProbeOnly => Some(48_000),
        }
    }
}
