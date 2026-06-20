//! AudioCapture implementation for Apple platforms via native CoreAudio.
//!
//! Builds the input AudioUnit directly using coreaudio-rs instead of cpal's
//! `build_input_stream` — the fix for `DeviceNotAvailable` on device.
//!
//! # Call sequence (input capture, all before initialize)
//!
//! iOS:  `AudioUnit::new(IOType::RemoteIO)`
//! macOS: `audio_unit_from_device_id(device_id, true)` (create + CurrentDevice)
//! Both: EnableIO input=1 (Scope::Input, Element::Input)
//!       DisableIO output=0 (Scope::Output, Element::Output)
//!       set_stream_format IS_FLOAT|IS_PACKED interleaved on (Scope::Output, Element::Input)
//!       set_input_callback → read args.data.buffer, stamp t_ms, call on_frame, Ok(())
//!       initialize()
//!       start()

use crate::devices::AppleStreamHandle;
use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::render_callback::data;
use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope, StreamFormat};
use domain_ports::audio_capture::{
    AudioCapture, CaptureCallback, CaptureConfig, CaptureError, CaptureFrame, CaptureSession,
    LifecycleSink,
};
use domain_ports::audio_devices::StreamHandle;
use domain_ports::clock::Clock;
use objc2_audio_toolbox::kAudioOutputUnitProperty_EnableIO;
use std::sync::Arc;

/// Build an Apple-native `AudioCapture`. The `clock` stamps `t_ms` on
/// every delivered frame. Cheap construction — opens nothing until
/// [`AudioCapture::open`].
pub fn new(clock: Arc<dyn Clock>) -> impl AudioCapture {
    AppleAudioCapture { clock }
}

struct AppleAudioCapture {
    clock: Arc<dyn Clock>,
}

impl AudioCapture for AppleAudioCapture {
    fn negotiate(
        &self,
        handle: &StreamHandle,
        requested: &CaptureConfig,
    ) -> Result<CaptureConfig, CaptureError> {
        handle
            .0
            .downcast_ref::<AppleStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        // Reject absurd configs early.
        if requested.channels == 0 || requested.channels > 64 {
            return Err(CaptureError::UnsupportedConfig {
                wanted: requested.clone(),
                actual: None,
            });
        }
        if requested.sample_rate == 0 {
            return Err(CaptureError::UnsupportedConfig {
                wanted: requested.clone(),
                actual: None,
            });
        }

        negotiate_impl(requested)
    }

    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        on_frame: CaptureCallback,
        _on_event: LifecycleSink,
    ) -> Result<CaptureSession, CaptureError> {
        let apple = handle
            .0
            .downcast_ref::<AppleStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        // Reject absurd configs (guard against callers that bypass negotiate).
        if cfg.channels == 0 || cfg.channels > 64 {
            return Err(CaptureError::UnsupportedConfig {
                wanted: cfg.clone(),
                actual: None,
            });
        }
        if cfg.sample_rate == 0 {
            return Err(CaptureError::UnsupportedConfig {
                wanted: cfg.clone(),
                actual: None,
            });
        }

        let clock = Arc::clone(&self.clock);
        let audio_unit = build_input_unit(apple, &cfg, on_frame, clock)?;

        Ok(CaptureSession::new(move || {
            // coreaudio-rs Drop does stop+uninitialize+dispose.
            drop(audio_unit);
        }))
    }
}

// -----------------------------------------------------------------
// negotiate: read back the live AVAudioSession rate (iOS) or accept
// the requested rate (macOS — AudioUnit handles it).
// -----------------------------------------------------------------

#[cfg(target_os = "ios")]
fn negotiate_impl(requested: &CaptureConfig) -> Result<CaptureConfig, CaptureError> {
    use objc2_avf_audio::AVAudioSession;

    let session = unsafe { AVAudioSession::sharedInstance() };
    // Set preferred rate; ignore error — we read back the actual.
    let _ = unsafe { session.setPreferredSampleRate_error(requested.sample_rate as f64) };
    let actual_rate = unsafe { session.sampleRate() };
    let sample_rate = if actual_rate > 0.0 {
        actual_rate.round() as u32
    } else {
        requested.sample_rate
    };

    // iOS exposes the live channel count via the session.
    let channels_raw = unsafe { session.inputNumberOfChannels() } as isize;
    let channels = if channels_raw >= 1 {
        (channels_raw as u16).min(requested.channels)
    } else {
        requested.channels
    };

    Ok(CaptureConfig {
        sample_rate,
        channels,
        buffer_frames: requested.buffer_frames,
    })
}

#[cfg(target_os = "macos")]
fn negotiate_impl(requested: &CaptureConfig) -> Result<CaptureConfig, CaptureError> {
    // macOS: accept the requested config; the AudioUnit will adapt to the
    // default input device's actual rate at initialize() time.
    Ok(CaptureConfig {
        sample_rate: requested.sample_rate,
        channels: requested.channels,
        buffer_frames: requested.buffer_frames,
    })
}

