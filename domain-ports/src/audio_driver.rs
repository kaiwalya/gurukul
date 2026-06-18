//! AudioDriver port: async OS audio permission + session activation.
//!
//! On iOS (and Android), the app must obtain microphone permission before
//! audio devices can be used. The OS permission model is inherently async:
//! the user may or may not be shown a dialog, and the result arrives on an
//! arbitrary thread via a callback. This port models that lifecycle as a
//! *factory* that yields a ready [`crate::audio_devices::AudioDevices`].
//!
//! # Design rationale
//!
//! Unprepared state is unrepresentable: `new_devices()` succeeds only when
//! the session is active and permission is granted. A caller that holds a
//! `Box<dyn AudioDevices>` produced by this port can trust the session
//! is ready — it need not check permission separately.
//!
//! # Permission flow
//!
//! 1. Call `init_status()` to read the current state without any dialog.
//! 2. If `Undetermined`, call `request(sink)` to fire the OS dialog.
//!    The control plane supplies a sink whose `signal()` re-enqueues a
//!    control-plane event so the state can be re-read. The sink carries no
//!    payload — on resolution the control plane re-reads `init_status()`
//!    (Decision 3 from the spec: live truth, not a stale verdict).
//! 3. If `Granted`, call `new_devices()` to get a live `AudioDevices`.
//!
//! # Non-iOS platforms
//!
//! The default (Mac/Linux/Windows) implementation treats permission as
//! always `Granted` and invokes the sink immediately in `request()` so the
//! same state machine is exercised everywhere.

use crate::audio_devices::AudioDevices;

/// Current state of the OS mic permission for this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AudioInitStatus {
    /// The user has not been asked yet. `request()` will show a dialog.
    Undetermined,
    /// The user denied permission. Showing the dialog again does nothing;
    /// the user must enable it in OS Settings.
    Denied,
    /// Permission is granted. `new_devices()` may be called.
    Granted,
}

/// Why `new_devices()` failed.
#[derive(Debug)]
pub enum AudioInitError {
    /// The user denied mic permission.
    Denied,
    /// The user has not been asked yet. The caller should call `request()`
    /// before retrying `new_devices()`.
    Undetermined,
    /// Permission was granted but `setActive(true)` (or equivalent) failed.
    /// The `String` carries the platform error message.
    ActivationFailed(String),
}

impl std::fmt::Display for AudioInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => write!(f, "microphone permission denied"),
            Self::Undetermined => write!(f, "microphone permission not yet determined"),
            Self::ActivationFailed(msg) => write!(f, "audio session activation failed: {msg}"),
        }
    }
}

/// A one-shot signal the OS permission callback fires when permission state
/// changes.
///
/// The control plane constructs a sink per request by closing over the
/// generation it wants to re-evaluate and the `Sender<Input>` it owns.
/// The sink type itself carries only a boxed callback — `FnOnce() + Send` —
/// so the port has no dependency on the control plane's private `Input` enum.
///
/// When the OS callback fires (on an arbitrary thread), it calls `sink.signal()`
/// which invokes the closure, re-enqueuing the appropriate control-plane event.
pub struct AudioPermissionSink(pub Box<dyn FnOnce() + Send>);

impl AudioPermissionSink {
    /// Invoke the callback. Consumes the sink (one-shot).
    pub fn signal(self) {
        (self.0)();
    }
}

/// A factory that activates the OS audio session and yields a ready
/// [`AudioDevices`].
///
/// On iOS (and Android) the session requires explicit OS permission.
/// On Mac/Linux/Windows permission is always granted and `new_devices()`
/// succeeds immediately.
///
/// The port owns the lifecycle policy: `new_devices()` succeeds ONLY when
/// `init_status() == Granted` AND the session activates without error.
pub trait AudioDriver: Send + Sync {
    /// Synchronously read the current permission state. Never shows a
    /// dialog. Cheap.
    fn init_status(&self) -> AudioInitStatus;

    /// Fire the async OS permission request. Returns immediately. The
    /// `sink` is invoked later, on an arbitrary thread, when the state
    /// may have changed. The sink carries no payload — the caller re-reads
    /// `init_status()` on receipt.
    ///
    /// On non-iOS platforms, invokes the sink promptly (before returning
    /// or on the calling thread) so the park-and-resume flow is exercised
    /// everywhere.
    fn request(&self, sink: AudioPermissionSink);

    /// Bring up a working session and return a ready `AudioDevices`.
    ///
    /// Succeeds only when `init_status() == Granted` AND the OS session
    /// activates. Each call produces a *fresh* independent devices handle;
    /// the caller is responsible for dropping it when the session ends.
    fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError>;
}

