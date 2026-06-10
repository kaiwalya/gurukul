//! InGame time-graph feeder.
//!
//! This bridges the semantic graph model to the normalized scene
//! consumed by the UI widget.

use crate::game::{InGameRoot, SemanticGraphRes};
use crate::widgets::time_graph::{project_scene, spawn as spawn_widget, TimeGraphSceneRes};
use bevy::prelude::*;

pub fn spawn(commands: Commands, root: Single<Entity, With<InGameRoot>>) {
    spawn_widget(commands, *root);
}

pub fn refresh_scene(graph: Res<SemanticGraphRes>, mut scene: ResMut<TimeGraphSceneRes>) {
    let next = project_scene(&graph.0);
    if scene.0 != next {
        scene.0 = next;
    }
}
