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
//! - `trace` — the UX flight recorder (gzip-compressed JSONL trace of
//!   inputs, coach reads, and computed geometry); wired in `main`, not
//!   `build_app`.
//!
//! Items are `pub` so integration tests under `tests/` can spawn the
//! schedule against a fake `AppCoach`.

pub mod coach;
pub mod feature_history;
pub mod feature_types;
pub mod font;
pub mod game;
pub mod menu;
pub mod replay_audio;
pub mod semantic_graph;
pub mod state;
pub mod trace;
pub mod ui;
pub mod widgets;

use bevy::prelude::*;
use bevy::sprite_render::Material2dPlugin;
use bevy::ui::UiSystems;
use menu::permission::{MicStatus, PermissionPrompt};
use state::{
    AppSettings, AppState, Autostart, HasPausedSession, KnownDevices, KnownScales, ResumeLocked,
    SelectedDevice, SongTonality,
};

/// Register the GPU mesh-trace material plugin and embed its WGSL shader.
///
/// Called from `main::finish` (not from `build_app`) so headless tests — which
/// use `MinimalPlugins` and have no `EmbeddedAssetRegistry` — are unaffected.
pub fn add_mesh_trace_plugin(app: &mut App) {
    app.add_plugins(Material2dPlugin::<widgets::time_graph::TraceMaterial>::default());
    bevy::asset::embedded_asset!(app, "widgets/time_graph/trace.wgsl");
}

