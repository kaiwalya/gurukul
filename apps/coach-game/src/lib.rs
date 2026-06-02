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
//!
//! Items are `pub` so integration tests under `tests/` can spawn the
//! schedule against a fake `AppCoach`.

pub mod coach;
pub mod game;
pub mod menu;
pub mod state;
pub mod ui;
pub mod widgets;

use bevy::prelude::*;
use state::{AppState, HasPausedSession, KnownDevices, SelectedDevice};

/// Register the game's state, resources, and systems. Split out of
/// `main` so headless tests can call it after `MinimalPlugins +
/// StatesPlugin` without dragging in the renderer or the production
/// `Coach` construction.
pub fn build_app(app: &mut App) {
    app.init_state::<AppState>()
        .init_resource::<SelectedDevice>()
        .init_resource::<KnownDevices>()
        .init_resource::<HasPausedSession>()
        .init_resource::<menu::paused::ShowingQuitConfirm>()
        .init_resource::<game::LastFeatureTs>()
        // Always-on
        .add_systems(
            Update,
            (
                coach::drain_events,
                ui::update_button_colors,
                // Order: rebuild_slots spawns slot children; apply_state
                // paints them. `.chain()` inserts the sync point that
                // flushes the rebuild's spawn commands so apply_state
                // sees the new SlotDot entities on the same frame.
                (
                    widgets::note_dial::rebuild_slots,
                    widgets::note_dial::apply_state,
                )
                    .chain(),
            ),
        )
        // Shutdown lives in `Last` so it runs after any system that
        // writes AppExit (Quit button, window-close handler, etc.) in
        // the same frame the runner is about to exit on.
        .add_systems(Last, coach::shutdown_on_exit)
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
        .add_systems(
            OnEnter(AppState::InGame),
            (game::on_enter, game::dial::spawn),
        )
        .add_systems(OnExit(AppState::InGame), game::on_exit)
        .add_systems(
            Update,
            (
                game::log_features,
                game::handle_esc_in_game,
                game::dial::update_from_features,
            )
                .run_if(in_state(AppState::InGame)),
        )
        // Paused
        .add_systems(
            OnEnter(AppState::Paused),
            (menu::paused::spawn, menu::paused::reset_confirm_flag),
        )
        .add_systems(OnExit(AppState::Paused), menu::paused::reset_confirm_flag)
        .add_systems(
            Update,
            (
                menu::paused::handle_resume,
                menu::paused::handle_quit_to_main,
                menu::paused::handle_confirm_yes,
                menu::paused::handle_confirm_cancel,
                menu::paused::sync_confirm_modal,
                game::handle_esc_paused,
            )
                .run_if(in_state(AppState::Paused)),
        );
}
