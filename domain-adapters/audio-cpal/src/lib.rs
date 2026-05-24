//! adapter-audio-cpal: AudioDevices and AudioCapture ports backed by
//! the [`cpal`] crate.
//!
//! Targets macOS (CoreAudio), Windows (WASAPI), and Linux
//! (ALSA / JACK) through one Rust dependency. cpal abstracts away
//! the per-platform enumeration APIs and gives us a uniform iterator
//! of `Device`s, each with a list of supported input configs.
//!
//! # Crates / ports
//!
//! Two ports live in this one adapter crate because they share the
//! same backing tech (cpal) and an internal type — [`CpalStreamHandle`]
//! — that `AudioCapture` downcasts out of a [`StreamHandle`] vended
//! by `AudioDevices`. Splitting them into separate crates would
//! force that shared type into either `domain-ports` (wrong place)
//! or a third crate (excess ceremony).
//!
//! - [`new_devices`] — AudioDevices impl. Enumeration only.
//! - [`new_capture`] — AudioCapture impl. Opens a stream, delivers
//!   `f32` PCM frames to a callback on cpal's audio thread.
//!
//! # Mapping notes
//!
//! cpal's model is "Device with many supported configs," which is
//! one level shallower than our port's Device → Streams split. We
//! flatten as follows: each cpal `Device` becomes one
//! [`InputDevice`] containing **a single** [`InputStream`]. This is
//! imprecise for multi-stream pro-audio interfaces on macOS
//! (Scarlett 18i20 reports as one device with N channels rather than
//! N streams). The port shape is honest; this adapter is honest
//! about being approximate. A future `adapter-audio-devices-mac`
//! built on `coreaudio-rs` would split properly.
//!
//! cpal does not surface CoreAudio's transport-type property or
//! Windows' EndpointFormFactor, so every device reports
//! [`Transport::Unknown`]. Sample-rate support is expressed as
//! ranges (cpal's `SupportedStreamConfigRange` carries
//! `min_sample_rate` / `max_sample_rate`).
//!
//! Persistent IDs: cpal 0.17 exposes `Device::id()` as a string-like
//! identifier "stable across runs, disconnections, reboots where
//! possible." We pass it through verbatim.

mod capture;
mod devices;

pub use capture::new as new_capture;
pub use devices::new as new_devices;

/// Inner payload of a [`domain_ports::audio_devices::StreamHandle`]
/// vended by this adapter. The capture impl downcasts through the
/// handle's `Arc<dyn Any>` to recover the cpal `Device` and start
/// streaming.
pub(crate) struct CpalStreamHandle {
    pub(crate) device: cpal::Device,
}
