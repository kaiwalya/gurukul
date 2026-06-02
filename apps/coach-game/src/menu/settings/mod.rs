//! Settings screen: tabbed layout (Audio / Music) + Back.
//!
//! This module's shell — the screen frame, tab bar, Back button, and
//! tab-switching machinery — lives here. Each tab's content + systems
//! live in its own submodule:
//!
//! - [`audio`] — input-device picker.
//! - [`music`] — reference Hz / tuning system / note system pickers.
//!
//! Both tabs are spawned at `on_enter`; only the active one is visible
//! (toggled by [`sync_tab_visibility`] when [`SettingsTab`] changes).
//! Tab content does not despawn-and-respawn on switch, so child entity
//! identity is stable across tab switches.
//!
//! Tabs are a desktop-first pattern; iOS will likely want a different
//! shell (push-navigation grouped list). The per-tab modules port
//! directly when that lands.

pub mod audio;
pub mod music;

use crate::coach::Coach;
use crate::state::AppState;
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::app_coach::Command;

// Re-export the per-tab systems and types so external call sites
// (lib.rs registry, integration tests) keep using
// `menu::settings::rebuild_device_list` etc. without knowing the
// internal split.
pub use audio::{handle_row_click, rebuild_device_list, DeviceList, DeviceRow};
pub use music::{
    handle_master_row_click, handle_note_system_click, handle_reference_hz_click,
    handle_tuning_system_click, rebuild_master_rows, rebuild_settings_list, sync_music_detail,
    MusicMasterRow, MusicSelection, NoteSystemList, NoteSystemRow, ReferenceHzList, ReferenceHzRow,
    TuningSystemList, TuningSystemRow,
};

/// Which tab is active on the Settings screen. Pure UI state — lives
/// here rather than in `crate::state` because it's local to this
/// screen and nothing outside Settings cares about it. Resets to
/// `Audio` every time the Settings screen is entered (handled in
/// `on_enter`), so the user always lands on the first tab.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    Audio,
    Music,
}

/// Marker for a tab-bar button. The variant is the tab the button
/// activates; the click handler reads it and writes `SettingsTab`.
#[derive(Component, Clone, Copy)]
pub struct TabButton(pub SettingsTab);

/// Marker on the container for each tab's content. `sync_tab_visibility`
/// toggles each container's `Visibility` based on the current tab.
#[derive(Component, Clone, Copy)]
pub struct TabContent(pub SettingsTab);

#[derive(Component)]
pub struct BackButton;

pub fn on_enter(
    coach: NonSend<Coach>,
    mut commands: Commands,
    mut tab: ResMut<SettingsTab>,
    mut music_selection: ResMut<music::MusicSelection>,
) {
    // Reset to the first tab so re-entering Settings never lands the
    // user on whatever they had open last time.
    *tab = SettingsTab::Audio;
    // Same idea for the Music tab's internal selection — first row.
    *music_selection = music::MusicSelection::ReferenceHz;

    // Kick off a fresh enumeration. `KnownDevices` will be populated
    // within a frame or two by `coach::drain_events`.
    coach.0.send_command(Command::ListDevices);

    let root = commands
        .spawn((
            DespawnOnExit(AppState::Settings),
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                padding: UiRect::all(px(32)),
                row_gap: px(20),
                ..default()
            },
            BackgroundColor(COLOR_BG),
        ))
        .id();

    // Title.
    commands.spawn((
        ChildOf(root),
        Text::new("Settings"),
        TextFont {
            font_size: FONT_TITLE,
            ..default()
        },
        TextColor(COLOR_ACCENT),
    ));

    // Tab bar.
    let tab_bar = commands
        .spawn((
            ChildOf(root),
            Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(8),
                margin: UiRect::top(px(8)),
                ..default()
            },
        ))
        .id();
    spawn_tab_button(&mut commands, tab_bar, SettingsTab::Audio, "Audio", true);
    spawn_tab_button(&mut commands, tab_bar, SettingsTab::Music, "Music", false);

    // Tab content containers. Both spawned; `sync_tab_visibility`
    // toggles `Visibility` based on `SettingsTab`. The Audio container
    // is visible at spawn since we just reset the tab to Audio above.
    audio::spawn_tab(&mut commands, root);
    music::spawn_tab(&mut commands, root);

    // Back button (outside the tabs — always accessible).
    commands
        .spawn((
            ChildOf(root),
            Button,
            BackButton,
            Node {
                width: px(160),
                height: px(48),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                margin: UiRect::top(px(32)),
                ..default()
            },
            BackgroundColor(COLOR_BUTTON),
        ))
        .with_child((
            Text::new("Back"),
            TextFont {
                font_size: FONT_BUTTON,
                ..default()
            },
            TextColor(COLOR_TEXT),
        ));
}

