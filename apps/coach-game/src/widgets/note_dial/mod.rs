//! Note-dial widget slice — N positions around a circle plus zero or more
//! needles pointing into the circle.
//!
//! `model` is the only music-aware layer: it folds frequencies, scales,
//! and tonics into slot angles, needle angles, and the hub state. `scene`
//! is the music-blind contract (components on the dial entity plus the
//! [`HubState`] enum); `systems` spawns the tree and paints. See
//! [`ARCHITECTURE.md`](../../ARCHITECTURE.md) for the music-quarantine rule.

pub mod model;
pub mod scene;
pub mod systems;

pub use model::{
    build_slots, capture_scale, hub_visual_state, is_capture_voiced, project_needle,
    CAPTURE_CONF_GATE,
};
pub use scene::{DialScale, DialSlot, DialState, HubState, Needle, NeedleStyle};
pub use systems::{hub_colors, spawn, DialHub, DialHubLabel, NoteDialRoot, DIAL_BOX_PX};
