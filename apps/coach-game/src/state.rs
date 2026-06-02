//! App-wide state and player-facing settings resources.
//!
//! The state machine is intentionally flat (MainMenu / Settings / InGame /
//! Paused). Settings doesn't need a sub-state for the audio tab yet —
//! there's only one tab. Promote to a SubStates when a second tab appears.

use bevy::prelude::*;
use domain_ports::audio_devices::{DeviceId, InputDevice};

#[derive(States, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[states(scoped_entities)]
pub enum AppState {
    #[default]
    MainMenu,
    Settings,
    InGame,
    /// Esc-pause overlay drawn on top of (but mutually exclusive with)
    /// InGame. Entering Paused stops the AppCoach session; resuming
    /// starts a fresh one. See `HasPausedSession` for the menu-label
    /// gate that lets MainMenu show "Continue".
    Paused,
}

/// True while there's a paused session the player can resume. Set by
/// `OnEnter(Paused)`, cleared when the player Quits-to-Main from the
/// pause overlay (which truly ends the run). Drives the main menu's
/// Continue/New Game label.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct HasPausedSession(pub bool);

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