// ---------------------------------------------------------------------
// Test fake — gated. See crate-root docs ("test fakes") for the rule.
// ---------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub use fakes::FakeAudioDriver;

#[cfg(any(test, feature = "test-util"))]
pub mod fakes {
    use super::{AudioDriver, AudioInitError, AudioInitStatus, AudioPermissionSink};
    use crate::audio_devices::{AudioDevices, InputDevice, InputStream};
    use std::sync::{Arc, Mutex};

    /// What `FakeAudioDriver` returns from `new_devices()`.
    pub enum FakeDevicesResult {
        /// Return the supplied `AudioDevices` impl (boxed).
        Ok(Box<dyn AudioDevices>),
        /// Return this error.
        Err(FakeActivationError),
    }

    /// Which `AudioInitError` variant the fake `new_devices()` should return.
    pub enum FakeActivationError {
        Denied,
        Undetermined,
        ActivationFailed(String),
    }

    struct Inner {
        status: AudioInitStatus,
        /// Pending sink from a `request()` call that hasn't been resolved yet.
        pending_sink: Option<AudioPermissionSink>,
        /// What `new_devices()` returns on the next call.
        /// If `None`, returns the default — a trivially-empty devices impl.
        devices_result: Option<FakeDevicesResult>,
    }

    /// Deterministic fake for consumer tests. Controllable from the test thread.
    ///
    /// # Usage
    ///
    /// ```ignore
    /// let provider = FakeAudioDriver::new(AudioInitStatus::Undetermined);
    /// // ... hand to the control plane via AppCoachDeps ...
    ///
    /// // Later, from the test thread, flip permission and resolve:
    /// provider.set_status(AudioInitStatus::Granted);
    /// provider.resolve();    // invokes the pending sink (if any)
    /// ```
    pub struct FakeAudioDriver {
        inner: Arc<Mutex<Inner>>,
    }

    impl FakeAudioDriver {
        /// Create a new fake with the given initial status.
        pub fn new(initial_status: AudioInitStatus) -> Self {
            Self {
                inner: Arc::new(Mutex::new(Inner {
                    status: initial_status,
                    pending_sink: None,
                    devices_result: None,
                })),
            }
        }

        /// Change the status. Does not invoke the pending sink — call
        /// `resolve()` separately to fire it.
        pub fn set_status(&self, status: AudioInitStatus) {
            self.inner.lock().unwrap().status = status;
        }

        /// Set what `new_devices()` returns on the next call. If never set,
        /// returns an empty `AudioDevices` impl (suitable for tests that only
        /// care about the permission flow and never reach `resolve_stream`).
        pub fn set_devices_result(&self, result: FakeDevicesResult) {
            self.inner.lock().unwrap().devices_result = Some(result);
        }

        /// Invoke the pending sink (if any), consuming it. Call this from
        /// the test thread after adjusting status to simulate the OS callback
        /// arriving. No-op if no sink is pending.
        pub fn resolve(&self) {
            let sink = self.inner.lock().unwrap().pending_sink.take();
            if let Some(s) = sink {
                s.signal();
            }
        }

        /// True if a `request()` call is pending (sink not yet resolved).
        pub fn has_pending_request(&self) -> bool {
            self.inner.lock().unwrap().pending_sink.is_some()
        }
    }

    impl AudioDriver for FakeAudioDriver {
        fn init_status(&self) -> AudioInitStatus {
            self.inner.lock().unwrap().status
        }

        fn request(&self, sink: AudioPermissionSink) {
            self.inner.lock().unwrap().pending_sink = Some(sink);
        }

        fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError> {
            let result = self.inner.lock().unwrap().devices_result.take();
            match result {
                None => Ok(Box::new(EmptyDevices)),
                Some(FakeDevicesResult::Ok(d)) => Ok(d),
                Some(FakeDevicesResult::Err(e)) => Err(match e {
                    FakeActivationError::Denied => AudioInitError::Denied,
                    FakeActivationError::Undetermined => AudioInitError::Undetermined,
                    FakeActivationError::ActivationFailed(msg) => {
                        AudioInitError::ActivationFailed(msg)
                    }
                }),
            }
        }
    }

    /// Trivially-empty `AudioDevices` used by the fake when no devices result
    /// is configured. Never returns any devices or a default input — the control
    /// plane's `resolve_stream` will return `None` and fail the session start with
    /// `DeviceUnavailable`, which is fine for tests that only care about the
    /// permission state machine.
    pub struct EmptyDevices;

    impl AudioDevices for EmptyDevices {
        fn list_devices(&self) -> Vec<InputDevice> {
            Vec::new()
        }
        fn default_input(&self) -> Option<InputStream> {
            None
        }
    }
}