fn spawn_tab_button(
    commands: &mut Commands,
    parent: Entity,
    tab: SettingsTab,
    label: &str,
    active: bool,
) {
    let bg = if active { COLOR_ACCENT } else { COLOR_BUTTON };
    let btn = commands
        .spawn((
            ChildOf(parent),
            Button,
            TabButton(tab),
            Node {
                width: px(180),
                height: px(44),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(bg),
        ))
        .id();
    if active {
        // ButtonSelected opts the active tab out of generic hover/press
        // repaint so the accent colour stays during interaction.
        commands.entity(btn).insert(ButtonSelected);
    }
    commands.entity(btn).with_child((
        Text::new(label.to_string()),
        TextFont {
            font_size: FONT_BUTTON,
            ..default()
        },
        TextColor(COLOR_TEXT),
    ));
}

pub fn handle_back(
    q: Query<&Interaction, (Changed<Interaction>, With<BackButton>)>,
    mut next: ResMut<NextState<AppState>>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            next.set(AppState::MainMenu);
        }
    }
}

/// Tab button click → write `SettingsTab`. `sync_tab_visibility` reacts
/// to the resource change and swaps which content container is visible
/// and which tab button carries the `ButtonSelected` highlight.
pub fn handle_tab_click(
    q: Query<(&Interaction, &TabButton), (Changed<Interaction>, With<Button>)>,
    mut tab: ResMut<SettingsTab>,
) {
    for (interaction, btn) in q.iter() {
        if *interaction == Interaction::Pressed && *tab != btn.0 {
            *tab = btn.0;
        }
    }
}

/// Spawn one row of a settings picker: a clickable `Button` with a
/// `marker` component identifying it and a text label. Highlights in
/// `COLOR_ACCENT` when `selected_now`, otherwise `COLOR_BUTTON`. Shared
/// by every picker in this module (device, reference Hz, tuning, note
/// system) so they look and behave identically.
pub(super) fn spawn_choice_row<M: Component>(
    commands: &mut Commands,
    list_entity: Entity,
    marker: M,
    label: &str,
    selected_now: bool,
) {
    let row = commands
        .spawn((
            ChildOf(list_entity),
            Button,
            marker,
            Node {
                padding: UiRect::axes(px(12), px(8)),
                width: percent(100),
                ..default()
            },
            BackgroundColor(if selected_now {
                COLOR_ACCENT
            } else {
                COLOR_BUTTON
            }),
        ))
        .id();
    if selected_now {
        commands.entity(row).insert(ButtonSelected);
    }
    commands.entity(row).with_child((
        Text::new(label.to_string()),
        TextFont {
            font_size: FONT_BODY,
            ..default()
        },
        TextColor(COLOR_TEXT),
    ));
}

/// Push `SettingsTab` into the UI: show the active tab's content, hide
/// the others; repaint the active tab button to `COLOR_ACCENT` and the
/// others to `COLOR_BUTTON`; mark the active button as `ButtonSelected`
/// so generic hover/press repaint doesn't wipe the highlight.
///
/// Hides inactive tabs with `Display::None` (not `Visibility::Hidden`)
/// so flex layout *collapses* their slot — otherwise the active tab
/// gets pushed down by the empty space the inactive tab reserves.
///
/// Runs when the resource changes OR when the buttons/containers are
/// freshly spawned (`Added<...>`), since `OnEnter(Settings)` resets the
/// tab to `Audio` *before* this system gets a chance to paint — and
/// without the `Added` trigger the initial frame's coloring would be
/// whatever the spawn defaults are.
pub fn sync_tab_visibility(
    tab: Res<SettingsTab>,
    mut commands: Commands,
    mut content: Query<(&TabContent, &mut Node)>,
    mut buttons: Query<(Entity, &TabButton, &mut BackgroundColor)>,
    just_added: Query<Entity, Or<(Added<TabContent>, Added<TabButton>)>>,
) {
    if !tab.is_changed() && just_added.is_empty() {
        return;
    }

    for (content_tab, mut node) in content.iter_mut() {
        let wanted = if content_tab.0 == *tab {
            Display::Flex
        } else {
            Display::None
        };
        // Guard the write so we don't trigger change detection (and a
        // full relayout) every frame when the tab hasn't changed.
        if node.display != wanted {
            node.display = wanted;
        }
    }

    for (entity, btn, mut bg) in buttons.iter_mut() {
        let active = btn.0 == *tab;
        *bg = BackgroundColor(if active { COLOR_ACCENT } else { COLOR_BUTTON });
        if active {
            commands.entity(entity).insert(ButtonSelected);
        } else {
            commands.entity(entity).remove::<ButtonSelected>();
        }
    }
}
