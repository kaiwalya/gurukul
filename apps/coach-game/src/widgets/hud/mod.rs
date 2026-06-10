//! HUD widget slice — a top-left math-view panel of the current tonality.
//!
//! `model` projects [`MusicInfo`](domain_ports::app_coach::MusicInfo) into
//! display rows; `scene` is the [`HudSceneRes`] text-row contract (a
//! Resource); `systems` spawns the panel and syncs its text. See
//! [`ARCHITECTURE.md`](../../ARCHITECTURE.md) for the music-quarantine rule.

pub mod model;
pub mod scene;
pub mod systems;

pub use model::int_row;
pub use scene::HudSceneRes;
pub use systems::{spawn, HudBadge, HudDegRow};
