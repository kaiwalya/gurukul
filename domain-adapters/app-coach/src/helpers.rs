//! Small helpers shared across the control plane: capture-error
//! classification (port-level error → [`AudioSessionErrorKind`]) and the
//! default sample-rate picker.

use domain_ports::app_coach::AudioSessionErrorKind;
use domain_ports::audio_capture::{CaptureError, LifecycleEvent};
use domain_ports::audio_devices::SampleRateSupport;

/// Map a *terminal* mid-stream [`LifecycleEvent`] to the
/// [`AudioSessionErrorKind`] (and reason string) the head sees.
///
/// Returns `None` for the **recoverable** events — `Interrupted`,
/// `InterruptionEnded`, and `RouteChanged` — which the control plane's
/// state logic handles (pause / reconcile), not this classifier.
///
/// The taxonomy (per the 1.6.1 spec):
///
/// - `BackendError` / `DeviceUnavailable` → [`MidStreamFailure`]: the
///   stream died after a clean start, distinct from a start-time failure.
/// - `PermissionDenied` → [`PermissionDenied`]: route the user to Settings.
/// - `MediaServicesReset` → [`MidStreamFailure`]: terminal in Phase 1
///   (recoverable rebuild is deferred to 1.6.2, on-device only).
///
/// `DeviceUnavailable`'s "default intent → re-select rather than terminal"
/// exception lives in the control plane, which knows the stored start
/// intent; this classifier only sees the event.
///
/// [`MidStreamFailure`]: AudioSessionErrorKind::MidStreamFailure
/// [`PermissionDenied`]: AudioSessionErrorKind::PermissionDenied
pub(crate) fn classify_lifecycle_event(
    ev: &LifecycleEvent,
) -> Option<(AudioSessionErrorKind, String)> {
    match ev {
        LifecycleEvent::BackendError { reason } => Some((
            AudioSessionErrorKind::MidStreamFailure,
            format!("backend stream error: {reason}"),
        )),
        LifecycleEvent::DeviceUnavailable => Some((
            AudioSessionErrorKind::MidStreamFailure,
            "capture device became unavailable mid-session".to_string(),
        )),
        LifecycleEvent::PermissionDenied => Some((
            AudioSessionErrorKind::PermissionDenied,
            "microphone permission revoked mid-session".to_string(),
        )),
        LifecycleEvent::MediaServicesReset => Some((
            AudioSessionErrorKind::MidStreamFailure,
            "media services reset (terminal in Phase 1)".to_string(),
        )),
        // Recoverable — the control plane's state logic decides, not us.
        LifecycleEvent::Interrupted
        | LifecycleEvent::InterruptionEnded { .. }
        | LifecycleEvent::RouteChanged => None,
    }
}

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
