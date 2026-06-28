//! Layout-aware non-overlap guarantee: the three InGame widgets (HUD, time
//! graph, dial) are pairwise disjoint by construction once the flex
//! scaffold owns the partition.
//!
//! Uses the real layout schedule at 2× scale (the only level that
//! populates ComputedNode + UiGlobalTransform). Mirrors the GlobalRect
//! helper from time_graph_layout.rs.

mod common;

use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};
use coach_game::game::InGameRoot;
use coach_game::menu::main_menu::NewGameButton;
use coach_game::widgets::hud::HudBadge;
use coach_game::widgets::note_dial::{NoteDialRoot, DIAL_BOX_PX};
use coach_game::widgets::time_graph::TimeGraphRoot;
use common::{build_layout_test_app, pump, pump_layout};
use domain_ports::app_coach::{CoachEvent, FeatureSnapshot, MusicInfo};
use domain_ports::pitch::{PitchLog2, PitchLog2Interval};
use domain_ports::scale::{Scale, ScaleIntervals};
use domain_ports::tuning::{Tuning, TuningAbsolute, TuningKind};

/// Axis-aligned rect in physical pixels from global transform + computed size.
#[derive(Debug, Clone, Copy)]
struct GlobalRect {
    min: Vec2,
    max: Vec2,
}

impl GlobalRect {
    fn from_node(xform: &UiGlobalTransform, node: &ComputedNode) -> Self {
        let half = node.size() * 0.5;
        let affine = xform.affine();
        let corners = [
            Vec2::new(-half.x, -half.y),
            Vec2::new(half.x, -half.y),
            Vec2::new(half.x, half.y),
            Vec2::new(-half.x, half.y),
        ]
        .map(|c| affine.transform_point2(c));
        let min = corners.iter().copied().reduce(Vec2::min).unwrap();
        let max = corners.iter().copied().reduce(Vec2::max).unwrap();
        Self { min, max }
    }

    fn within(&self, outer: &GlobalRect) -> bool {
        const EPS: f32 = 1.0;
        self.min.x >= outer.min.x - EPS
            && self.min.y >= outer.min.y - EPS
            && self.max.x <= outer.max.x + EPS
            && self.max.y <= outer.max.y + EPS
    }

    /// True when this rect and `other` do not overlap (disjoint).
    fn disjoint(&self, other: &GlobalRect) -> bool {
        const EPS: f32 = 1.0;
        self.max.x <= other.min.x + EPS
            || other.max.x <= self.min.x + EPS
            || self.max.y <= other.min.y + EPS
            || other.max.y <= self.min.y + EPS
    }

    fn size(&self) -> Vec2 {
        self.max - self.min
    }
}

fn publish_music(fake: &common::FakeCoach, info: MusicInfo) {
    let mut state = fake.inner.lock().unwrap();
    state.music_info = Some(info);
    state
        .pending_events
        .push(CoachEvent::MusicSessionConfigured { scale: info.scale });
}

fn music(octave: i32) -> MusicInfo {
    let tuning = TuningAbsolute::new(TuningKind::TwelveTet.intervals(), PitchLog2Interval(0.17));
    MusicInfo {
        scale: Scale::new(
            ScaleIntervals::from_widths(&[2, 2, 1, 2, 2, 2, 1]),
            tuning.shift_up(3),
            octave,
        ),
    }
}

fn enter_in_game(app: &mut App) {
    app.world_mut()
        .spawn((Button, NewGameButton, Interaction::Pressed));
    pump(app);
    assert_eq!(
        *app.world()
            .resource::<State<coach_game::state::AppState>>()
            .get(),
        coach_game::state::AppState::InGame,
        "NewGame must transition into InGame"
    );
}

fn widget_rect<M: Component>(world: &mut World) -> GlobalRect {
    let (xform, node) = world
        .query_filtered::<(&UiGlobalTransform, &ComputedNode), With<M>>()
        .single(world)
        .expect("widget has a global transform and computed node");
    GlobalRect::from_node(xform, node)
}

#[test]
fn ingame_widgets_are_pairwise_disjoint_and_fill_viewport() {
    let (mut app, fake) = build_layout_test_app();
    pump(&mut app);
    enter_in_game(&mut app);

    // Feed music so the dial slots are populated.
    publish_music(&fake, music(4));
    // Feed a snapshot so there is a pitch reading.
    {
        let mut state = fake.inner.lock().unwrap();
        state.pending_features = vec![FeatureSnapshot {
            hop_index: 0,
            f0_hz: PitchLog2(8.0).to_hz(),
            confidence: 0.8,
            onset: 0.0,
            breath: 0.0,
            vibrato_rate: 0.0,
            vibrato_amplitude: 0.0,
            vibrato_phase: 0.0,
            vibrato_t_ms: 1_000,
            t_ms: 1_000,
        }];
    }
    pump_layout(&mut app);

    let world = app.world_mut();

    let root_rect = widget_rect::<InGameRoot>(world);
    let hud_rect = widget_rect::<HudBadge>(world);
    let graph_rect = widget_rect::<TimeGraphRoot>(world);
    let dial_rect = widget_rect::<NoteDialRoot>(world);

    // InGameRoot fills the logical viewport (1600×1200 physical at 2× = 800×600 logical,
    // but ComputedNode is in physical pixels so 1600×1200).
    let root_size = root_rect.size();
    assert!(
        root_size.x > 0.0 && root_size.y > 0.0,
        "InGameRoot must have a non-degenerate computed size, got {root_size:?}"
    );
    assert!(
        root_size.x >= 1590.0,
        "InGameRoot width must fill the viewport, got {:.0}",
        root_size.x
    );
    assert!(
        root_size.y >= 1190.0,
        "InGameRoot height must fill the viewport, got {:.0}",
        root_size.y
    );

    // All three widget rects are pairwise disjoint.
    assert!(
        hud_rect.disjoint(&graph_rect),
        "HUD and time-graph overlap:\n  hud = {hud_rect:?}\n  graph = {graph_rect:?}"
    );
    assert!(
        hud_rect.disjoint(&dial_rect),
        "HUD and dial overlap:\n  hud = {hud_rect:?}\n  dial = {dial_rect:?}"
    );
    assert!(
        graph_rect.disjoint(&dial_rect),
        "time-graph and dial overlap:\n  graph = {graph_rect:?}\n  dial = {dial_rect:?}"
    );

    // Time-graph lies inside the InGameRoot (basic containment).
    assert!(
        graph_rect.within(&root_rect),
        "time-graph escapes InGameRoot:\n  graph = {graph_rect:?}\n  root = {root_rect:?}"
    );

    // Dial shell computed size matches its intrinsic box (catches the rail squeezing it).
    // ComputedNode size is in physical pixels; DIAL_BOX_PX is logical. At 2× scale: physical = 2 × logical.
    let dial_size = dial_rect.size();
    let expected_physical = DIAL_BOX_PX * 2.0;
    assert!(
        (dial_size.x - expected_physical).abs() < 2.0,
        "dial shell width should be {expected_physical} physical px (2× logical {DIAL_BOX_PX}), got {:.0}",
        dial_size.x
    );
    assert!(
        (dial_size.y - expected_physical).abs() < 2.0,
        "dial shell height should be {expected_physical} physical px (2× logical {DIAL_BOX_PX}), got {:.0}",
        dial_size.y
    );
}