/// Register the game's state, resources, and systems. Split out of
/// `main` so headless tests can call it after `MinimalPlugins +
/// StatesPlugin` without dragging in the renderer or the production
/// `Coach` construction.
pub fn build_app(app: &mut App) {
    app.init_state::<AppState>()
        // When --autostart is passed, main inserts the `Autostart` marker before
        // `build_app` runs. This startup system fires on the first update and
        // jumps straight to InGame — identical to the Free Practice button path.
        // `run_if(resource_exists)` makes this a no-op on a normal run.
        .add_systems(
            Startup,
            autostart_system.run_if(resource_exists::<Autostart>),
        )
        .init_resource::<AppSettings>()
        .init_resource::<SongTonality>()
        .init_resource::<SelectedDevice>()
        .init_resource::<KnownDevices>()
        .init_resource::<KnownScales>()
        .init_resource::<HasPausedSession>()
        .init_resource::<ResumeLocked>()
        .init_resource::<PermissionPrompt>()
        .init_resource::<MicStatus>()
        .init_resource::<menu::paused::ShowingQuitConfirm>()
        .init_resource::<menu::settings::SettingsTab>()
        .init_resource::<menu::settings::MusicSelection>()
        .init_resource::<game::LastFeatureHop>()
        .init_resource::<game::SessionCounter>()
        .init_resource::<game::scale_picker::ShowingScalePicker>()
        .init_resource::<coach::MusicInfoRes>()
        .init_resource::<coach::LatestFeatures>()
        .init_resource::<coach::FeatureHistoryRes>()
        .init_resource::<coach::FeatureDrainScratch>()
        .init_resource::<coach::FeatureDrainCount>()
        .init_resource::<game::GraphProjectorRes>()
        .init_resource::<game::SemanticGraphRes>()
        .init_resource::<widgets::time_graph::scene::TimeGraphPitchLaneSize>()
        .init_resource::<widgets::time_graph::scene::TimeGraphPitchLanePhysRect>()
        .init_resource::<widgets::time_graph::scene::TimeGraphPitchLaneScale>()
        .init_resource::<widgets::time_graph::systems::LastTraceGeom>()
        .init_resource::<widgets::time_graph::scene::TimeGraphGridSceneRes>()
        .init_resource::<widgets::time_graph::scene::TimeGraphLiveSceneRes>()
        .init_resource::<widgets::hud::scene::HudSceneRes>()
        // Register AppLifecycle so `query_on_foreground` works in headless tests
        // (MinimalPlugins skips WindowPlugin which normally adds it).
        .add_message::<bevy::window::AppLifecycle>()
        // Always-on
        .add_observer(ui::on_scroll)
        .add_systems(
            Update,
            (
                coach::drain_events,
                coach::query_on_foreground,
                ui::update_button_colors,
                ui::send_scroll_events,
                // Order: update_dial_metrics reads ComputedNode and writes
                // DialMetrics; rebuild_slots spawns slot children;
                // apply_state paints them. `.chain()` at each level
                // flushes Commands so the next system sees the new entities.
                // update_hub_size also depends on DialMetrics and runs
                // after rebuild_slots/apply_state (parallel is fine, but
                // chaining keeps the order explicit and avoids confusion).
                (
                    widgets::note_dial::systems::update_dial_metrics,
                    (
                        widgets::note_dial::systems::rebuild_slots,
                        widgets::note_dial::systems::apply_state,
                    )
                        .chain(),
                    widgets::note_dial::systems::update_hub_size,
                )
                    .chain(),
            ),
        )
        // Shutdown lives in `Last` so it runs after any system that
        // writes AppExit (Quit button, window-close handler, etc.) in
        // the same frame the runner is about to exit on.
        .add_systems(Last, coach::shutdown_on_exit)
        // MainMenu
        .add_systems(
            OnEnter(AppState::MainMenu),
            (menu::main_menu::spawn, send_boot_permission_query),
        )
        .add_systems(
            Update,
            (
                menu::main_menu::handle_new_game,
                menu::main_menu::handle_settings,
                menu::main_menu::handle_quit,
                (
                    menu::main_menu::advance_checking_hardware,
                    menu::permission::sync_permission_modal,
                    menu::permission::handle_allow_mic,
                    menu::permission::handle_open_settings,
                    menu::permission::handle_permission_cancel,
                )
                    .after(coach::drain_events),
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
                    game::spawn_pause_button,
                    game::time_graph::spawn,
                )
                    .chain(),
            ),
        )
        .add_systems(OnExit(AppState::InGame), game::on_exit)
        .add_systems(
            OnExit(AppState::InGame),
            widgets::time_graph::systems::clear_trace_mesh_handles
                .run_if(resource_exists::<widgets::time_graph::MeshTrace>),
        )
        // Mesh-trace overlay camera and painter — only active when `--mesh-trace`
        // is passed (resource_exists guard; no-op otherwise).
        .add_systems(
            OnEnter(AppState::InGame),
            widgets::time_graph::systems::spawn_trace_overlay_camera
                .run_if(resource_exists::<widgets::time_graph::MeshTrace>),
        )
        .add_systems(
            Update,
            (
                widgets::time_graph::systems::clear_pitch_lane_bg_for_mesh,
                widgets::time_graph::systems::apply_mesh_lane_bg,
                widgets::time_graph::systems::apply_mesh_gridlines,
                widgets::time_graph::systems::apply_mesh_trace,
            )
                .run_if(in_state(AppState::InGame))
                .run_if(resource_exists::<widgets::time_graph::MeshTrace>),
        )
        .add_systems(
            Update,
            (
                game::log_features,
                (
                    game::refresh_semantic_graph,
                    game::time_graph::refresh_scene,
                    widgets::time_graph::systems::apply_gridlines,
                    widgets::time_graph::systems::apply_trace,
                    widgets::time_graph::systems::apply_events,
                )
                    .chain(),
                game::handle_esc_in_game,
                game::handle_pause_button,
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
                menu::paused::sync_resume_locked,
                game::handle_esc_paused,
            )
                .run_if(in_state(AppState::Paused)),
        );
}

/// Startup system: set the initial state to `InGame` when `--autostart` is
/// active. Guarded by `run_if(resource_exists::<Autostart>)` so it is a no-op
/// on normal runs where the `Autostart` marker is absent.
fn autostart_system(mut next: ResMut<NextState<AppState>>) {
    next.set(AppState::InGame);
}

fn send_boot_permission_query(coach: NonSend<coach::Coach>) {
    coach
        .0
        .send_command(domain_ports::app_coach::Command::AudioPermissionQuery);
}
