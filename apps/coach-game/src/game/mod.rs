//! InGame state: open a session with the selected device, stream
//! features. Features print to stdout (see [`log_features`]) and feed
//! the note dial overlay (see [`note_dial`]).

pub mod hud;
pub mod note_dial;
pub mod scale_picker;
pub mod time_graph;

use crate::coach::{Coach, FeatureHistoryRes, Features, LatestFeatures, MusicInfoRes};
use crate::semantic_graph::{GraphProjector, SemanticGraph};
use crate::state::{AppState, HasPausedSession, ResumeLocked, SelectedDevice, SongTonality};
use crate::ui::{COLOR_TEXT_DIM, FONT_HEADER};
use bevy::prelude::*;
use domain_ports::app_coach::{AudioConfig, Command};

#[derive(Resource, Default)]
pub struct LastFeatureHop(Option<u64>);

/// The UX-trace stamp for this run — the `<YYYY-MM-DD-HHMMSS-mmm>` string used
/// in the UX trace filename. Inserted by `main.rs` so `on_enter` can pair the
/// engine-input audio prefix with the UX trace.
#[derive(Resource, Clone)]
pub struct LaunchStamp(pub String);

/// Counts how many `StartSession` calls have been issued this run. Starts at
/// `0`; incremented each time `on_enter` fires. Used to disambiguate repeated
/// sessions: the first session uses `<stamp>-engine-input`, the second uses
/// `<stamp>-engine-input-s2`, etc.
#[derive(Resource, Default)]
pub struct SessionCounter(pub u32);

#[derive(Resource, Default)]
pub struct GraphProjectorRes(pub GraphProjector);

#[derive(Resource, Default)]
pub struct SemanticGraphRes(pub SemanticGraph);

#[derive(Component)]
pub struct InGameRoot;

#[derive(Component)]
pub struct HudSlot;

#[derive(Component)]
pub struct ContentRow;

#[derive(Component)]
pub struct GraphSlot;

#[derive(Component)]
pub struct DialSlot;

/// Marker for the on-screen pause button in the in-game HUD strip.
/// Tapping/clicking it triggers the same transition as the Escape key.
#[derive(Component)]
pub struct PauseButton;

pub fn spawn_root(mut commands: Commands) {
    // One breathing unit between/around the content widgets. Replaces the old
    // 80px dial inset that pooled dead space in the middle of a landscape row.
    const GUTTER: f32 = 24.0;

    let hud_slot = commands
        .spawn((
            HudSlot,
            Name::new("hud_slot"),
            Node {
                width: Val::Percent(100.0),
                flex_direction: FlexDirection::Row,
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::FlexStart,
                padding: UiRect {
                    right: px(32),
                    ..default()
                },
                ..default()
            },
        ))
        .id();

    let graph_slot = commands
        .spawn((
            GraphSlot,
            Name::new("graph_slot"),
            Node {
                flex_grow: 1.0,
                // Explicit full height: the row now centers its items
                // (`align_items: Center`), which would otherwise collapse this
                // slot to its content height. The graph fills the row.
                height: Val::Percent(100.0),
                ..default()
            },
        ))
        .id();

    let dial_slot = commands
        .spawn((
            DialSlot,
            Name::new("dial_slot"),
            Node {
                // A-outer: the dial is height-driven and square, so the slot
                // sizes to it instead of pinning a fixed-width rail. The dial
                // fills the column height and its width follows via
                // aspect_ratio; `flex_shrink: 0` stops a narrow row from
                // squeezing the rail. Centered both ways so the dial sits in
                // the middle of its slot, not jammed into a corner — paired
                // with the row's `align_items: Center` this vertically centers
                // the dial against the graph panel. The right padding is one
                // breathing unit (was 80px, which pooled dead space mid-row).
                flex_shrink: 0.0,
                // Full height so the dial's `height: 100%` resolves against the
                // row height (the row centers items, so this slot would
                // otherwise be content-height and the dial would collapse).
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                padding: UiRect {
                    right: px(GUTTER),
                    ..default()
                },
                ..default()
            },
        ))
        .id();

    let content_row = commands
        .spawn((
            ContentRow,
            Name::new("content_row"),
            Node {
                flex_direction: FlexDirection::Row,
                flex_grow: 1.0,
                // Center the dial vertically against the graph panel (kills the
                // top-heavy "floating" look), and keep a tight gutter between
                // them so they read as one composition, not two apps.
                align_items: AlignItems::Center,
                column_gap: px(GUTTER),
                ..default()
            },
        ))
        .add_children(&[graph_slot, dial_slot])
        .id();

    commands
        .spawn((
            DespawnOnExit(AppState::InGame),
            InGameRoot,
            Name::new("in_game"),
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                right: px(0),
                bottom: px(0),
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                padding: UiRect {
                    left: px(32),
                    right: px(0),
                    top: px(24),
                    // Match today's 24px breathing room under the graph
                    // (the dial keeps its own 80px via the rail padding).
                    bottom: px(24),
                },
                ..default()
            },
        ))
        .add_children(&[hud_slot, content_row]);
}

