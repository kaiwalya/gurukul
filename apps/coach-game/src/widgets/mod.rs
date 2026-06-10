//! InGame UI widget slices.
//!
//! Each widget is a vertical slice (`model` / `scene` / `systems`) that
//! owns the whole path from a domain fact to pixels. The slice is
//! **domain-aware as a whole**, but that awareness is quarantined to the
//! `model` layer: `model` turns music into geometry, and `scene` + `systems`
//! below it are music-blind. This replaces the earlier "widgets are dumb
//! geometry; callers translate" rule — the music-blindness did not die, it
//! moved down one level to the `model`/`scene` seam.
//!
//! See [`ARCHITECTURE.md`](../ARCHITECTURE.md) for the slice doctrine,
//! marker ownership, and scene-shape rules; [`CONTRIBUTING.md`](../CONTRIBUTING.md)
//! for how to build one.

pub mod hud;
pub mod note_dial;
pub mod scale_picker;
pub mod time_graph;
