//! AudioDriver implementation backed by AVAudioSession (Apple-native).
//!
//! # Platform split
//!
//! - **iOS** (`#[cfg(target_os = "ios")]`): wraps `AVAudioSession` via the
//!   `objc2-avf-audio` binding. Maps `recordPermission()` to the enum;
//!   `requestRecordPermission(block)` fires the OS dialog and invokes the
//!   sink from the callback.
//! - **macOS** (`#[cfg(target_os = "macos")]`): mic TCC exists but
//!   AVAudioSession is not available. Permission is treated as always Granted
//!   (same as cpal's non-iOS DefaultAudioDriver). `request()` invokes the
//!   sink immediately; `new_devices()` delegates to this crate's `devices::new()`
//!   — see TODO(Phase B). Keeping the state machine identical on both platforms
//!   means the control-plane park-and-resume path is exercised everywhere.
//!
//! Lifted from `adapter-audio-cpal/src/driver.rs` (the proven iOS path).

use domain_ports::audio_driver::{
    AudioDriver, AudioInitError, AudioInitStatus, AudioPermissionSink,
};

// devices::new() is called by new_devices() on both platforms.
// TODO(Phase B): uncomment when devices module lands.
// use crate::devices;

/// Build an `AudioDriver` appropriate for the current Apple platform.
pub fn new() -> impl AudioDriver {
    #[cfg(target_os = "ios")]
    {
        IosAudioDriver
    }
    #[cfg(target_os = "macos")]
    {
        MacAudioDriver
    }
}

// =====================================================================
// macOS default: permission always granted, devices deferred to Phase B.
// =====================================================================

#[cfg(target_os = "macos")]
struct MacAudioDriver;

#[cfg(target_os = "macos")]
impl AudioDriver for MacAudioDriver {
    fn init_status(&self) -> AudioInitStatus {
        // macOS has mic TCC but AVAudioSession is not available here.
        // Treat as Granted — the same approach as adapter-audio-cpal's
        // DefaultAudioDriver on non-iOS. The control plane's state machine
        // is exercised identically on both platforms.
        AudioInitStatus::Granted
    }

    fn request(&self, sink: AudioPermissionSink) {
        // macOS: permission is treated as always granted. Invoke the sink
        // immediately so the park-and-resume path is exercised on every platform.
        sink.signal();
    }

    fn new_devices(
        &self,
    ) -> Result<Box<dyn domain_ports::audio_devices::AudioDevices>, AudioInitError> {
        // TODO(Phase B): replace with Ok(Box::new(crate::devices::new()))
        Err(AudioInitError::ActivationFailed(
            "audio-apple devices not yet implemented (Phase B)".to_string(),
        ))
    }
}

// =====================================================================
// iOS: real AVAudioSession wiring.
// =====================================================================
//
// This block compiles only for iOS targets (simulator or device).
// Code lifted verbatim from adapter-audio-cpal's IosAudioDriver —
// this path is confirmed working on device (Phase 1.6.0 spike).

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
            if let Some(s) = sink.take() {
                s.signal();
            }
        });

        // Class method (no &self): fires the OS dialog, or invokes the block
        // immediately if already decided. May call back on any thread.
        unsafe { AVAudioApplication::requestRecordPermissionWithCompletionHandler(&block) };
    }

    fn new_devices(
        &self,
    ) -> Result<Box<dyn domain_ports::audio_devices::AudioDevices>, AudioInitError> {
        use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategoryRecord};

        // Guard: only activate after permission is confirmed. Re-read live
        // status (not a stale snapshot) so revocation between init_status()
        // and here is reported accurately.
        match self.init_status() {
            AudioInitStatus::Granted => {}
            AudioInitStatus::Denied => return Err(AudioInitError::Denied),
            AudioInitStatus::Undetermined => return Err(AudioInitError::Undetermined),
        }

        // Configure the session for input *before* activating. `request()`
        // also sets this, but that path only runs when the permission dialog
        // fires; on a later launch that boots already-Granted, `request()` is
        // never called, so the category would otherwise stay at its playback
        // default and input capture fails with a misleading DeviceNotAvailable.
        // Setting it here makes every bring-up configure recording, independent
        // of whether the dialog fired this launch.
        let session = unsafe { AVAudioSession::sharedInstance() };
        if let Some(category) = unsafe { AVAudioSessionCategoryRecord } {
            let _ = unsafe { session.setCategory_error(category) };
        }

        // `setActive_error(true)` returns Result<(), Retained<NSError>>; map the
        // NSError to our ActivationFailed message.
        if let Err(err) = unsafe { session.setActive_error(true) } {
            return Err(AudioInitError::ActivationFailed(
                err.localizedDescription().to_string(),
            ));
        }

        // TODO(Phase B): replace with Ok(Box::new(crate::devices::new()))
        Err(AudioInitError::ActivationFailed(
            "audio-apple devices not yet implemented (Phase B)".to_string(),
        ))
    }
}
