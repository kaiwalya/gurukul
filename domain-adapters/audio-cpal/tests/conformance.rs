//! Conformance test: runs the audio-port battery against the cpal adapter.
//!
//! cpal may have zero input devices on a headless CI box. The test is split:
//! - Unconditional: `verify_devices_contract` and `verify_handle_rejection`
//!   run real code with no mic required.
//! - Device-gated: `verify_negotiate_contract` and `verify_capture_delivery`
//!   run only when `devices.default_input().is_some()`; otherwise an
//!   `eprintln!` skip message is printed (the battery functions also guard
//!   internally, but the caller gates to keep intent obvious).

use adapter_audio_cpal::{new_capture, new_devices};
use domain_ports::audio_capture::conformance as cap_conformance;
use domain_ports::audio_capture::AudioCapture;
use domain_ports::audio_devices::conformance as dev_conformance;
use domain_ports::audio_devices::AudioDevices;
use domain_ports::clock::TestClock;
use std::sync::Arc;

#[test]
fn cpal_adapter_conformance() {
    let devices = new_devices();
    let clock = Arc::new(TestClock::default());
    let capture = new_capture(clock as Arc<dyn domain_ports::clock::Clock>);

    // Unconditional — always run real assertions, no mic required.
    dev_conformance::verify_devices_contract(&devices);
    cap_conformance::verify_handle_rejection(&capture);

    // Device-gated — skip if no mic is present.
    if devices.default_input().is_some() {
        cap_conformance::verify_negotiate_contract(&devices, &capture);
        cap_conformance::verify_capture_delivery(&devices, &capture);
    } else {
        eprintln!(
            "[conformance] cpal: no default input device — \
             skipping verify_negotiate_contract and verify_capture_delivery"
        );
    }
}

/// Live-smoke tier: asserts the default mic actually delivers non-silent frames.
///
/// Run explicitly on a machine with a working mic:
///   cargo test --release -p adapter-audio-cpal -- --ignored live_mic
///
/// NOT in the CI path. This is the tier that would have caught
/// "cpal never enables the iOS input bus."
#[test]
#[ignore]
fn live_mic_delivers_frames() {
    let devices = new_devices();
    let stream = match devices.default_input() {
        Some(s) => s,
        None => {
            eprintln!("[live_mic] no default input device — test inconclusive");
            return;
        }
    };

    let clock = Arc::new(TestClock::default());
    let capture = new_capture(clock as Arc<dyn domain_ports::clock::Clock>);

    // Run the full delivery battery first (geometry, bounds, RAII-stop, reuse)
    cap_conformance::verify_capture_delivery(&devices, &capture);

    // Additionally assert non-silence: the real mic must deliver at least one
    // non-zero sample — a silent-room mic still satisfies bounds, so non-silence
    // distinguishes "input bus actually came up" from "callback fires with zeros."
    let sample_rate = {
        use domain_ports::audio_devices::SampleRateSupport;
        match &stream.sample_rates {
            SampleRateSupport::List(r) => r.first().copied().unwrap_or(48_000),
            SampleRateSupport::Ranges(r) => r.first().map(|(lo, _)| *lo).unwrap_or(48_000),
            SampleRateSupport::ProbeOnly => 48_000,
        }
    };

    let wanted = domain_ports::audio_capture::CaptureConfig {
        sample_rate,
        channels: stream.channels,
        buffer_frames: None,
    };
    let negotiated = capture
        .negotiate(&stream.handle, &wanted)
        .expect("negotiate must succeed for live-smoke test");

    let saw_nonzero = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_nonzero_cb = Arc::clone(&saw_nonzero);

    let session = capture
        .open(
            stream.handle.clone(),
            negotiated,
            Box::new(move |frame| {
                if frame.samples.iter().any(|&s| s != 0.0) {
                    saw_nonzero_cb.store(true, std::sync::atomic::Ordering::Release);
                }
            }),
            Box::new(|_| {}),
        )
        .expect("open must succeed for live-smoke test");

    // Give the mic up to 3 seconds to produce a non-zero sample
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if saw_nonzero.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            drop(session);
            panic!("live_mic_delivers_frames: mic delivered only silence for 3s — input bus may not be active");
        }
    }
    drop(session);
}
