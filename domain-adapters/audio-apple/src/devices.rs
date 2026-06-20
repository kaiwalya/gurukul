//! AudioDevices implementation for Apple platforms.
//!
//! # Platform split
//!
//! - **iOS** (`#[cfg(target_os = "ios")]`): Enumerates via AVAudioSession. iOS
//!   exposes essentially one input route, so this returns a single `InputDevice`
//!   with a single `InputStream`. Channel count and sample rate are read from
//!   the live `sharedInstance()`. `sample_rates` is `ProbeOnly` — AVAudioSession
//!   does not enumerate a discrete list; callers request a rate and it
//!   succeeds or fails.
//!
//! - **macOS** (`#[cfg(target_os = "macos")]`): Reads the default input device
//!   via `coreaudio::audio_unit::macos_helpers::get_default_device_id(true)`.
//!   Returns an empty list / `None` if no input device is present (headless CI,
//!   Mac without a mic). The `AppleStreamHandle` carries the `AudioDeviceID`
//!   (u32) that Phase C's capture will downcast to build the RemoteIO unit.
//!
//! # AppleStreamHandle
//!
//! A private struct stashed inside the opaque `StreamHandle(Arc<dyn Any + …>)`.
//! Phase C's `open()` will downcast back to this to retrieve the platform info
//! it needs (no-info marker on iOS; `AudioDeviceID` on macOS).

use domain_ports::audio_devices::{
    AudioDevices, InputDevice, InputStream, SampleRateSupport, StreamHandle, Transport,
};
use std::sync::Arc;

// -----------------------------------------------------------------
// AppleStreamHandle — private carrier between Phase B and Phase C.
// -----------------------------------------------------------------

/// Opaque payload stored inside `StreamHandle` for this adapter.
///
/// **iOS:** a unit struct — the session is process-wide; Phase C
/// re-reads `sharedInstance()` to build the RemoteIO unit.
///
/// **macOS:** carries the `AudioDeviceID` (u32) of the default input so
/// Phase C can call `macos_helpers::audio_unit_from_device_id`.
///
/// Both arms are `Send + Sync`: `u32` trivially is; the iOS unit struct has
/// no fields. No raw ObjC pointers are stored.
#[cfg(target_os = "ios")]
pub(crate) struct AppleStreamHandle;

#[cfg(target_os = "macos")]
pub(crate) struct AppleStreamHandle {
    /// CoreAudio device identifier for the default input.
    /// `AudioDeviceID = AudioObjectID = u32` — trivially Send+Sync.
    /// Read by Phase C's capture open path; pre-provisioned here.
    #[allow(dead_code)]
    pub(crate) device_id: u32,
}

// Safety: u32 / unit struct — trivially Send+Sync.
#[cfg(target_os = "ios")]
unsafe impl Send for AppleStreamHandle {}
#[cfg(target_os = "ios")]
unsafe impl Sync for AppleStreamHandle {}

#[cfg(target_os = "macos")]
unsafe impl Send for AppleStreamHandle {}
#[cfg(target_os = "macos")]
unsafe impl Sync for AppleStreamHandle {}

// -----------------------------------------------------------------
// Factory
// -----------------------------------------------------------------

/// Build an Apple-native `AudioDevices`.
///
/// Cheap construction — no I/O at this point. Enumeration happens
/// lazily on each `list_devices()` / `default_input()` call so the
/// snapshot is always fresh.
pub fn new() -> impl AudioDevices {
    AppleAudioDevices
}

struct AppleAudioDevices;

// -----------------------------------------------------------------
// iOS implementation
// -----------------------------------------------------------------

#[cfg(target_os = "ios")]
impl AudioDevices for AppleAudioDevices {
    fn list_devices(&self) -> Vec<InputDevice> {
        match build_ios_device() {
            Some(d) => vec![d],
            None => Vec::new(),
        }
    }

    fn default_input(&self) -> Option<InputStream> {
        // Single-stream device: pop the one stream.
        build_ios_device()?.streams.into_iter().next()
    }
}

