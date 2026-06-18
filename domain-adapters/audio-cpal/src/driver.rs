//! AudioDriver implementation backed by cpal / AVAudioSession.
//!
//! # Platform split
//!
//! - **iOS** (`#[cfg(target_os = "ios")]`): wraps `AVAudioSession` via the
//!   `objc2-avf-audio` binding. Maps `recordPermission()` to the enum;
//!   `requestRecordPermission(block)` fires the OS dialog and invokes the
//!   sink from the callback.
//! - **All other platforms** (Mac, Linux, Windows): permission is always
//!   `Granted`; `request()` invokes the sink immediately; `new_devices()`
//!   delegates to the existing cpal devices factory. This ensures the
//!   identical state machine is exercised on every platform.

use crate::devices;
use domain_ports::audio_devices::AudioDevices;
use domain_ports::audio_driver::{
    AudioDriver, AudioInitError, AudioInitStatus, AudioPermissionSink,
};

/// Build an `AudioDriver` appropriate for the current platform.
pub fn new() -> impl AudioDriver {
    #[cfg(target_os = "ios")]
    {
        IosAudioDriver
    }
    #[cfg(not(target_os = "ios"))]
    {
        DefaultAudioDriver
    }
}

// =====================================================================
// Non-iOS default: permission always granted, devices from cpal.
// =====================================================================

#[cfg(not(target_os = "ios"))]
struct DefaultAudioDriver;

#[cfg(not(target_os = "ios"))]
impl AudioDriver for DefaultAudioDriver {
    fn init_status(&self) -> AudioInitStatus {
        AudioInitStatus::Granted
    }

    fn request(&self, sink: AudioPermissionSink) {
        // Non-iOS: permission is always granted. Invoke the sink immediately
        // so the park-and-resume path in the control plane is exercised
        // on every platform (not just iOS).
        sink.signal();
    }

    fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError> {
        Ok(Box::new(devices::new()))
    }
}

// =====================================================================
// iOS: real AVAudioSession wiring.
// =====================================================================
//
// This block is cfg'd out on the Mac host; it gets compiled only for
// ios targets (simulator or device). A // TODO(1.6.2) comment marks any
// method call that needs on-device verification.

#[cfg(target_os = "ios")]
struct IosAudioDriver;

#[cfg(target_os = "ios")]
impl AudioDriver for IosAudioDriver {
    fn init_status(&self) -> AudioInitStatus {
        use objc2_avf_audio::{AVAudioApplication, AVAudioApplicationRecordPermission};
        // Modern (iOS 17+) permission read on the process-wide singleton.
        // `AVAudioApplicationRecordPermission` is a struct-of-consts, not a
        // closed enum — the wildcard arm is required by the binding.
        // SAFETY: `sharedInstance` returns the process-wide singleton; the
        // read is thread-safe.
        let app = unsafe { AVAudioApplication::sharedInstance() };
        match unsafe { app.recordPermission() } {
            AVAudioApplicationRecordPermission::Granted => AudioInitStatus::Granted,
            AVAudioApplicationRecordPermission::Denied => AudioInitStatus::Denied,
            AVAudioApplicationRecordPermission::Undetermined => AudioInitStatus::Undetermined,
            _ => AudioInitStatus::Undetermined,
        }
    }

    fn request(&self, sink: AudioPermissionSink) {
        use block2::RcBlock;
        use objc2::runtime::Bool;
        use objc2_avf_audio::{AVAudioApplication, AVAudioSession, AVAudioSessionCategoryRecord};

        // Configure the session for input *before* prompting, so a granted
        // session is immediately usable. `setCategory_error` returns a Result
        // (objc2 0.6 wraps the NSError** out-param); the record category is an
        // extern `Option<&NSString>` static, deref'd here.
        let session = unsafe { AVAudioSession::sharedInstance() };
        if let Some(category) = unsafe { AVAudioSessionCategoryRecord } {
            let _ = unsafe { session.setCategory_error(category) };
        }

        // The completion block captures ONLY the `Send` sink — never the
        // `!Send` session/app. It ignores the granted Bool; the control plane
        // re-reads init_status() on resolution (live truth, not a stale verdict).
        //
        // `RcBlock` requires `Fn`, but `sink.signal()` consumes the sink
        // (one-shot). The OS fires this completion block exactly once, so a
        // `Cell<Option<_>>` taken on first call bridges the two: `Fn`-callable,
        // but signals at most once. `Cell` is fine — the block is not `Sync`
        // and the OS serializes the single callback.
        let sink = std::cell::Cell::new(Some(sink));
        let block = RcBlock::new(move |_granted: Bool| {
            if let Some(sink) = sink.take() {
                sink.signal();
            }
        });

        // Class method (no &self): fires the OS dialog, or invokes the block
        // immediately if already decided. May call back on any thread.
        unsafe { AVAudioApplication::requestRecordPermissionWithCompletionHandler(&block) };
    }

    fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError> {
        use objc2_avf_audio::AVAudioSession;

        // Guard: only activate after permission is confirmed. Re-read live
        // status (not a stale snapshot) so revocation between init_status()
        // and here is reported accurately.
        match self.init_status() {
            AudioInitStatus::Granted => {}
            AudioInitStatus::Denied => return Err(AudioInitError::Denied),
            AudioInitStatus::Undetermined => return Err(AudioInitError::Undetermined),
        }

        // `setActive_error(true)` returns Result<(), Retained<NSError>>; map the
        // NSError to our ActivationFailed message.
        let session = unsafe { AVAudioSession::sharedInstance() };
        if let Err(err) = unsafe { session.setActive_error(true) } {
            return Err(AudioInitError::ActivationFailed(
                err.localizedDescription().to_string(),
            ));
        }

        Ok(Box::new(devices::new()))
    }
}
