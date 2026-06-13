//! AudioDevices impl backed by a WAV file.
//!
//! Vends exactly one virtual device with one stream whose channel count and
//! sample rate match the WAV header. The coach-game device-selection UI lists
//! this device and auto-selects it as the default.

use crate::WavStreamHandle;
use domain_ports::audio_devices::{
    AudioDevices, DeviceId, InputDevice, InputStream, SampleRateSupport, StreamHandle, Transport,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Build a WAV-backed AudioDevices. Reads the WAV header once at construction
/// to report the file's real sample rate and channel count.
///
/// Returns an error (as a panic — construction failure is a programmer error at
/// startup) if the WAV cannot be opened. In practice callers validate the path
/// before constructing.
pub fn new(wav_path: PathBuf) -> impl AudioDevices {
    let spec = hound::WavReader::open(&wav_path)
        .unwrap_or_else(|e| panic!("adapter-audio-wav: cannot open {}: {e}", wav_path.display()))
        .spec();
    WavAudioDevices {
        path: wav_path,
        spec,
    }
}

struct WavAudioDevices {
    path: PathBuf,
    spec: hound::WavSpec,
}

impl AudioDevices for WavAudioDevices {
    fn list_devices(&self) -> Vec<InputDevice> {
        vec![self.make_device()]
    }

    fn default_input(&self) -> Option<InputStream> {
        Some(self.make_stream())
    }
}

impl WavAudioDevices {
    fn make_stream(&self) -> InputStream {
        let handle = StreamHandle(Arc::new(WavStreamHandle {
            path: self.path.clone(),
            spec: self.spec,
        }));

        InputStream {
            handle,
            name: "WAV replay".to_string(),
            channels: self.spec.channels,
            sample_rates: SampleRateSupport::List(vec![self.spec.sample_rate]),
        }
    }

    fn make_device(&self) -> InputDevice {
        let basename = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());

        InputDevice {
            persistent_id: Some(DeviceId("wav-replay".into())),
            name: format!("WAV replay: {basename}"),
            transport: Transport::Virtual,
            streams: vec![self.make_stream()],
        }
    }
}
