//! Time-graph widget slice: a scrolling pitch/event panel.
//!
//! `model` projects the [semantic graph](crate::semantic_graph) into a
//! normalized [scene]; `systems` spawns the lane tree and paints it.
//! Music-awareness is quarantined to `model` — see
//! [`ARCHITECTURE.md`](../../ARCHITECTURE.md).

pub mod model;
pub mod scene;
pub mod systems;

pub use model::project_scene;
pub use scene::{
    LogicalSize, NormalizedBreathSpan, NormalizedGrooveLine, NormalizedOnsetTick, NormalizedPoint,
    NormalizedTracePoint, NormalizedTraceSegment, TimeGraphGridSceneRes, TimeGraphLiveSceneRes,
    TimeGraphPitchLanePhysRect, TimeGraphPitchLaneScale, TimeGraphPitchLaneSize, TimeGraphScene,
};
pub use systems::{
    spawn, BreathSpanMarker, GridlineLayer, GridlineMeshEntity, GrooveLineMarker, LaneBgMeshEntity,
    OnsetTickMarker, TimeGraphEventsLane, TimeGraphPitchLane, TimeGraphRoot, TraceMaterial,
    TraceMeshCamera, TraceMeshEntity,
};
