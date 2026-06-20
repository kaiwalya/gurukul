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
//!   via raw CoreAudio `AudioObjectGetPropertyData` calls (objc2-core-audio).
//!   Returns an empty list / `None` if no input device is present (headless CI,
//!   Mac without a mic). The `AppleStreamHandle` carries the `AudioDeviceID`
//!   (u32) that capture's `open()` downcasts to build the HALOutput unit.
//!
//! # AppleStreamHandle
//!
//! A private struct stashed inside the opaque `StreamHandle(Arc<dyn Any + ...>)`.
//! `open()` downcasts back to this to retrieve the platform info
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
/// capture's `open()` can build the HALOutput unit for that device.
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
    let device_id = get_default_input_device_id()?;
    let name = "Default input".to_string();
    let sample_rates = build_sample_rate_support(device_id);
    let channels = channels_for_device(device_id);
    let handle = StreamHandle(Arc::new(AppleStreamHandle { device_id }));
    let stream = InputStream {
        handle,
        name: name.clone(),
        channels,
        sample_rates,
    };
    Some(InputDevice {
        persistent_id: None,
        name,
        transport: Transport::Unknown,
        streams: vec![stream],
    })
}

#[cfg(target_os = "macos")]
fn get_default_input_device_id() -> Option<u32> {
    use objc2_core_audio::{
        kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal,
        kAudioObjectSystemObject, AudioObjectGetPropertyData, AudioObjectPropertyAddress,
    };
    use std::os::raw::c_void;
    use std::ptr::NonNull;

    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: 0,
    };
    let mut device_id: u32 = 0u32;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as u32,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::new(&mut size).unwrap(),
            NonNull::new(&mut device_id as *mut u32 as *mut c_void).unwrap(),
        )
    };
    if status != 0 || device_id == 0 {
        None
    } else {
        Some(device_id)
    }
}

/// Derive sample-rate support from CoreAudio's available nominal sample rates.
/// Falls back to `ProbeOnly` if the query fails (e.g. no streams yet).
#[cfg(target_os = "macos")]
fn build_sample_rate_support(device_id: u32) -> SampleRateSupport {
    use objc2_core_audio::{
        kAudioDevicePropertyAvailableNominalSampleRates, kAudioObjectPropertyScopeInput,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    };
    use objc2_core_audio_types::AudioValueRange;
    use std::os::raw::c_void;
    use std::ptr::NonNull;

    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyAvailableNominalSampleRates,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: 0,
    };

    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device_id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::new(&mut size).unwrap(),
        )
    };
    if status != 0 || size == 0 {
        return SampleRateSupport::ProbeOnly;
    }

    let count = size as usize / std::mem::size_of::<AudioValueRange>();
    if count == 0 {
        return SampleRateSupport::ProbeOnly;
    }

    let mut ranges_raw: Vec<AudioValueRange> = vec![
        AudioValueRange {
            mMinimum: 0.0,
            mMaximum: 0.0
        };
        count
    ];
    let mut size2 = size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::new(&mut size2).unwrap(),
            NonNull::new(ranges_raw.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status != 0 {
        return SampleRateSupport::ProbeOnly;
    }

    let ranges: Vec<(u32, u32)> = ranges_raw
        .iter()
        .map(|r| (r.mMinimum as u32, r.mMaximum as u32))
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

/// Read the channel count from the stream configuration.
/// Falls back to 1 if the query fails.
#[cfg(target_os = "macos")]
fn channels_for_device(device_id: u32) -> u16 {
    use objc2_core_audio::{
        kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    };
    use objc2_core_audio_types::AudioBufferList;
    use std::os::raw::c_void;
    use std::ptr::NonNull;

    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: 0,
    };

    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device_id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::new(&mut size).unwrap(),
        )
    };
    if status != 0 || size < std::mem::size_of::<AudioBufferList>() as u32 {
        return 1;
    }

    let mut buf: Vec<u8> = vec![0u8; size as usize];
    let mut size2 = size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::new(&mut size2).unwrap(),
            NonNull::new(buf.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status != 0 {
        return 1;
    }

    let abl = unsafe { &*(buf.as_ptr() as *const AudioBufferList) };
    let n_bufs = abl.mNumberBuffers as usize;
    if n_bufs == 0 {
        return 1;
    }
    let bufs_ptr = abl.mBuffers.as_ptr();
    let total_channels: u32 = (0..n_bufs)
        .map(|i| unsafe { (*bufs_ptr.add(i)).mNumberChannels })
        .sum();
    (total_channels.max(1) as usize).min(u16::MAX as usize) as u16
}
