//! Music tab of the Settings screen: master/detail layout.
//!
//! Two `AppSettings` fields (reference Hz, tuning kind) are exposed as
//! pickers. There is deliberately **no note-system picker**: the head
//! is vocabulary-free (see `docs/MUSIC_MODEL.md`). Choosing a note
//! naming scheme belongs to the deferred label layer; until it ships,
//! the only musical settings are the raw calibration inputs. The tab is
//! split horizontally:
//!
//! - **Master pane** (left): one row per setting, showing name + the
//!   current value. Click selects which picker to show.
//! - **Detail pane** (right): the picker for the currently-selected
//!   setting. Both pickers are kept spawned; the inactive one is
//!   collapsed with `Display::None` (same trick as tab switching).
//!
//! Selection state lives in [`MusicSelection`]. Master rows rebuild
//! whenever it changes (so the highlight follows) or [`AppSettings`]
//! changes (so the "current value" text stays in sync). Detail-pane
//! pickers rebuild on `Changed<AppSettings>` like before.

use super::{spawn_choice_row, SettingsTab, TabContent};
use crate::state::AppSettings;
use crate::ui::*;
use bevy::prelude::*;
use domain_ports::tuning::TuningKind;

/// Which setting's picker the detail pane is currently showing. Pure
/// UI state, local to the Music tab. Resets to the first row on every
/// entry into Settings via `on_enter`.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicSelection {
    #[default]
    ReferenceHz,
    TuningKind,
}

#[derive(Component)]
pub struct ReferenceHzList;

#[derive(Component)]
pub struct TuningKindList;

/// Marker on the master pane so we can despawn-and-respawn its rows
/// when their "current value" subtitle needs to update.
#[derive(Component)]
pub struct MusicMasterPane;

/// Tag on each master-pane row. The click handler reads this to set
/// [`MusicSelection`]; `sync_music_detail` reads it to know which row
/// to highlight.
#[derive(Component, Clone, Copy)]
pub struct MusicMasterRow(pub MusicSelection);

/// One of the discrete reference-Hz choices the picker offers. The
/// underlying resource stores arbitrary `f32`; the picker just exposes
/// the four conventional values until there's reason to add a text
/// input.
#[derive(Component, Clone, Copy)]
pub struct ReferenceHzRow(pub f32);

#[derive(Component, Clone, Copy)]
pub struct TuningKindRow(pub TuningKind);

const REFERENCE_HZ_CHOICES: [(f32, &str); 4] = [
    (440.0, "A = 440 Hz (standard)"),
    (442.0, "A = 442 Hz"),
    (432.0, "A = 432 Hz"),
    (415.0, "A = 415 Hz (Baroque)"),
];

pub(super) fn spawn_tab(commands: &mut Commands, parent: Entity) {
    // Horizontal split: master (left) + detail (right). The whole tab
    // is `Display::None` at spawn so its layout slot collapses while
    // Audio is active — `sync_tab_visibility` flips this to `Flex`.
    let container = commands
        .spawn((
            ChildOf(parent),
            TabContent(SettingsTab::Music),
            Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(24),
                flex_grow: 1.0,
                min_height: px(0),
                display: Display::None,
                ..default()
            },
        ))
        .id();

    // Master pane. Fixed width, vertically stacked rows.
    commands.spawn((
        ChildOf(container),
        MusicMasterPane,
        Node {
            width: px(280),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        },
    ));

    // Detail pane. Takes the remaining horizontal space. Each picker
    // lives as a direct child here; `sync_music_detail` toggles the
    // inactive ones to `Display::None`.
    let detail = commands
        .spawn((
            ChildOf(container),
            Node {
                flex_direction: FlexDirection::Column,
                flex_grow: 1.0,
                min_width: px(0),
                row_gap: px(8),
                ..default()
            },
        ))
        .id();

    // Two pickers, identical shape — width fills the detail pane.
    commands.spawn((
        ChildOf(detail),
        ReferenceHzList,
        Node {
            width: percent(100),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        },
    ));
    commands.spawn((
        ChildOf(detail),
        TuningKindList,
        Node {
            width: percent(100),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            display: Display::None,
            ..default()
        },
    ));
}