// -----------------------------------------------------------------
// build_input_unit: the spec-ordered call sequence.
// Steps are numbered to match the spec's invariant list.
// -----------------------------------------------------------------

fn build_input_unit(
    handle: &AppleStreamHandle,
    cfg: &CaptureConfig,
    mut on_frame: CaptureCallback,
    clock: Arc<dyn Clock>,
) -> Result<AudioUnit, CaptureError> {
    // Step 1: Create the AudioUnit (platform-specific).
    let mut au = create_audio_unit(handle)?;

    // Uninitialize before configuring: on iOS, RemoteIO is born already
    // initialized (coreaudio-rs calls AudioUnitInitialize inside `new`).
    // EnableIO and stream-format properties cannot be set on an initialized
    // unit — they return kAudioUnitErr_Initialized (-10851). Uninitializing
    // is harmless on macOS (no-op on a freshly-created unit) and required
    // on iOS. This matches the canonical CoreAudio pattern:
    // uninitialize → configure → initialize.
    au.uninitialize()
        .map_err(|e| CaptureError::DeviceUnavailable {
            reason: format!("uninitialize: {e}"),
        })?;

    // Step 2: EnableIO input=1 on (Scope::Input, Element::Input = bus 1).
    au.set_property(
        kAudioOutputUnitProperty_EnableIO,
        Scope::Input,
        Element::Input,
        Some(&1u32),
    )
    .map_err(|e| CaptureError::DeviceUnavailable {
        reason: format!("EnableIO input: {e}"),
    })?;

    // Step 3: DisableIO output=0 on (Scope::Output, Element::Output = bus 0).
    au.set_property(
        kAudioOutputUnitProperty_EnableIO,
        Scope::Output,
        Element::Output,
        Some(&0u32),
    )
    .map_err(|e| CaptureError::DeviceUnavailable {
        reason: format!("DisableIO output: {e}"),
    })?;

    // Step 5: stream format — interleaved F32 (IS_FLOAT|IS_PACKED).
    // Scope::Output, Element::Input = the input bus output scope.
    // NOT IS_NON_INTERLEAVED — interleaved matches CaptureFrame.samples directly.
    let stream_format = StreamFormat {
        sample_rate: cfg.sample_rate as f64,
        sample_format: SampleFormat::F32,
        flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
        channels: cfg.channels as u32,
    };
    au.set_stream_format(stream_format, Scope::Output, Element::Input)
        .map_err(|_e| CaptureError::UnsupportedConfig {
            wanted: cfg.clone(),
            actual: Some(CaptureConfig {
                sample_rate: cfg.sample_rate,
                channels: cfg.channels,
                buffer_frames: cfg.buffer_frames,
            }),
        })?;

    // Step 6: set_input_callback.
    // Closure bound: FnMut(Args<D>) -> Result<(), ()> + 'static.
    // RT callback: read slice, stamp t_ms, call on_frame. No alloc/locks.
    au.set_input_callback(
        move |args: coreaudio::audio_unit::render_callback::Args<data::Interleaved<f32>>| {
            let buf: &[f32] = args.data.buffer;
            let num_frames = args.num_frames;
            let t_ms = clock.now_ms();
            on_frame(CaptureFrame {
                samples: buf,
                frames: num_frames,
                t_ms,
            });
            Ok(())
        },
    )
    .map_err(|e| CaptureError::DeviceUnavailable {
        reason: format!("set_input_callback: {e}"),
    })?;

    // Step 7: initialize() — AFTER all EnableIO + format + callback.
    au.initialize()
        .map_err(|e| CaptureError::DeviceUnavailable {
            reason: format!("initialize: {e}"),
        })?;

    // Step 8: start().
    au.start().map_err(|e| CaptureError::DeviceUnavailable {
        reason: format!("start: {e}"),
    })?;

    Ok(au)
}

// -----------------------------------------------------------------
// Platform-specific AudioUnit creation
// -----------------------------------------------------------------

#[cfg(target_os = "ios")]
fn create_audio_unit(_handle: &AppleStreamHandle) -> Result<AudioUnit, CaptureError> {
    use coreaudio::audio_unit::IOType;
    AudioUnit::new(IOType::RemoteIO).map_err(|e| CaptureError::DeviceUnavailable {
        reason: format!("AudioUnit::new(RemoteIO): {e}"),
    })
}

#[cfg(target_os = "macos")]
fn create_audio_unit(handle: &AppleStreamHandle) -> Result<AudioUnit, CaptureError> {
    use coreaudio::audio_unit::macos_helpers;
    // audio_unit_from_device_id: create AudioUnit + set CurrentDevice + enable I/O
    // for the given device. We still explicitly set EnableIO below for clarity.
    macos_helpers::audio_unit_from_device_id(handle.device_id, /*input=*/ true).map_err(|e| {
        CaptureError::DeviceUnavailable {
            reason: format!("audio_unit_from_device_id: {e}"),
        }
    })
}
