//! AudioDevices port: enumerate audio input devices and their streams.
//!
//! The port is a **snapshot**. Callers `list_devices()` immediately
//! before they want to act on the result. Hotplug / device-change
//! notifications are intentionally not modelled here — adding them
//! turns enumeration into a long-lived subscription and complicates
//! every adapter. If a future feature needs to react to device
//! changes while a session is *not* running, that becomes its own
//! sub-trait; today, callers re-enumerate on demand.
//!
//! # Model
//!
//! - [`InputDevice`] — the physical thing (built-in mic, AirPods, a
//!   Scarlett interface). Carries a persistent identity *where the
//!   platform supports one* and a list of streams.
//! - [`InputStream`] — what callers actually open and record from.
//!   Has channel count, sample-rate support, and an opaque handle
//!   the (future) capture port consumes.
//!
//! The Device → Stream split exists because pro audio interfaces
//! expose multiple physical inputs as one device with N input
//! streams (Scarlett 18i20). Singing coaches almost always use a
//! single mic on a single-stream device, where the split collapses
//! to `device.streams.len() == 1` — but the port stays honest about
//! multi-stream hardware.
//!
//! # Persistent identity
//!
//! Only [`InputDevice::persistent_id`] is stable across reboot /
//! replug, and only on platforms that natively support it (macOS
//! UID, Windows IMMDevice id). It's `None` on Android and bare ALSA
//! — callers wanting durable identity there must reconstruct from
//! `(transport, name)` and accept ambiguity (two identical AirPods
//! are not distinguishable).
//!
//! [`StreamHandle`] is *session-scoped*: it identifies a stream
//! within the current adapter instance, and must not be persisted.
//!
//! # Default device
//!
//! Windows distinguishes three "default" roles — Console, Multimedia,
//! Communications. A coaching app wants the *multimedia* default
//! (what media apps record from), not Communications (hijacked by
//! voice-chat apps). The port commits to multimedia-role semantics
//! on every platform; adapters that have only one notion of "default"
//! just use it.

use std::any::Any;
use std::sync::Arc;

/// Stable identifier for a device across reboots / replugs.
///
/// Newtype around `String` so callers can't fabricate ids — they pass
/// through values they got from a prior [`AudioDevices::list_devices`]
/// call. Treat the inner string as opaque; do not parse it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A physical audio device (built-in mic, USB interface, etc.).
///
/// A device has one or more [`InputStream`]s — most consumer devices
/// have exactly one; multi-input interfaces have several.
#[derive(Clone)]
pub struct InputDevice {
    /// Stable across reboots / replugs where the platform supports
    /// it. `None` on Android and bare ALSA. Treat as opaque; do not
    /// parse.
    pub persistent_id: Option<DeviceId>,

    /// Human label for the physical device. Not unique — two AirPods
    /// of the same model present identical names.
    pub name: String,

    pub transport: Transport,

    /// All input streams this device exposes. Always non-empty for a
    /// device returned from [`AudioDevices::list_devices`].
    pub streams: Vec<InputStream>,
}

/// A single input stream on a device — what a caller opens to record.
///
/// On a built-in mic this is "the mic." On a Scarlett 18i20 this is
/// "Input 1/2" or "ADAT 1-8".
#[derive(Clone)]
pub struct InputStream {
    /// Opaque, **session-scoped** identifier. The capture port (future)
    /// will accept this to open the stream. Do not persist; do not
    /// compare across adapter instances.
    pub handle: StreamHandle,

    /// Display label. On single-stream devices often matches the
    /// device name; on multi-stream devices identifies the input.
    pub name: String,

    /// Channels in this stream. Almost always 1 (mono mic) or 2
    /// (stereo pair). Pro interfaces can present more (8 ADAT
    /// channels as a single stream).
    pub channels: u16,

    pub sample_rates: SampleRateSupport,
}

/// Physical transport a device is reached over. `Unknown` is allowed
/// — not every backend reports this cleanly (cpal doesn't surface
/// CoreAudio's transport-type property, for instance).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    BuiltIn,
    Usb,
    Bluetooth,
    /// Software device — BlackHole, Loopback, an aggregate device.
    Virtual,
    Unknown,
}

/// How a stream reports its supported sample rates. The shape varies
/// by platform — modelling it as a flat `Vec<u32>` would force every
/// adapter except Android to lie.
#[derive(Debug, Clone)]
pub enum SampleRateSupport {
    /// A discrete list (Android). May be empty / incomplete in
    /// practice.
    List(Vec<u32>),
    /// One or more `(min, max)` ranges (macOS CoreAudio, cpal).
    Ranges(Vec<(u32, u32)>),
    /// The platform does not enumerate — callers must request a
    /// rate and let the open call succeed or fail (Windows WASAPI,
    /// iOS AVAudioSession).
    ProbeOnly,
}

/// Opaque handle identifying a stream within the current adapter.
///
/// Internally each adapter stashes whatever native handle it needs
/// (cpal `Device`, `AudioDeviceID`, etc.) inside the `Arc<dyn Any>`.
/// The capture port (future) will downcast on the way back in.
#[derive(Clone)]
pub struct StreamHandle(pub Arc<dyn Any + Send + Sync>);

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner Any isn't Debug. Identify by pointer so two
        // clones of the same handle look the same in logs.
        write!(f, "StreamHandle({:p})", Arc::as_ptr(&self.0))
    }
}

pub trait AudioDevices: Send + Sync {
    /// Snapshot of currently-present input devices. Order is
    /// adapter-defined and not stable across calls.
    fn list_devices(&self) -> Vec<InputDevice>;

    /// The system's multimedia-role default input stream. Returns
    /// the stream directly because recording happens at the stream
    /// level — callers who want the parent device can find it in
    /// [`list_devices`].
    fn default_input(&self) -> Option<InputStream>;
}
