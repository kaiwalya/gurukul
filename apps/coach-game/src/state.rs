//! App-wide state and player-facing settings resources.
//!
//! The state machine is intentionally flat (MainMenu / Settings / InGame).
//! Settings doesn't need a sub-state for the audio tab yet — there's only
//! one tab. Promote to a SubStates when a second tab appears.

use bevy::prelude::*;
use domain_ports::audio_devices::{DeviceId, InputDevice};

#[derive(States, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[states(scoped_entities)]
pub enum AppState {
    #[default]
    MainMenu,
    Settings,
    InGame,
}

/// User-chosen input device, persists across menu navigation. `None`
/// means "use OS default" — what AppCoach's `SessionConfig::device_id`
/// also treats as default.
#[derive(Resource, Default, Debug, Clone)]
pub struct SelectedDevice(pub Option<DeviceId>);

/// The most recent device list from `CoachEvent::DevicesListed`. The
/// Settings → Audio screen populates it on enter (via `ListDevices`)
/// and renders one row per device.
#[derive(Resource, Default)]
pub struct KnownDevices(pub Vec<InputDevice>);