#[cfg(target_os = "ios")]
fn build_ios_device() -> Option<InputDevice> {
    use objc2_avf_audio::AVAudioSession;

    // SAFETY: sharedInstance() returns the process-wide singleton; both
    // property reads are documented as safe for concurrent access.
    let session = unsafe { AVAudioSession::sharedInstance() };

    // inputNumberOfChannels() returns 0 if no input is routed yet
    // (session not activated or category ≠ Record). Clamp to at least 1 so
    // we still return a device — the caller will activate the session via the
    // driver before actually opening a stream.
    // NSInteger is isize on all Apple targets (32-bit: i32, 64-bit: i64).
    // Cast via isize to be target-width agnostic before clamping to u16.
    let channels_raw = unsafe { session.inputNumberOfChannels() } as isize;
    let channels: u16 = if channels_raw >= 1 {
        (channels_raw as usize).min(u16::MAX as usize) as u16
    } else {
        1 // default — actual channel count confirmed at capture-open time
    };

    let sample_rate = unsafe { session.sampleRate() };
    // sampleRate() returns 0.0 before the session is activated. Still build
    // a device entry — the port contract allows ProbeOnly when the platform
    // can't enumerate (iOS AVAudioSession fits that model exactly).
    let _ = sample_rate; // used as contextual comment; ProbeOnly is authoritative

    let handle = StreamHandle(Arc::new(AppleStreamHandle));

    let stream = InputStream {
        handle,
        name: "Built-in microphone".to_string(),
        channels,
        sample_rates: SampleRateSupport::ProbeOnly,
    };

    Some(InputDevice {
        persistent_id: None, // iOS doesn't vend stable input UIDs
        name: "Built-in microphone".to_string(),
        transport: Transport::BuiltIn,
        streams: vec![stream],
    })
}

// -----------------------------------------------------------------
// macOS implementation
// -----------------------------------------------------------------

#[cfg(target_os = "macos")]
impl AudioDevices for AppleAudioDevices {
    fn list_devices(&self) -> Vec<InputDevice> {
        match build_macos_device() {
            Some(d) => vec![d],
            None => Vec::new(),
        }
    }

    fn default_input(&self) -> Option<InputStream> {
        build_macos_device()?.streams.into_iter().next()
    }
}

#[cfg(target_os = "macos")]
fn build_macos_device() -> Option<InputDevice> {
    use coreaudio::audio_unit::macos_helpers;

    // None = no input device present (e.g. a headless Mac or CI box).
    // The port contract tolerates list_devices() == [] and default_input() == None.
    let device_id = macos_helpers::get_default_device_id(/*input=*/ true)?;

    let name =
        macos_helpers::get_device_name(device_id).unwrap_or_else(|_| "Default input".to_string());

    // Sample-rate ranges from CoreAudio physical stream format query.
    let sample_rates = build_sample_rate_support(device_id);

    // Channel count from the default physical format.
    let channels = channels_for_device(device_id);

    let handle = StreamHandle(Arc::new(AppleStreamHandle { device_id }));

    let stream = InputStream {
        handle,
        name: name.clone(),
        channels,
        sample_rates,
    };

    Some(InputDevice {
        persistent_id: None, // stable UID available via kAudioDevicePropertyDeviceUID;
        // deferred — Phase C doesn't need it and neither does
        // the conformance battery.
        name,
        transport: Transport::Unknown, // transport-type query deferred to Phase C if needed
        streams: vec![stream],
    })
}

/// Derive sample-rate support from CoreAudio's available physical formats.
/// Falls back to `ProbeOnly` if the query fails (e.g. no streams yet).
#[cfg(target_os = "macos")]
fn build_sample_rate_support(device_id: u32) -> SampleRateSupport {
    use coreaudio::audio_unit::macos_helpers;

    let formats = match macos_helpers::get_supported_physical_stream_formats(device_id) {
        Ok(f) if !f.is_empty() => f,
        _ => return SampleRateSupport::ProbeOnly,
    };

    let ranges: Vec<(u32, u32)> = formats
        .iter()
        .map(|f| {
            (
                f.mSampleRateRange.mMinimum as u32,
                f.mSampleRateRange.mMaximum as u32,
            )
        })
        // Deduplicate identical ranges (CoreAudio sometimes lists the same
        // range for each channel-count variant of a format).
        .fold(Vec::new(), |mut acc, r| {
            if !acc.contains(&r) {
                acc.push(r);
            }
            acc
        });

    if ranges.is_empty() {
        SampleRateSupport::ProbeOnly
    } else {
        SampleRateSupport::Ranges(ranges)
    }
}

/// Read the channel count from the default physical stream format.
/// Falls back to 1 if the query fails.
#[cfg(target_os = "macos")]
fn channels_for_device(device_id: u32) -> u16 {
    use coreaudio::audio_unit::macos_helpers;

    let formats = match macos_helpers::get_supported_physical_stream_formats(device_id) {
        Ok(f) if !f.is_empty() => f,
        _ => return 1,
    };

    // The first format entry's channel count is a reasonable default.
    let ch = formats[0].mFormat.mChannelsPerFrame;
    if ch >= 1 {
        ch.min(u16::MAX as u32) as u16
    } else {
        1
    }
}
