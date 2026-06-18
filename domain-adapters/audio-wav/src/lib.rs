//! adapter-audio-wav: AudioDevices and AudioCapture ports backed by a
//! recorded WAV file.
//!
//! Intended for **visual replay** — `cargo run -p coach-game -- --replay-audio
//! <file.wav>` boots the real app (engine, worker, UI) but feeds it pre-recorded
//! PCM instead of the microphone. The feeder thread paces delivery in real time
//! so the UI animates as if live.
//!
//! # Two ports, one shared type
//!
//! [`new_devices`] (AudioDevices) and [`new_capture`] (AudioCapture) live in one
//! crate because they share a private [`WavStreamHandle`] type. `new_devices`
//! stashes the path + WAV spec inside a `StreamHandle`; `new_capture::open`
//! downcasts back to it. If they were separate crates the downcast would always
//! fail.
//!
//! # Source of truth: CaptureConfig, not the WAV header
//!
//! The host derives `CaptureConfig` (sample_rate, channels) *from the stream
//! this adapter's `new_devices` vended*, so the two are guaranteed to agree when
//! the caller follows the normal enumerate → pick → open flow. In `open`, the
//! WAV header is **validated** against `cfg` (mismatch → `UnsupportedConfig`);
//! all runtime math (chunk size, pacing) is then driven off `cfg`, never the WAV
//! header. See the spec's ⚠️ callout.
//!
//! # Visual-only contract
//!
//! Real-time pacing re-enters a ring buffer, so samples may be dropped or
//! realigned under load. This path is for watching the UI, not for bit-exact
//! reproduction (that belongs to the Phase-1 headless path).

mod capture;
mod devices;

/// Inner payload of a [`domain_ports::audio_devices::StreamHandle`] vended by
/// this adapter. Both `devices.rs` and `capture.rs` share this type; the capture
/// impl downcasts through `Arc<dyn Any>` to recover the WAV path and spec.
pub(crate) struct WavStreamHandle {
    pub(crate) path: std::path::PathBuf,
    pub(crate) spec: hound::WavSpec,
}

pub use capture::new as new_capture;
pub use devices::new as new_devices;

use domain_ports::audio_devices::AudioDevices;
use domain_ports::audio_session::{
    AudioInitError, AudioInitStatus, AudioPermissionSink, AudioSessionProvider,
};

/// Build an `AudioSessionProvider` backed by a WAV file.
///
/// Permission is always `Granted` (WAV replay doesn't need OS permission);
/// `new_devices()` returns the WAV-backed `AudioDevices` on every call.
/// `request()` invokes the sink immediately to exercise the park-and-resume path.
pub fn new_session_provider(wav_path: std::path::PathBuf) -> impl AudioSessionProvider {
    WavSessionProvider { wav_path }
}

struct WavSessionProvider {
    wav_path: std::path::PathBuf,
}

impl AudioSessionProvider for WavSessionProvider {
    fn init_status(&self) -> AudioInitStatus {
        AudioInitStatus::Granted
    }

    fn request(&self, sink: AudioPermissionSink) {
        sink.signal();
    }

    fn new_devices(&self) -> Result<Box<dyn AudioDevices>, AudioInitError> {
        Ok(Box::new(devices::new(self.wav_path.clone())))
    }
}

/// Cheaply validate that `path` is an openable WAV, returning its header on
/// success. Hosts call this up front (before the slow renderer boot) so a bad
/// `--replay-audio` path fails with a clean message instead of panicking deep
/// inside [`new_devices`]. Reads only the header, not the samples.
pub fn probe(path: &std::path::Path) -> Result<hound::WavSpec, String> {
    hound::WavReader::open(path)
        .map(|r| r.spec())
        .map_err(|e| e.to_string())
}
