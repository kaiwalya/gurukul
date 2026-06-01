//! Engine inspection port: lets a developer-facing head pick any
//! `(node, out-port)` in a live engine and watch its most recent
//! block. Not part of the production [`AppCoach`](crate::app_coach)
//! contract — production heads ignore this port; only hosts with a
//! debug pane need it.
//!
//! # Shape
//!
//! - The head writes a [`Selection`] via [`EngineInspect::set_selection`].
//!   `None` clears.
//! - The audio worker (whoever owns the engine) reads the selection at
//!   the top of each block, copies the matching port's samples into a
//!   fresh [`TapSnapshot`], and publishes it through
//!   [`EngineInspect::latest_tap`].
//! - There are no subscribe / unsubscribe verbs. "Selection is whatever
//!   is in the slot right now." Cycling selections is racy-safe — the
//!   worker just catches up on the next block.
//!
//! Implementations are expected to use `ArcSwap` (or equivalent
//! wait-free publishing) so head and worker never block each other.

use std::sync::Arc;

/// Trait the head depends on. The adapter that owns the engine
/// implements this; heads receive an `Arc<dyn EngineInspect>` from
/// the adapter factory.
///
/// **`Send + Sync`** because the head and the audio worker live on
/// different threads and both hold the `Arc`.
pub trait EngineInspect: Send + Sync {
    /// Every `(node, out-port)` the engine exposes, paired with the
    /// shape we expect that port to carry. Built once when the
    /// engine is constructed; doesn't change mid-session.
    ///
    /// Returns an empty slice when no engine is built (no session
    /// running). Heads should reload on session-state transitions
    /// rather than caching across sessions.
    fn list_node_ports(&self) -> Vec<NodePortInfo>;

    /// Atomically replace the current tap selection. `None` clears.
    /// The next worker block will observe the change.
    ///
    /// Wait-free; safe to call from the UI thread at any cadence.
    fn set_selection(&self, sel: Option<Selection>);

    /// Most recent [`TapSnapshot`] published by the worker, or `None`
    /// if there's no selection, no engine, or the first tap hasn't
    /// landed yet.
    ///
    /// Wait-free; the head polls at its UI cadence (~30Hz). The
    /// snapshot's `seq` strictly increases so a head can detect
    /// stalls.
    fn latest_tap(&self) -> Option<Arc<TapSnapshot>>;
}

/// What `(node, port)` does the head want to inspect, and what shape
/// does it expect? The shape lets the renderer pick its widget
/// *before* the first tap arrives, so the panel doesn't flash the
/// wrong widget on selection change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub node: String,
    pub port: String,
    pub shape: PortShape,
}

/// One row in the node-port enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePortInfo {
    pub node: String,
    pub port: String,
    pub shape: PortShape,
}

/// How the head should interpret the samples in a [`TapSnapshot`].
///
/// Ports in the dsp engine all carry `f32` buffers, but what those
/// floats *mean* varies. The shape lets the head pick a widget:
///
/// - **Audio** — block of audio samples in roughly `[-1, 1]`. Render
///   as a waveform / mini-scope.
/// - **FeatureHz** — sample-and-hold detector output (e.g. `f0_hz`).
///   The last sample in the block is the latest estimate; `0.0`
///   means unvoiced / no detection. Render as a big number.
/// - **FeatureEvent** — short impulse train; `max |x|` over the block
///   is the strength of any event that fired this block. Render as
///   a bar / blip.
/// - **Control** — slowly-varying scalar (gain, smoothed envelope,
///   detector confidence). Render as a precise decimal readout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortShape {
    Audio,
    FeatureHz,
    FeatureEvent,
    Control,
}

/// A coherent snapshot of one engine port's most recent block.
///
/// `samples` is the copy the worker made when it tapped — the head
/// owns it for as long as it holds the `Arc`. `seq` strictly
/// increases so a UI poll can tell "same tap as last frame" from
/// "fresh tap." `shape` mirrors the selection's shape and lets the
/// renderer reassert on each frame.
pub struct TapSnapshot {
    pub seq: u64,
    pub shape: PortShape,
    /// Number of valid frames in `samples`. Always ≤ `samples.len()`.
    pub frames: usize,
    pub samples: Box<[f32]>,
}
