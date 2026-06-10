//! Time-graph scene: the render-facing contract.
//!
//! Already-projected data only — lane-local normalized coordinates in
//! `[0, 1]`, never frequencies or times. Music-blind. Produced by
//! [`model::project_scene`](super::model::project_scene) and consumed by
//! [`systems`](super::systems).

use bevy::prelude::*;
use bevy::ui::ComputedNode;

/// A node size in **logical** pixels — the frame `px(...)` / `Val::Px`
/// speak. `ComputedNode::size()` is *physical* pixels (a 2× display
/// doubles every coordinate), so a measured size must be converted before
/// it can be fed back into a `Node`. This newtype is the single place that
/// conversion happens: the field is private and the *only* constructor is
/// [`LogicalSize::from_computed`], so a consumer cannot receive an
/// unconverted (physical) size. A raw `Vec2` in the capture resource moved
/// that disagreement from compile time to a line on the wrong side of the
/// screen — see `ARCHITECTURE.md` ("unit-of-measure is part of the
/// contract"). There is deliberately no test-only constructor: faking a
/// size here reopens exactly the hole the newtype closes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalSize(Vec2);

impl LogicalSize {
    /// Convert a node's measured (physical) size into logical pixels. This
    /// is the sole way to build a `LogicalSize`, so every value of this
    /// type has been through the `inverse_scale_factor` conversion exactly
    /// once. `src/ui.rs`'s scroll clamp is the in-crate precedent for the
    /// same `physical × inverse_scale_factor` step.
    pub fn from_computed(node: &ComputedNode) -> Self {
        Self(node.size() * node.inverse_scale_factor())
    }

    /// The size in logical pixels, ready to feed back into `Node` layout.
    pub fn get(self) -> Vec2 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedTracePoint {
    pub point: NormalizedPoint,
    pub confidence: f32,
    pub vibrato_rate: f32,
    pub vibrato_depth: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedTraceSegment {
    pub points: Vec<NormalizedTracePoint>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedGrooveLine {
    pub y: f32,
    pub slot: usize,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedOnsetTick {
    pub x: f32,
    pub strength: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedBreathSpan {
    pub x0: f32,
    pub x1: f32,
    pub peak: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeGraphScene {
    pub pitch_segments: Vec<NormalizedTraceSegment>,
    pub grooves: Vec<NormalizedGrooveLine>,
    pub onset_ticks: Vec<NormalizedOnsetTick>,
    pub breath_spans: Vec<NormalizedBreathSpan>,
}

#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct TimeGraphSceneRes(pub TimeGraphScene);

/// Pitch lane's measured size, captured after `PostLayout` and fed back
/// into the next frame's trace painting. Held as a [`LogicalSize`] so the
/// physical→logical conversion happens once, at capture, and the paint
/// system cannot accidentally consume a physical size.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct TimeGraphPitchLaneSize(pub Option<LogicalSize>);
