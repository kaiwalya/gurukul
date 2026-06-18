//! AudioCapture impl backed by cpal.
//!
//! Uses cpal's `build_input_stream` for F32 PCM. The cpal callback
//! converts to a [`CaptureFrame`], stamps `t_ms` from the adapter's
//! clock, and forwards to the user's callback.
//!
//! ## What about non-F32 devices?
//!
//! Some devices' native format is I16 or U16. cpal's
//! `build_input_stream::<f32>` succeeds anyway on every backend we
//! care about — cpal performs the conversion internally when the
//! requested sample format differs from the device's native format.
//! This costs a copy and a conversion per frame; for a singing
//! coach at 48kHz mono it's negligible. A future native adapter
//! could skip the conversion.

use crate::CpalStreamHandle;
use cpal::traits::{DeviceTrait, StreamTrait};
use domain_ports::audio_capture::{
    AudioCapture, CaptureCallback, CaptureConfig, CaptureError, CaptureFrame, CaptureSession,
    LifecycleEvent, LifecycleSink,
};
use domain_ports::audio_devices::StreamHandle;
use domain_ports::clock::Clock;
use std::sync::Arc;

/// Build a cpal-backed AudioCapture. The `clock` stamps `t_ms` on
/// every delivered frame. Cheap construction — opens nothing until
/// [`AudioCapture::open`].
pub fn new(clock: Arc<dyn Clock>) -> impl AudioCapture {
    CpalAudioCapture { clock }
}

struct CpalAudioCapture {
    clock: Arc<dyn Clock>,
}

impl AudioCapture for CpalAudioCapture {
    fn negotiate(
        &self,
        handle: &StreamHandle,
        requested: &CaptureConfig,
    ) -> Result<CaptureConfig, CaptureError> {
        let cpal_handle = handle
            .0
            .downcast_ref::<CpalStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        // Re-query the device's supported input configs. Match against the
        // requested channel count and check whether the requested sample rate
        // falls in any supported range.
        let configs = cpal_handle.device.supported_input_configs().map_err(|e| {
            CaptureError::DeviceUnavailable {
                reason: format!("supported_input_configs: {e}"),
            }
        })?;

        // Walk ALL configs matching the requested channel count. For each one
        // whose sample-rate range covers the request, check whether the buffer
        // range (when known) also covers the requested buffer. A config with
        // SupportedBufferSize::Unknown defers buffer validation to open() and
        // still counts as a match.
        //
        // Do NOT stop on the first rate match whose buffer is out-of-range — a
        // later config entry (same channels, same rate range) might have Unknown
        // or a wider buffer range. Only after exhausting all channel-matching
        // configs without any full match is UnsupportedConfig returned.
        //
        // The device exposes ranges, not single configs, so a mismatch has no
        // single `actual` CaptureConfig to report — `actual: None` is the
        // honest answer here.
        for cfg in configs {
            if cfg.channels() != requested.channels {
                continue;
            }
            let lo = cfg.min_sample_rate();
            let hi = cfg.max_sample_rate();
            if !(lo..=hi).contains(&requested.sample_rate) {
                continue;
            }
            // Rate range matches. Check the buffer constraint.
            if let Some(n) = requested.buffer_frames {
                if let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
                    if n < *min || n > *max {
                        // This config's buffer range excludes the request; keep
                        // scanning — another config may accept it.
                        continue;
                    }
                }
                // SupportedBufferSize::Unknown: defer validation to open().
            }
            return Ok(CaptureConfig {
                sample_rate: requested.sample_rate,
                channels: requested.channels,
                buffer_frames: requested.buffer_frames,
            });
        }

        // No supported config matched the requested channels + rate + buffer.
        // This also covers the generic-ProbeOnly case: an empty supported-config
        // list produces no iterations and falls through to this rejection
        // (real ProbeOnly/iOS handling is step 1.6.1b).
        Err(CaptureError::UnsupportedConfig {
            wanted: requested.clone(),
            actual: None,
        })
    }

    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        mut on_frame: CaptureCallback,
        on_event: LifecycleSink,
    ) -> Result<CaptureSession, CaptureError> {
        let cpal_handle = handle
            .0
            .downcast_ref::<CpalStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        // TODO(1.6.2): on iOS, arm the AVAudioSession observers here
        // (interruption / route-change / mediaServicesReset notifications)
        // and route them through `on_event`. cpal does not own the session,
        // so the observer wiring lives alongside the stream. Out of scope for
        // 1.6.1c — the seam is proven on Mac with the fake event source.

        let stream_config = cpal::StreamConfig {
            channels: cfg.channels,
            sample_rate: cfg.sample_rate,
            buffer_size: match cfg.buffer_frames {
                Some(n) => cpal::BufferSize::Fixed(n),
                None => cpal::BufferSize::Default,
            },
        };

        let channels = cfg.channels as usize;
        let clock = Arc::clone(&self.clock);

        let stream = cpal_handle
            .device
            .build_input_stream(
                &stream_config,
                move |samples: &[f32], _info: &cpal::InputCallbackInfo| {
                    let frames = samples.len() / channels.max(1);
                    on_frame(CaptureFrame {
                        samples,
                        frames,
                        t_ms: clock.now_ms(),
                    });
                },
                move |err| {
                    // cpal delivers errors on its own thread. Route them
                    // through the lifecycle sink as a `BackendError` — the
                    // control plane marshals it onto its own thread and
                    // classifies it (today's mid-stream-death dead-end).
                    // `on_event` only enqueues, so calling it from cpal's
                    // error thread is safe.
                    on_event(LifecycleEvent::BackendError {
                        reason: err.to_string(),
                    });
                },
                None,
            )
            .map_err(|e| match e {
                // The format itself is unsupported — this is the SupportedBufferSize::
                // Unknown fallback negotiate() deferred to us. Report it as a format
                // mismatch, not a device error.
                cpal::BuildStreamError::StreamConfigNotSupported => {
                    CaptureError::UnsupportedConfig {
                        wanted: cfg.clone(),
                        actual: None,
                    }
                }
                _ => CaptureError::DeviceUnavailable {
                    reason: format!("build_input_stream: {e}"),
                },
            })?;

        stream.play().map_err(|e| CaptureError::DeviceUnavailable {
            reason: format!("Stream::play: {e}"),
        })?;

        // Teardown closure owns the stream; drop = stop on this thread.
        Ok(CaptureSession::new(move || {
            drop(stream);
        }))
    }
}
