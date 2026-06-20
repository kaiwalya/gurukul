//! Conformance test: runs the full audio-port battery against the WAV adapter.
//!
//! The WAV adapter is deterministic and always has a device, so it exercises
//! ALL four battery functions including `verify_capture_delivery`. This is the
//! primary proof that the battery is sound.

use adapter_audio_wav::{new_capture, new_devices};
use domain_ports::audio_capture::conformance as cap_conformance;
use domain_ports::audio_devices::conformance as dev_conformance;
use domain_ports::clock::TestClock;
use std::sync::Arc;

/// Write a minimal valid WAV to a tempdir and return the path.
fn write_test_wav(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("conformance.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    // 5 seconds worth of non-silent samples so delivery test has plenty of frames
    for i in 0..(48_000 * 5) {
        writer
            .write_sample((i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5)
            .unwrap();
    }
    writer.finalize().unwrap();
    path
}

#[test]
fn wav_adapter_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_test_wav(dir.path());

    let devices = new_devices(path.clone());
    let clock = Arc::new(TestClock::default());
    let capture = new_capture(clock as Arc<dyn domain_ports::clock::Clock>);

    dev_conformance::verify_devices_contract(&devices);
    cap_conformance::verify_handle_rejection(&capture);
    cap_conformance::verify_negotiate_contract(&devices, &capture);
    cap_conformance::verify_capture_delivery(&devices, &capture);
}
