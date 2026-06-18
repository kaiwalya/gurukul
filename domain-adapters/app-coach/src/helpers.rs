//! Small helpers shared across the control plane: capture-error
//! classification (port-level error → [`AudioSessionErrorKind`]) and the
//! default sample-rate picker.

use domain_ports::app_coach::AudioSessionErrorKind;
use domain_ports::audio_capture::CaptureError;
use domain_ports::audio_devices::SampleRateSupport;

pub(crate) fn classify_open_error(e: CaptureError) -> (AudioSessionErrorKind, String) {
    match e {
        CaptureError::InvalidHandle => (AudioSessionErrorKind::DeviceUnavailable, e.to_string()),
        CaptureError::DeviceUnavailable { .. } => {
            (AudioSessionErrorKind::DeviceUnavailable, e.to_string())
        }
        CaptureError::UnsupportedConfig { .. } => {
            (AudioSessionErrorKind::UnsupportedConfig, e.to_string())
        }
        CaptureError::Other(_) => (AudioSessionErrorKind::Other, e.to_string()),
    }
}

/// Pick a sample rate to request from the stream. Prefer 48000 when
/// it falls in any reported range; else use the lowest range minimum;
/// else guess 48000 for `ProbeOnly`.
pub(crate) fn preferred_sample_rate(s: &SampleRateSupport) -> u32 {
    const PREFERRED: u32 = 48_000;
    match s {
        SampleRateSupport::List(rates) => {
            if rates.contains(&PREFERRED) {
                PREFERRED
            } else {
                rates.first().copied().unwrap_or(PREFERRED)
            }
        }
        SampleRateSupport::Ranges(ranges) => {
            for (lo, hi) in ranges {
                if (*lo..=*hi).contains(&PREFERRED) {
                    return PREFERRED;
                }
            }
            ranges.iter().map(|(lo, _)| *lo).min().unwrap_or(PREFERRED)
        }
        SampleRateSupport::ProbeOnly => PREFERRED,
    }
}
