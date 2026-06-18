//! AudioCapture impl backed by a WAV file.
//!
//! `open` reads the entire WAV into memory, spawns a feeder thread that walks
//! the samples in chunk-sized slices and calls the user's callback at real-time
//! pace. When the WAV drains the thread exits with a single `eprintln!` line;
//! the session stays open (the port models a never-ending mic and adding an
//! EOS concept is out of scope for Phase 3).
//!
//! The `CaptureConfig` passed by the host is the sole source of truth for chunk
//! size and pacing math. The WAV header is only used for an upfront validation
//! that it matches `cfg`.

use crate::WavStreamHandle;
use domain_ports::audio_capture::{
    AudioCapture, CaptureCallback, CaptureConfig, CaptureError, CaptureFrame, CaptureSession,
    LifecycleSink,
};
use domain_ports::audio_devices::StreamHandle;
use domain_ports::clock::Clock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Build a WAV-backed AudioCapture. `clock` stamps `t_ms` on every delivered
/// frame. Cheap construction — opens nothing until [`AudioCapture::open`].
pub fn new(clock: Arc<dyn Clock>) -> impl AudioCapture {
    WavAudioCapture { clock }
}

struct WavAudioCapture {
    clock: Arc<dyn Clock>,
}

impl AudioCapture for WavAudioCapture {
    /// Validate `requested` against the WAV header's single supported config.
    ///
    /// WAV has exactly one concrete format (the header's sample_rate and
    /// channels). If the request matches, return that config with
    /// `buffer_frames: None` (the feeder takes chunk geometry from `open`,
    /// not a pre-known hardware buffer). If the request mismatches, or the
    /// header is corrupt (zero rate/channels), return an error so the
    /// control plane learns about the problem before any frames flow.
    fn negotiate(
        &self,
        handle: &StreamHandle,
        requested: &CaptureConfig,
    ) -> Result<CaptureConfig, CaptureError> {
        let wav_handle = handle
            .0
            .downcast_ref::<WavStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        let spec = wav_handle.spec;

        // A zero sample_rate or channel count means the WAV header is corrupt.
        // open()'s feeder divides by sample_rate, so let this through is UB.
        if spec.sample_rate == 0 || spec.channels == 0 {
            return Err(CaptureError::Other(
                "WAV header has zero sample_rate/channels".into(),
            ));
        }

        let header_config = CaptureConfig {
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            buffer_frames: None,
        };

        if requested.sample_rate == header_config.sample_rate
            && requested.channels == header_config.channels
        {
            Ok(header_config)
        } else {
            Err(CaptureError::UnsupportedConfig {
                wanted: requested.clone(),
                actual: Some(header_config),
            })
        }
    }

    fn open(
        &self,
        handle: StreamHandle,
        cfg: CaptureConfig,
        on_frame: CaptureCallback,
        // A WAV file is a never-ending mic: nothing can interrupt it, change
        // its route, or revoke its permission. The sink is accepted to honour
        // the port contract and ignored — no lifecycle event will ever fire.
        _on_event: LifecycleSink,
    ) -> Result<CaptureSession, CaptureError> {
        // Step 1: downcast the handle to our private WavStreamHandle type.
        let wav_handle = handle
            .0
            .downcast_ref::<WavStreamHandle>()
            .ok_or(CaptureError::InvalidHandle)?;

        // open() trusts the negotiated config (negotiate() already vetted
        // it against the WAV header). The only check left is about the WAV
        // *file*, not the config: the sample count must divide evenly by the
        // channel count, else interleaving is undefined.
        let spec = wav_handle.spec;
        let samples = read_wav_samples(&wav_handle.path, spec)?;

        let channels = cfg.channels as usize;
        if channels == 0 || !samples.len().is_multiple_of(channels) {
            return Err(CaptureError::Other(format!(
                "WAV sample count {} is not divisible by channel count {channels}",
                samples.len(),
            )));
        }

        // Step 4: compute chunk geometry from cfg (not the WAV header).
        // The `None` default is a 10ms chunk; the `.max(1)` clamp guards two
        // cases: an absurdly low sample_rate (<100) yielding a 0-frame default
        // chunk, and a caller-supplied `buffer_frames` of 0 — either way one
        // frame minimum keeps `chunks(0)` from panicking.
        let chunk_frames = cfg
            .buffer_frames
            .map(|f| f as usize)
            .unwrap_or((cfg.sample_rate / 100) as usize)
            .max(1);
        let chunk_samples = chunk_frames * channels;

        // Step 5: move samples + callback into a feeder thread.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_feeder = Arc::clone(&stop);
        let clock = Arc::clone(&self.clock);
        let sample_rate = cfg.sample_rate;

        let basename = wav_handle
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| wav_handle.path.display().to_string());

