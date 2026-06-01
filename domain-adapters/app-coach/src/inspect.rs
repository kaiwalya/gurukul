//! Adapter-side implementation of the [`EngineInspect`] port.
//!
//! Two lock-free publishers shared between the head and the data
//! plane worker:
//!
//! - **`selection_publisher`** — head writes via
//!   [`EngineInspect::set_selection`], worker reads at the top of
//!   each block.
//! - **`tap_publisher`** — worker writes the latest [`TapSnapshot`]
//!   after `process_block`, head polls via
//!   [`EngineInspect::latest_tap`].
//!
//! A `Vec<NodePortInfo>` is built once when the worker constructs
//! the engine and stored behind an `ArcSwap` so the head sees the
//! current list whenever a session is running. It's empty when no
//! engine is built (no session).

use arc_swap::ArcSwap;
use engine::Engine;
use std::sync::Arc;

use domain_ports::engine_inspect::{
    EngineInspect, NodePortInfo, PortShape, Selection, TapSnapshot,
};

/// Inner state shared by both the head-facing trait impl and the
/// data-plane worker. The worker holds a clone of this `Arc` and
/// pushes tap snapshots from inside its block loop; the head pulls
/// through the `EngineInspect` impl.
pub(crate) struct InspectShared {
    selection: ArcSwap<Option<Selection>>,
    tap: ArcSwap<Option<Arc<TapSnapshot>>>,
    /// Snapshot of every `(node, port, shape)` the engine exposes.
    /// Populated by the worker when it builds the engine; cleared
    /// when the worker exits.
    node_ports: ArcSwap<Vec<NodePortInfo>>,
}

impl InspectShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            selection: ArcSwap::from_pointee(None),
            tap: ArcSwap::from_pointee(None),
            node_ports: ArcSwap::from_pointee(Vec::new()),
        })
    }

    /// Worker-side: enumerate the engine's node/port surface and
    /// publish it for the head's picker.
    pub(crate) fn publish_node_ports(&self, engine: &Engine) {
        let mut list: Vec<NodePortInfo> = Vec::new();
        for node in engine.node_ids() {
            let ports = match engine.out_port_names(node) {
                Ok(p) => p,
                Err(_) => continue,
            };
            for port in ports {
                list.push(NodePortInfo {
                    node: node.clone(),
                    port: port.to_string(),
                    shape: classify_port(node, port),
                });
            }
        }
        self.node_ports.store(Arc::new(list));
    }

    /// Worker-side: called at the top of every block. If a selection
    /// is set and the port still exists with the expected shape,
    /// peek the engine and publish a fresh [`TapSnapshot`].
    pub(crate) fn tap_if_selected(&self, engine: &Engine, seq: u64) {
        let sel_guard = self.selection.load();
        let Some(sel) = sel_guard.as_ref() else {
            return;
        };
        let Ok(samples) = engine.peek(&sel.node, &sel.port) else {
            return;
        };
        let frames = samples.len();
        let snapshot = Arc::new(TapSnapshot {
            seq,
            shape: sel.shape,
            frames,
            samples: samples.to_vec().into_boxed_slice(),
        });
        self.tap.store(Arc::new(Some(snapshot)));
    }

    /// Worker-side: clear all published state when the engine tears
    /// down. The head observes an empty node list + no tap.
    pub(crate) fn clear(&self) {
        self.node_ports.store(Arc::new(Vec::new()));
        self.tap.store(Arc::new(None));
        // Leave `selection` alone — the head's pick persists across
        // session restarts. The next worker will pick it up if the
        // node/port still exists.
    }
}

/// Public `EngineInspect` impl handed to hosts by the
/// `new_with_inspect` factory. Wraps the same [`InspectShared`] the
/// worker is writing to.
pub(crate) struct EngineInspectImpl {
    pub(crate) shared: Arc<InspectShared>,
}

impl EngineInspect for EngineInspectImpl {
    fn list_node_ports(&self) -> Vec<NodePortInfo> {
        (**self.shared.node_ports.load()).clone()
    }

    fn set_selection(&self, sel: Option<Selection>) {
        self.shared.selection.store(Arc::new(sel));
    }

    fn latest_tap(&self) -> Option<Arc<TapSnapshot>> {
        (**self.shared.tap.load()).clone()
    }
}

/// Best-effort shape classification for a `(node, port)` pair given
/// what we know about the nodes in `coach.json`. Unknown ports fall
/// through to `Control` — a generic decimal readout is the least
/// surprising default. As new node types appear, extend this table.
fn classify_port(_node: &str, port: &str) -> PortShape {
    match port {
        // PitchYin emits f0 (sample-and-hold Hz).
        "f0" => PortShape::FeatureHz,
        // Onset emits short impulses.
        "onset" => PortShape::FeatureEvent,
        // Breath / vibrato detectors emit slowly-varying scalars.
        "breath" | "rate" | "depth" => PortShape::Control,
        _ => PortShape::Control,
    }
}
