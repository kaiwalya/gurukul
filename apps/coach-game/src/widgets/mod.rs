//! Reusable visual primitives.
//!
//! Widgets are dumb — they own geometry and per-frame state, not
//! musical semantics. Callers translate their domain (raga, scale,
//! pitch) into widget input (angles, slot states, needle positions).

pub mod note_dial;