pub fn on_enter(
    coach: NonSend<Coach>,
    selected: Res<SelectedDevice>,
    tonality: Res<SongTonality>,
    mut has_paused: ResMut<HasPausedSession>,
    stamp: Option<Res<LaunchStamp>>,
    mut counter: ResMut<SessionCounter>,
) {
    // Configure the musical frame of reference *before* starting audio,
    // so the coach holds the scale the moment a session is live. The two
    // are decoupled (configure causes no state change), but configuring
    // first means the reference is never momentarily absent while Running.
    coach
        .0
        .send_command(Command::MusicConfigureSession { scale: tonality.0 });

    // Build the audio-input recording prefix from the same stamp used for the
    // UX trace, so both artifacts share the same run identity.
    let session_label = stamp.map(|s| {
        counter.0 += 1;
        let base = format!("{}-engine-input", s.0);
        let name = if counter.0 == 1 {
            base
        } else {
            format!("{}-s{}", base, counter.0)
        };
        let prefix = crate::trace::trace_root().join(&name);
        info!("recording engine input → {}.wav", prefix.display());
        prefix
    });

    coach
        .0
        .send_command(Command::AudioStartSession(AudioConfig {
            device_id: selected.0.clone(),
            sample_rate: None,
            buffer_frames: None,
            session_label,
        }));
    // Whether we got here from MainMenu (Free Practice / Continue) or from
    // Paused (Resume), a session is now live and there's no separate
    // paused-session to keep around.
    has_paused.0 = false;
}

pub fn on_exit(
    coach: NonSend<Coach>,
    mut last: ResMut<LastFeatureHop>,
    mut history: ResMut<FeatureHistoryRes>,
    mut projector: ResMut<GraphProjectorRes>,
    mut graph: ResMut<SemanticGraphRes>,
    mut grid: ResMut<crate::widgets::time_graph::TimeGraphGridSceneRes>,
    mut live: ResMut<crate::widgets::time_graph::TimeGraphLiveSceneRes>,
) {
    coach.0.send_command(Command::AudioStopSession);
    last.0 = None;
    history.0.clear();
    projector.0.clear();
    graph.0 = SemanticGraph::default();
    *grid = Default::default();
    *live = Default::default();
}

pub fn refresh_semantic_graph(
    history: Res<FeatureHistoryRes>,
    music: Res<MusicInfoRes>,
    mut projector: ResMut<GraphProjectorRes>,
    mut graph: ResMut<SemanticGraphRes>,
) {
    graph.0 = projector.0.project(&history.0, music.0.as_ref());
}

