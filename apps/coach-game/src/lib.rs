#![allow(clippy::type_complexity)]

//! coach-game: Bevy host for the singing-coach.
//!
//! Three-state app shell: MainMenu → Settings → InGame. AppCoach lives
//! as a `NonSend` resource for the app's lifetime; session lifecycle
//! is driven by state transitions (start on InGame enter, stop on
//! exit). Module layout:
//!
//! - `coach` — AppCoach handle + always-on event drain.
//! - `font` — global default-font override (Devanagari support); wired
//!   in `main`, not `build_app` (tests have no AssetServer).
//! - `state` — AppState enum + shared resources.
//! - `ui` — colour palette + per-frame button repaint.
//! - `menu::main_menu`, `menu::settings` — menu screens.
//! - `game` — InGame setup/teardown + feature logging, plus the route
//!   glue (`game/<name>.rs`, one per widget) that stitches widgets to
//!   app state.
//! - `widgets::<name>` — InGame UI vertical slices (`model` / `scene` /
//!   `systems`); see `ARCHITECTURE.md`.
//! - `semantic_graph` — crate-level shared pitch/time projection that
//!   feeds the time-graph widget.
//!
//! Items are `pub` so integration tests under `tests/` can spawn the
//! schedule against a fake `AppCoach`.

pub mod coach;
pub mod feature_history;
pub mod feature_types;
pub mod font;
pub mod game;
pub mod menu;
pub mod semantic_graph;
pub mod state;
pub mod ui;
pub mod widgets;

use bevy::prelude::*;
use bevy::ui::UiSystems;
use state::{
    AppSettings, AppState, HasPausedSession, KnownDevices, KnownScales, SelectedDevice,
    SongTonality,
};

/// Register the game's state, resources, and systems. Split out of
/// `main` so headless tests can call it after `MinimalPlugins +
/// StatesPlugin` without dragging in the renderer or the production
/// `Coach` construction.
pub fn build_app(app: &mut App) {
    app.init_state::<AppState>()
        .init_resource::<AppSettings>()
        .init_resource::<SongTonality>()
        .init_resource::<SelectedDevice>()
        .init_resource::<KnownDevices>()
        .init_resource::<KnownScales>()
        .init_resource::<HasPausedSession>()
        .init_resource::<menu::paused::ShowingQuitConfirm>()
        .init_resource::<menu::settings::SettingsTab>()
        .init_resource::<menu::settings::MusicSelection>()
        .init_resource::<game::LastFeatureHop>()
        .init_resource::<game::scale_picker::ShowingScalePicker>()
        .init_resource::<coach::MusicInfoRes>()
        .init_resource::<coach::LatestFeatures>()
        .init_resource::<coach::FeatureHistoryRes>()
        .init_resource::<coach::FeatureDrainScratch>()
        .init_resource::<game::GraphProjectorRes>()
        .init_resource::<game::SemanticGraphRes>()
        .init_resource::<widgets::time_graph::scene::TimeGraphPitchLaneSize>()
        .init_resource::<widgets::time_graph::scene::TimeGraphSceneRes>()
        .init_resource::<widgets::hud::scene::HudSceneRes>()
        // Always-on
        .add_observer(ui::on_scroll)
        .add_systems(
            Update,
            (
                coach::drain_events,
                ui::update_button_colors,
                ui::send_scroll_events,
                // Order: rebuild_slots spawns slot children; apply_state
                // paints them. `.chain()` inserts the sync point that
                // flushes the rebuild's spawn commands so apply_state
                // sees the new SlotDot entities on the same frame.
                (
                    widgets::note_dial::systems::rebuild_slots,
                    widgets::note_dial::systems::apply_state,
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
                menu::settings::rebuild_settings_list,
                menu::settings::rebuild_master_rows,
                menu::settings::sync_tab_visibility,
                menu::settings::sync_music_detail,
                menu::settings::handle_tab_click,
                menu::settings::handle_row_click,
                menu::settings::handle_master_row_click,
                menu::settings::handle_reference_hz_click,
                menu::settings::handle_tuning_kind_click,
                menu::settings::handle_back,
            )
                .run_if(in_state(AppState::Settings)),
        )
        // InGame
        .add_systems(
            OnEnter(AppState::InGame),
            (
                game::on_enter,
                (
                    game::spawn_root,
                    game::note_dial::spawn,
                    game::hud::spawn,
                    game::time_graph::spawn,
                )
                    .chain(),
            ),
        )
        .add_systems(OnExit(AppState::InGame), game::on_exit)
        .add_systems(
            Update,
            (
                game::log_features,
                (
                    game::refresh_semantic_graph,
                    game::time_graph::refresh_scene,
                    widgets::time_graph::systems::apply_scene,
                    widgets::time_graph::systems::apply_trace_scene,
                )
                    .chain(),
                game::handle_esc_in_game,
                game::note_dial::update_from_features,
                game::note_dial::repaint_slots,
                game::note_dial::handle_hub_capture,
                game::note_dial::sync_hub,
                game::hud::refresh,
                widgets::hud::systems::sync_text,
                // Scale picker: handle_hud_click opens, sync_picker
                // spawns/despawns, sync_rows repopulates when the catalogue
                // lands, row/close clicks select or close. The three are
                // `.chain()`ed so sync_picker's spawn `Commands` flush
                // before sync_rows reads the new ScalePickerRows on the same
                // frame (the same-frame sync-point rule, AGENTS.md).
                (
                    game::scale_picker::handle_hud_click,
                    game::scale_picker::sync_picker,
                    game::scale_picker::sync_rows,
                )
                    .chain(),
                game::scale_picker::handle_row_click,
                game::scale_picker::handle_close_click,
            )
                // Read this frame's republished resources, not last
                // frame's: drain_events writes MusicInfoRes / LatestFeatures.
                .after(coach::drain_events)
                .run_if(in_state(AppState::InGame)),
        )
        .add_systems(
            PostUpdate,
            widgets::time_graph::systems::capture_pitch_lane_size
                .after(UiSystems::PostLayout)
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