/// Rebuild all three detail-pane pickers when `AppSettings` changes
/// or when the section containers are freshly spawned (on entering the
/// Settings screen).
///
/// We rebuild on `Added<...List>` (not just `Changed<AppSettings>`)
/// because `AppSettings` is initialised at startup, so by the time the
/// user enters Settings, `is_changed()` is `false` and the rows would
/// never appear without a manual setting tweak first.
pub fn rebuild_settings_list(
    mut commands: Commands,
    settings: Res<AppSettings>,
    ref_hz_list: Query<Entity, With<ReferenceHzList>>,
    tuning_list: Query<Entity, With<TuningKindList>>,
    just_added: Query<Entity, Or<(Added<ReferenceHzList>, Added<TuningKindList>)>>,
) {
    if !settings.is_changed() && just_added.is_empty() {
        return;
    }

    if let Ok(list_entity) = ref_hz_list.single() {
        commands.entity(list_entity).despawn_related::<Children>();
        for (hz, label) in REFERENCE_HZ_CHOICES {
            spawn_choice_row(
                &mut commands,
                list_entity,
                ReferenceHzRow(hz),
                label,
                (settings.reference_hz - hz).abs() < 1e-3,
            );
        }
    }

    if let Ok(list_entity) = tuning_list.single() {
        commands.entity(list_entity).despawn_related::<Children>();
        for kind in [
            TuningKind::TwelveTet,
            TuningKind::HindustaniJust,
            TuningKind::TwentyTwoShruti,
        ] {
            spawn_choice_row(
                &mut commands,
                list_entity,
                TuningKindRow(kind),
                tuning_kind_label(kind),
                settings.tuning_kind == kind,
            );
        }
    }
}

/// Rebuild the master-pane rows when `AppSettings` changes (so the
/// "current value" subtitle updates) or when the pane is freshly
/// spawned. Highlighting is handled separately by `sync_music_detail`
/// so we don't rebuild on every selection change.
pub fn rebuild_master_rows(
    mut commands: Commands,
    settings: Res<AppSettings>,
    selection: Res<MusicSelection>,
    pane: Query<Entity, With<MusicMasterPane>>,
    just_added: Query<Entity, Added<MusicMasterPane>>,
) {
    if !settings.is_changed() && just_added.is_empty() {
        return;
    }
    let Ok(pane_entity) = pane.single() else {
        return;
    };
    commands.entity(pane_entity).despawn_related::<Children>();

    spawn_master_row(
        &mut commands,
        pane_entity,
        MusicSelection::ReferenceHz,
        "Reference frequency",
        &reference_hz_value_label(settings.reference_hz),
        *selection == MusicSelection::ReferenceHz,
    );
    spawn_master_row(
        &mut commands,
        pane_entity,
        MusicSelection::TuningKind,
        "Tuning system",
        tuning_kind_label(settings.tuning_kind),
        *selection == MusicSelection::TuningKind,
    );
}

