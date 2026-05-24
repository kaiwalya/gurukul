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
    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        mut on_frame: CaptureCallback,
    ) -> Result<CaptureSession, CaptureError> {
        let cpal_handle = handle
            .0
            .downcast_ref::<CpalStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        let stream_config = cpal::StreamConfig {
            channels: cfg.channels,
            sample_rate: cfg.sample_rate,
            buffer_size: cpal::BufferSize::Default,
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
                |err| {
                    // cpal delivers errors on its own thread. There's
                    // no caller-supplied error sink at this layer —
                    // future work could route this through telemetry.
                    eprintln!("[adapter-audio-cpal] stream error: {err}");
                },
                None,
            )
            .map_err(|e| CaptureError::UnsupportedConfig {
                reason: format!("build_input_stream: {e}"),
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
