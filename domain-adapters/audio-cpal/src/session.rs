//! AudioSessionProvider implementation backed by cpal / AVAudioSession.
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
use domain_ports::audio_session::{
    AudioInitError, AudioInitStatus, AudioPermissionSink, AudioSessionProvider,
};

/// Build an `AudioSessionProvider` appropriate for the current platform.
pub fn new() -> impl AudioSessionProvider {
    #[cfg(target_os = "ios")]
    {
        IosSessionProvider
    }
    #[cfg(not(target_os = "ios"))]
    {
        DefaultSessionProvider
    }
}

// =====================================================================
// Non-iOS default: permission always granted, devices from cpal.
// =====================================================================

#[cfg(not(target_os = "ios"))]
struct DefaultSessionProvider;

#[cfg(not(target_os = "ios"))]
impl AudioSessionProvider for DefaultSessionProvider {
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
struct IosSessionProvider;

#[cfg(target_os = "ios")]
impl AudioSessionProvider for IosSessionProvider {
    fn init_status(&self) -> AudioInitStatus {
        use objc2_avf_audio::{AVAudioSession, AVAudioSessionRecordPermission};
        // SAFETY: `sharedInstance` returns a reference to the process-wide
        // singleton; it is safe to call on any thread.
        let session = unsafe { AVAudioSession::sharedInstance() };
        // TODO(1.6.2): verify AVAudioSessionRecordPermission variant names
        // against the 0.3.2 binding on-device.
        match unsafe { session.recordPermission() } {
            AVAudioSessionRecordPermission::Granted => AudioInitStatus::Granted,
            AVAudioSessionRecordPermission::Denied => AudioInitStatus::Denied,
            AVAudioSessionRecordPermission::Undetermined => AudioInitStatus::Undetermined,
            _ => AudioInitStatus::Undetermined,
        }
    }

    fn request(&self, sink: AudioPermissionSink) {
        use block2::RcBlock;
        use objc2::runtime::Bool;
        use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategory};

        let session = unsafe { AVAudioSession::sharedInstance() };

        // Set category to record before requesting — required so the session
        // is configured for input when the permission dialog fires.
        // TODO(1.6.2): verify setCategory error handling on-device.
        let _ = unsafe {
            session.setCategory_error(AVAudioSessionCategory::Record, std::ptr::null_mut())
        };

        // The block captures ONLY the `Send` sink — never the `!Send` session.
        // It ignores the Bool (we re-read init_status() on resolution).
        let block = RcBlock::new(move |_granted: Bool| {
            sink.signal();
        });

        // TODO(1.6.2): verify requestRecordPermission method name in 0.3.2 binding.
        unsafe { session.requestRecordPermission(&*block) };
    }

    fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError> {
        use objc2_avf_audio::AVAudioSession;

        // Guard: only call setActive after permission is confirmed. Re-read live
        // status (not a stale snapshot) so Denied is reported accurately even if
        // permission was revoked between init_status() and new_devices().
        match self.init_status() {
            AudioInitStatus::Granted => {}
            AudioInitStatus::Denied => return Err(AudioInitError::Denied),
            AudioInitStatus::Undetermined => return Err(AudioInitError::Undetermined),
        }

        let session = unsafe { AVAudioSession::sharedInstance() };

        // TODO(1.6.2): verify setActive error mapping on-device.
        let mut err: *mut objc2_foundation::NSError = std::ptr::null_mut();
        let ok = unsafe { session.setActive_error(true, &mut err) };
        if !ok {
            let msg = if err.is_null() {
                "unknown error".to_string()
            } else {
                unsafe { (*err).localizedDescription().to_string() }
            };
            return Err(AudioInitError::ActivationFailed(msg));
        }

        Ok(Box::new(devices::new()))
    }
}