/// One master row: a clickable `Button` with the setting name on top
/// and the current value in dimmed text below. Highlighted in accent
/// when this row's selection matches the current [`MusicSelection`].
fn spawn_master_row(
    commands: &mut Commands,
    pane: Entity,
    selection: MusicSelection,
    name: &str,
    value: &str,
    selected_now: bool,
) {
    let row = commands
        .spawn((
            ChildOf(pane),
            Button,
            MusicMasterRow(selection),
            Node {
                flex_direction: FlexDirection::Column,
                padding: UiRect::axes(px(12), px(10)),
                width: percent(100),
                row_gap: px(2),
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
    commands.entity(row).with_children(|p| {
        p.spawn((
            Text::new(name.to_string()),
            TextFont {
                font_size: FONT_BODY,
                ..default()
            },
            TextColor(COLOR_TEXT),
        ));
        p.spawn((
            Text::new(value.to_string()),
            TextFont {
                font_size: FONT_BODY - 4.0,
                ..default()
            },
            TextColor(COLOR_TEXT_DIM),
        ));
    });
}

/// Push `MusicSelection` into the UI: show the active picker, hide
/// the others; repaint the active master row to `COLOR_ACCENT` and
/// the others to `COLOR_BUTTON`; mark the active row as
/// `ButtonSelected` so generic hover/press repaint doesn't wipe it.
///
/// Runs when the resource changes OR when the rows/lists are freshly
/// spawned, mirroring the pattern in `sync_tab_visibility`.
pub fn sync_music_detail(
    selection: Res<MusicSelection>,
    mut commands: Commands,
    mut ref_hz: Query<&mut Node, (With<ReferenceHzList>, Without<TuningKindList>)>,
    mut tuning: Query<&mut Node, (With<TuningKindList>, Without<ReferenceHzList>)>,
    mut rows: Query<(Entity, &MusicMasterRow, &mut BackgroundColor)>,
    just_added: Query<
        Entity,
        Or<(
            Added<ReferenceHzList>,
            Added<TuningKindList>,
            Added<MusicMasterRow>,
        )>,
    >,
) {
    if !selection.is_changed() && just_added.is_empty() {
        return;
    }

    let set_display = |node: &mut Node, want_visible: bool| {
        let wanted = if want_visible {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != wanted {
            node.display = wanted;
        }
    };

    if let Ok(mut n) = ref_hz.single_mut() {
        set_display(&mut n, *selection == MusicSelection::ReferenceHz);
    }
    if let Ok(mut n) = tuning.single_mut() {
        set_display(&mut n, *selection == MusicSelection::TuningKind);
    }

    for (entity, row, mut bg) in rows.iter_mut() {
        let active = row.0 == *selection;
        *bg = BackgroundColor(if active { COLOR_ACCENT } else { COLOR_BUTTON });
        if active {
            commands.entity(entity).insert(ButtonSelected);
        } else {
            commands.entity(entity).remove::<ButtonSelected>();
        }
    }
}

pub fn handle_master_row_click(
    q: Query<(&Interaction, &MusicMasterRow), (Changed<Interaction>, With<Button>)>,
    mut selection: ResMut<MusicSelection>,
) {
    for (interaction, row) in q.iter() {
        if *interaction == Interaction::Pressed && *selection != row.0 {
            *selection = row.0;
        }
    }
}

fn reference_hz_value_label(hz: f32) -> String {
    for (choice_hz, label) in REFERENCE_HZ_CHOICES {
        if (hz - choice_hz).abs() < 1e-3 {
            return label.to_string();
        }
    }
    format!("A = {hz:.1} Hz")
}

fn tuning_kind_label(s: TuningKind) -> &'static str {
    match s {
        TuningKind::TwelveTet => "12-tone equal temperament",
        TuningKind::HindustaniJust => "Hindustani Just intonation",
        TuningKind::TwentyTwoShruti => "22-shruti just intonation",
    }
}

pub fn handle_reference_hz_click(
    q: Query<(&Interaction, &ReferenceHzRow), (Changed<Interaction>, With<Button>)>,
    mut settings: ResMut<AppSettings>,
) {
    for (interaction, row) in q.iter() {
        if *interaction == Interaction::Pressed {
            settings.reference_hz = row.0;
        }
    }
}

pub fn handle_tuning_kind_click(
    q: Query<(&Interaction, &TuningKindRow), (Changed<Interaction>, With<Button>)>,
    mut settings: ResMut<AppSettings>,
) {
    for (interaction, row) in q.iter() {
        if *interaction == Interaction::Pressed {
            settings.tuning_kind = row.0;
        }
    }
}
