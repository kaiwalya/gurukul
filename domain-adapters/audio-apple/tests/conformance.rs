//! Conformance test: runs the devices-port battery against the audio-apple adapter.
//!
//! `verify_devices_contract` is designed to pass even with zero devices (headless
//! CI box, macOS without a mic connected). On the iOS simulator there will always
//! be exactly one device (the Mac's mic routed through the sim).

use adapter_audio_apple::new_devices;
use domain_ports::audio_devices::conformance as dev_conformance;

#[test]
fn apple_adapter_devices_conformance() {
    let devices = new_devices();
    dev_conformance::verify_devices_contract(&devices);
}
