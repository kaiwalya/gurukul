//! adapter-audio-apple: native Apple (macOS + iOS) audio adapter.
//!
//! Replaces `adapter-audio-cpal` on Apple platforms because cpal 0.17's iOS
//! input-unit construction is broken on device (`DeviceNotAvailable` on every
//! open attempt — confirmed in device logs; Phase 1.6.0 spike).
//!
//! # Phases
//!
//! - **Phase A (this commit):** `AudioDriver` — AVAudioSession permission +
//!   activation. Code lifted verbatim from adapter-audio-cpal's proven iOS path.
//! - **Phase B (this commit):** `AudioDevices` — enumerate the input via AVAudioSession / CoreAudio.
//! - **Phase C (this commit):** `AudioCapture` — native CoreAudio RemoteIO input unit
//!   (the actual fix: builds the unit cpal fails to build on device).
//!
//! # Platform scope
//!
//! This crate targets `any(target_os = "ios", target_os = "macos")` — Apple-wide,
//! unlike adapter-audio-cpal's iOS-only AVAudioSession gate.
//!
//! # Re-exports
//!
//! Callers use these names (matching the adapter-audio-cpal surface so the
//! wiring switch in coach.rs is a one-line cfg swap):
//! - `new_driver` — Phase A
//! - `new_devices` — Phase B
//! - `new_capture` — Phase C

mod capture;
mod devices;
mod driver;

pub use capture::new as new_capture;
pub use devices::new as new_devices;
pub use driver::new as new_driver;
