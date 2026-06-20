//! Conformance battery for the Apple audio adapter.
//!
//! Run with: `cargo test -p adapter-audio-apple --release`
//!
//! Gates per spec:
//! - `apple_adapter_devices_conformance` — universal, passes even with no device.
//! - `handle_rejection` — universal, no device needed.
//! - `negotiate_contract` — needs a default input device.
//! - `capture_delivery` — needs a device that ACTUALLY delivers frames.
//!
//! The live-mic test is `#[ignore]` — run manually with
//! `cargo test -p adapter-audio-apple --release -- --ignored live_mic_delivers_frames`

use adapter_audio_apple::{new_capture, new_devices};
use domain_ports::audio_capture::conformance::{
    verify_capture_delivery, verify_handle_rejection, verify_negotiate_contract,
};
use domain_ports::audio_devices::conformance as dev_conformance;
use domain_ports::clock::Clock;
use std::sync::Arc;

fn make_clock() -> Arc<dyn Clock> {
    Arc::new(adapter_clock_std::new())
}

#[test]
fn apple_adapter_devices_conformance() {
    let devices = new_devices();
    dev_conformance::verify_devices_contract(&devices);
}

#[test]
fn handle_rejection() {
    verify_handle_rejection(&new_capture(make_clock()));
}

#[test]
fn negotiate_contract() {
    verify_negotiate_contract(&new_devices(), &new_capture(make_clock()));
}

#[test]
fn capture_delivery() {
    verify_capture_delivery(&new_devices(), &new_capture(make_clock()));
}

/// Explicit live-mic test for on-device validation.
/// Run with `--ignored` on a real device (phone or Mac with a live mic).
#[test]
#[ignore]
fn live_mic_delivers_frames() {
    verify_capture_delivery(&new_devices(), &new_capture(make_clock()));
}
