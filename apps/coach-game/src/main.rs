// Bevy queries naturally produce complex generic types; the canonical
// pattern in Bevy code is to allow this lint crate-wide rather than
// alias every Query.
#![allow(clippy::type_complexity)]

//! coach-game: Bevy host for the singing-coach.
//!
//! Three-state app shell: MainMenu → Settings → InGame. AppCoach lives
//! as a `NonSend` resource for the app's lifetime; session lifecycle
//! is driven by state transitions (start on InGame enter, stop on
//! exit). Module layout:
//!
//! - `coach` — AppCoach handle + always-on event drain.
//! - `state` — AppState enum + shared resources.
//! - `ui` — colour palette + per-frame button repaint.
//! - `menu::main_menu`, `menu::settings` — menu screens.
//! - `game` — InGame setup/teardown + feature logging.

mod coach;
mod game;
mod menu;
mod state;
mod ui;

use bevy::prelude::*;
use state::{AppState, KnownDevices, SelectedDevice};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .init_state::<AppState>()
        .init_resource::<SelectedDevice>()
        .init_resource::<KnownDevices>()
        .init_resource::<game::LastFeatureTs>()
        .add_systems(Startup, (spawn_camera, coach::spawn_coach))
        // Always-on
        .add_systems(Update, (coach::drain_events, ui::update_button_colors))
        // MainMenu
        .add_systems(OnEnter(AppState::MainMenu), menu::main_menu::spawn)
        .add_systems(
            Update,
            (
                menu::main_menu::handle_new_game,
                menu::main_menu::handle_settings,
                menu::main_menu::handle_quit,
            )
                .run_if(in_state(AppState::MainMenu)),
        )
        // Settings
        .add_systems(OnEnter(AppState::Settings), menu::settings::on_enter)
        .add_systems(
            Update,
            (
                menu::settings::rebuild_device_list,
                menu::settings::handle_row_click,
                menu::settings::handle_back,
            )
                .run_if(in_state(AppState::Settings)),
        )
        // InGame
        .add_systems(OnEnter(AppState::InGame), game::on_enter)
        .add_systems(OnExit(AppState::InGame), game::on_exit)
        .add_systems(
            Update,
            game::log_features.run_if(in_state(AppState::InGame)),
        )
        .run();
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}