/// Spawn the on-screen pause button and parent it into `hud_slot` as the
/// right child. Must run after [`game::hud::spawn`] so the HUD badge (left
/// child) is already present — child order drives `SpaceBetween` layout.
pub fn spawn_pause_button(mut commands: Commands, slot: Single<Entity, With<HudSlot>>) {
    let parent = *slot;
    commands
        .spawn((
            ChildOf(parent),
            PauseButton,
            Name::new("pause_button"),
            Button,
            Node {
                width: px(44),
                height: px(44),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|p| {
            // Two ASCII pipes as pause bars. The app's default face is Noto
            // Sans Devanagari (set in `font.rs` so Sargam renders); it lacks a
            // pause glyph (U+23F8) and renders it as tofu, but the pipe `|` is
            // present. Sized up so the bars read as a pause icon, not text.
            p.spawn((
                Text::new("||"),
                TextFont {
                    font_size: FONT_HEADER,
                    ..default()
                },
                TextColor(COLOR_TEXT_DIM),
            ));
        });
}

/// Shared pause logic: `AppState → Paused`, `HasPausedSession = true`,
/// `ResumeLocked = false`. Called by both the Escape handler and the
/// on-screen pause button so they stay in lockstep.
pub fn request_pause(
    next: &mut NextState<AppState>,
    has_paused: &mut HasPausedSession,
    resume_locked: &mut ResumeLocked,
) {
    has_paused.0 = true;
    resume_locked.0 = false;
    next.set(AppState::Paused);
}

/// Esc in InGame → Paused (stops session via OnEnter(Paused)). Marks
/// `HasPausedSession` so the main menu can offer Continue.
/// `ResumeLocked` is cleared so the Pause screen's Resume action is enabled
/// (this is a deliberate user pause, not an OS interruption).
pub fn handle_esc_in_game(
    keys: Res<ButtonInput<KeyCode>>,
    mut next: ResMut<NextState<AppState>>,
    mut has_paused: ResMut<HasPausedSession>,
    mut resume_locked: ResMut<ResumeLocked>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        request_pause(&mut next, &mut has_paused, &mut resume_locked);
    }
}

/// On-screen pause button handler — mirrors `handle_esc_in_game` for
/// touch devices that have no Escape key.
pub fn handle_pause_button(
    q: Query<&Interaction, (Changed<Interaction>, With<PauseButton>)>,
    mut next: ResMut<NextState<AppState>>,
    mut has_paused: ResMut<HasPausedSession>,
    mut resume_locked: ResMut<ResumeLocked>,
) {
    for i in q.iter() {
        if *i == Interaction::Pressed {
            request_pause(&mut next, &mut has_paused, &mut resume_locked);
        }
    }
}

/// Esc in Paused → InGame (starts a fresh session via OnEnter(InGame)).
pub fn handle_esc_paused(keys: Res<ButtonInput<KeyCode>>, mut next: ResMut<NextState<AppState>>) {
    if keys.just_pressed(KeyCode::Escape) {
        next.set(AppState::InGame);
    }
}

pub fn log_features(features: Res<LatestFeatures>, mut last: ResMut<LastFeatureHop>) {
    let Some(Features {
        hop_index,
        pitch,
        confidence,
        onset,
        breath,
        vibrato_rate,
        vibrato_depth,
        t_ms,
        ..
    }) = features.0
    else {
        return;
    };
    if last.0 == Some(hop_index) {
        return;
    }
    last.0 = Some(hop_index);
    // Debug log renders Hz for human eyes — the one place the game prints a
    // frequency. `pitch` is already a PitchLog2; `None` is unvoiced.
    let f0_str = match pitch {
        Some(p) => format!("{:7.2} Hz", p.to_hz()),
        None => "    --    ".to_string(),
    };
    let onset_marker = if onset > 0.0 { "•" } else { " " };
    info!(
        "hop={hop_index:>8}  t={t_ms:>8}ms  f0 {f0_str}  conf {confidence:>4.2}  br {breath:>4.2}  vib {vibrato_rate:>4.1}Hz/{vibrato_depth:>5.0}c  {onset_marker}"
    );
}