        let handle = thread::Builder::new()
            .name("wav-replay-feeder".into())
            .spawn(move || {
                feeder_loop(
                    samples,
                    chunk_samples,
                    channels,
                    sample_rate,
                    clock,
                    stop_feeder,
                    on_frame,
                    &basename,
                );
            })
            .map_err(|e| CaptureError::Other(format!("spawn feeder thread: {e}")))?;

        // Step 6: return a CaptureSession whose teardown stops and joins the feeder.
        // `join()` blocks the dropping thread (the !Send Bevy thread on
        // --replay-audio) until the feeder observes `stop` — bounded to one
        // ≤2ms sleep step. This relies on `on_frame` never blocking on the
        // dropping thread: today it pushes into a lock-free bounded-drop ring,
        // so a stalled drain drops samples rather than blocking the feeder. If
        // that callback ever takes a lock the dropping thread also holds, this
        // join would deadlock.
        Ok(CaptureSession::new(move || {
            stop.store(true, Ordering::Release);
            let _ = handle.join();
        }))
    }
}

/// The feeder thread body. Walks `samples` in `chunk_samples`-sized slices,
/// calls `on_frame` for each chunk, then sleeps the chunk's real-time duration
/// in interruptible ≤2ms steps.
#[allow(clippy::too_many_arguments)]
fn feeder_loop(
    samples: Vec<f32>,
    chunk_samples: usize,
    channels: usize,
    sample_rate: u32,
    clock: Arc<dyn Clock>,
    stop: Arc<AtomicBool>,
    mut on_frame: CaptureCallback,
    basename: &str,
) {
    let mut total_frames: usize = 0;

    for chunk in samples.chunks(chunk_samples) {
        // Check stop before delivering each chunk.
        if stop.load(Ordering::Acquire) {
            return;
        }

        let frames = chunk.len() / channels;
        on_frame(CaptureFrame {
            samples: chunk,
            frames,
            t_ms: clock.now_ms(),
        });
        total_frames += frames;

        // Pace: sleep the chunk's real duration in ≤2ms interruptible steps.
        let chunk_duration_ms = frames as f64 / sample_rate as f64 * 1000.0;
        let steps = (chunk_duration_ms / 2.0).ceil() as u64;
        let step_ms = if steps > 0 {
            chunk_duration_ms / steps as f64
        } else {
            0.0
        };
        for _ in 0..steps {
            if stop.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_micros((step_ms * 1000.0) as u64));
        }
    }

    let total_secs = total_frames as f64 / sample_rate as f64;
    eprintln!(
        "[adapter-audio-wav] replay drained: {basename} ({total_frames} frames, {total_secs:.2}s)"
    );
    // Thread ends; session stays Running per the port contract.
}

