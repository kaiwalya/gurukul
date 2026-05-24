//! AudioDevices impl backed by cpal.

use crate::CpalStreamHandle;
use cpal::traits::{DeviceTrait, HostTrait};
use domain_ports::audio_devices::{
    AudioDevices, InputDevice, InputStream, SampleRateSupport, StreamHandle, Transport,
};
use std::sync::Arc;

/// Build a cpal-backed AudioDevices. Cheap — `cpal::default_host()`
/// is a no-op on most platforms (the host is a process-wide
/// singleton). The actual enumeration happens lazily on
/// [`AudioDevices::list_devices`].
pub fn new() -> impl AudioDevices {
    CpalAudioDevices {
        host: cpal::default_host(),
    }
}

struct CpalAudioDevices {
    host: cpal::Host,
}

impl AudioDevices for CpalAudioDevices {
    fn list_devices(&self) -> Vec<InputDevice> {
        let Ok(devices) = self.host.input_devices() else {
            return Vec::new();
        };
        devices.filter_map(device_to_port).collect()
    }

    fn default_input(&self) -> Option<InputStream> {
        let device = self.host.default_input_device()?;
        let mut mapped = device_to_port(device)?;
        // device_to_port returns a single-stream device by construction.
        mapped.streams.pop()
    }
}

/// Convert one cpal `Device` into the port-shaped `InputDevice`.
/// Returns `None` when the device exposes no usable input configs
/// (typically: it's an output-only endpoint we shouldn't be seeing).
fn device_to_port(device: cpal::Device) -> Option<InputDevice> {
    // cpal 0.17 deprecated `name()` in favour of `description()` (a
    // richer struct) and `id()` (a stable string identifier).
    let description = device.description().ok();
    let name = description
        .as_ref()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let persistent_id = device.id().ok().map(|id| id.to_string());

    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut channels: u16 = 0;
    if let Ok(configs) = device.supported_input_configs() {
        for cfg in configs {
            if channels == 0 {
                channels = cfg.channels();
            }
            ranges.push((cfg.min_sample_rate(), cfg.max_sample_rate()));
        }
    }
    if channels == 0 {
        return None;
    }

    let sample_rates = if ranges.is_empty() {
        SampleRateSupport::ProbeOnly
    } else {
        SampleRateSupport::Ranges(ranges)
    };

    let handle = StreamHandle(Arc::new(CpalStreamHandle { device }));

    Some(InputDevice {
        persistent_id,
        name: name.clone(),
        transport: Transport::Unknown,
        streams: vec![InputStream {
            handle,
            name,
            channels,
            sample_rates,
        }],
    })
}
