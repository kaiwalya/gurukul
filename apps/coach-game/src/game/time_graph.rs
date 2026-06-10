//! InGame time-graph feeder.
//!
//! This bridges the semantic graph model to the normalized scene
//! consumed by the UI widget.

use crate::game::{InGameRoot, SemanticGraphRes};
use crate::widgets::time_graph::{
    project_scene, spawn as spawn_widget, TimeGraphGridSceneRes, TimeGraphLiveSceneRes,
};
use bevy::prelude::*;

pub fn spawn(mut commands: Commands, root: Single<Entity, With<InGameRoot>>) {
    spawn_widget(&mut commands, *root);
}

/// Project the semantic graph once, then distribute the single scene into the
/// two cadence-split resources. The repaint cadence is a presentation concern,
/// so the split happens here at the glue, not in the pure model. Grids are
/// value-gated with `set_if_neq` — change detection is write-based, so an
/// unconditional write would fire the slow painter every frame and void the
/// split. The live resource is written unconditionally (it scrolls every frame).
pub fn refresh_scene(
    graph: Res<SemanticGraphRes>,
    mut grid: ResMut<TimeGraphGridSceneRes>,
    mut live: ResMut<TimeGraphLiveSceneRes>,
) {
    let next = project_scene(&graph.0);
    grid.set_if_neq(TimeGraphGridSceneRes {
        grooves: next.grooves,
    });
    *live = TimeGraphLiveSceneRes {
        pitch_segments: next.pitch_segments,
        onset_ticks: next.onset_ticks,
        breath_spans: next.breath_spans,
    };
}
