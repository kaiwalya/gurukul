//! Scale-picker widget slice — an overlay listing one row per known scale
//! shape.
//!
//! `model` projects the catalogue into row labels and computes the
//! selected [`Scale`](domain_ports::scale::Scale); `scene` is the
//! music-blind row contract; `systems` spawns the overlay tree and syncs
//! rows. Open/closed visibility stays in glue, not the scene. See
//! [`ARCHITECTURE.md`](../../ARCHITECTURE.md) for the music-quarantine rule.

pub mod model;
pub mod scene;
pub mod systems;

pub use model::{row_labels, select_scale, shape_label};
pub use scene::{PickerRow, PickerRows};
pub use systems::{
    populate_rows, spawn, ScalePickerCloseButton, ScalePickerRoot, ScalePickerRows, ScaleRow,
};