/// Read all samples from a WAV file into a `Vec<f32>`.
/// Supports Float and Int sample formats; Int samples are normalized to [-1, 1].
fn read_wav_samples(
    path: &std::path::Path,
    spec: hound::WavSpec,
) -> Result<Vec<f32>, CaptureError> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| CaptureError::Other(format!("opening WAV {}: {e}", path.display())))?;

    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CaptureError::Other(format!("reading float samples: {e}")))?,
        hound::SampleFormat::Int => {
            // Samples are read as `i32`, so the bit depth must fit. Guard the
            // shift below too: `bits_per_sample` of 0 would underflow and any
            // value >32 can't be represented. hound would eventually error on
            // an out-of-range read, but validating here gives a clear message.
            if !(1..=32).contains(&spec.bits_per_sample) {
                return Err(CaptureError::Other(format!(
                    "unsupported integer bit depth {}; expected 1..=32",
                    spec.bits_per_sample
                )));
            }
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| CaptureError::Other(format!("reading int samples: {e}")))?
        }
    };

    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices;
    use domain_ports::audio_capture::AudioCapture;
    use domain_ports::audio_devices::{AudioDevices, StreamHandle};
    use domain_ports::clock::TestClock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn write_test_wav(
        dir: &std::path::Path,
        sample_rate: u32,
        channels: u16,
        n_frames: usize,
    ) -> std::path::PathBuf {
        let path = dir.join("test.wav");
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..(n_frames * channels as usize) {
            writer.write_sample(i as f32 / 1000.0).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_devices_report_wav_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_wav(dir.path(), 48000, 1, 480);

        let devs = devices::new(path);
        let list = devs.list_devices();
        assert_eq!(list.len(), 1);
        let stream = &list[0].streams[0];
        assert_eq!(stream.channels, 1);
        match &stream.sample_rates {
            domain_ports::audio_devices::SampleRateSupport::List(rates) => {
                assert!(rates.contains(&48000));
            }
            _ => panic!("expected List sample rates"),
        }
        assert!(devs.default_input().is_some());
    }

    #[test]
    fn test_capture_feeds_all_samples() {
        let dir = tempfile::tempdir().unwrap();
        let n_frames = 480usize;
        let path = write_test_wav(dir.path(), 48000, 1, n_frames);

        let clock = Arc::new(TestClock::default());
        let devs = devices::new(path);
        let stream = devs.default_input().unwrap();
        let handle = stream.handle.clone();

        let capture = new(clock as Arc<dyn Clock>);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let cfg = domain_ports::audio_capture::CaptureConfig {
            sample_rate: 48000,
            channels: 1,
            buffer_frames: Some(480),
        };

        let _session = capture
            .open(
                handle,
                cfg,
                Box::new(move |frame| {
                    counter_clone.fetch_add(frame.frames, Ordering::SeqCst);
                }),
                Box::new(|_| {}),
            )
            .unwrap();

        // Short WAV drains nearly instantly; sleep a bit to let the feeder finish.
        std::thread::sleep(Duration::from_millis(200));
        drop(_session);

        assert_eq!(counter.load(Ordering::SeqCst), n_frames);
    }

    #[test]
    fn test_wrong_handle_returns_invalid_handle() {
        let dir = tempfile::tempdir().unwrap();
        let _path = write_test_wav(dir.path(), 48000, 1, 480);

        let clock = Arc::new(TestClock::default());
        let capture = new(clock as Arc<dyn Clock>);

        // A foreign handle (unit type inside)
        let foreign_handle = StreamHandle(Arc::new(()));
        let cfg = domain_ports::audio_capture::CaptureConfig {
            sample_rate: 48000,
            channels: 1,
            buffer_frames: Some(480),
        };

        let result = capture.open(foreign_handle, cfg, Box::new(|_| {}), Box::new(|_| {}));
        assert!(matches!(result, Err(CaptureError::InvalidHandle)));
    }

    #[test]
    fn test_config_mismatch_returns_unsupported_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_wav(dir.path(), 48000, 1, 480);

        let clock = Arc::new(TestClock::default());
        let devs = devices::new(path);
        let stream = devs.default_input().unwrap();
        // Keep handle live for the borrow; negotiate takes &StreamHandle.
        let handle = stream.handle.clone();

        let capture = new(clock as Arc<dyn Clock>);

        // Mismatched sample rate: negotiate() owns format-mismatch detection.
        // It must return Err(UnsupportedConfig) with the header as `actual`.
        let mismatched_cfg = domain_ports::audio_capture::CaptureConfig {
            sample_rate: 44100, // WAV is 48000
            channels: 1,
            buffer_frames: Some(441),
        };
        let result = capture.negotiate(&handle, &mismatched_cfg);
        match result {
            Err(CaptureError::UnsupportedConfig { wanted, actual }) => {
                assert_eq!(
                    wanted.sample_rate, 44100,
                    "wanted must be the requested rate"
                );
                let actual =
                    actual.expect("WAV has a concrete single config — actual must be Some");
                assert_eq!(
                    actual.sample_rate, 48000,
                    "actual must be the WAV header rate"
                );
                assert_eq!(actual.channels, 1, "actual must be the WAV header channels");
            }
            other => panic!("expected UnsupportedConfig, got {other:?}"),
        }

        // Matching request: negotiate() must succeed and return the header config.
        let matching_cfg = domain_ports::audio_capture::CaptureConfig {
            sample_rate: 48000,
            channels: 1,
            buffer_frames: Some(480),
        };
        let negotiated = capture
            .negotiate(&handle, &matching_cfg)
            .expect("matching request must succeed");
        assert_eq!(
            negotiated.sample_rate, 48000,
            "negotiated rate must match header"
        );
        assert_eq!(
            negotiated.channels, 1,
            "negotiated channels must match header"
        );
        assert_eq!(negotiated.buffer_frames, None, "WAV has no hardware buffer");
    }

    /// Write a 16-bit integer-PCM mono WAV with the given raw sample values.
    fn write_int16_wav(
        dir: &std::path::Path,
        sample_rate: u32,
        samples: &[i32],
    ) -> std::path::PathBuf {
        let path = dir.join("int.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &s in samples {
            writer.write_sample(s as i16).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_int_pcm_normalizes_to_unit_range() {
        // Full-scale 16-bit values map into [-1, 1): -32768 -> -1.0, 0 -> 0.0,
        // 16384 -> 0.5. read_wav_samples divides by 2^(bits-1) = 32768.
        let dir = tempfile::tempdir().unwrap();
        let path = write_int16_wav(dir.path(), 48000, &[-32768, 0, 16384]);
        let spec = hound::WavReader::open(&path).unwrap().spec();

        let samples = read_wav_samples(&path, spec).unwrap();
        assert_eq!(samples.len(), 3);
        assert!((samples[0] - -1.0).abs() < 1e-6, "got {}", samples[0]);
        assert!(samples[1].abs() < 1e-6, "got {}", samples[1]);
        assert!((samples[2] - 0.5).abs() < 1e-6, "got {}", samples[2]);
    }

    #[test]
    fn test_low_sample_rate_default_buffer_does_not_panic() {
        // sample_rate < 100 with buffer_frames: None would make the default
        // chunk `sample_rate / 100 == 0` and panic in chunks(0); the .max(1)
        // clamp turns it into a 1-frame chunk instead. 8 frames must all flow.
        let dir = tempfile::tempdir().unwrap();
        let n_frames = 8usize;
        let path = write_test_wav(dir.path(), 50, 1, n_frames);

        let clock = Arc::new(TestClock::default());
        let devs = devices::new(path);
        let handle = devs.default_input().unwrap().handle.clone();
        let capture = new(clock as Arc<dyn Clock>);

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let cfg = domain_ports::audio_capture::CaptureConfig {
            sample_rate: 50,
            channels: 1,
            buffer_frames: None, // <- forces the default-chunk path
        };

        let session = capture
            .open(
                handle,
                cfg,
                Box::new(move |frame| {
                    counter_clone.fetch_add(frame.frames, Ordering::SeqCst);
                }),
                Box::new(|_| {}),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        drop(session);

        assert_eq!(counter.load(Ordering::SeqCst), n_frames);
    }
}
