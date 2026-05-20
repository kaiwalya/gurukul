use crate::node::{Node, NodeError, PortType};
use crate::registry::NodeRegistry;
use crate::world::{Connection, World, boundary_port_id_valid};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("unknown node type '{0}'")]
    UnknownNodeType(String),
    #[error("cycle detected in graph")]
    CycleDetected,
    #[error("invalid port path '{0}': expected '<node_id>.<port_name>'")]
    InvalidPortPath(String),
    #[error("node '{0}' not found")]
    NodeNotFound(String),
    #[error("port '{0}' not found on node '{1}'")]
    PortNotFound(String, String),
    #[error("invalid node id '{0}': {1}")]
    InvalidNodeId(String, String),
    #[error("invalid boundary port id '{0}': must match ^[a-z][a-z0-9_]*$")]
    InvalidBoundaryPortId(String),
    #[error("boundary port id '{0}' conflicts with a node id")]
    BoundaryPortIdConflict(String),
    #[error("boundary port '{0}' not found")]
    BoundaryPortNotFound(String),
    #[error("boundary in-port '{0}' has mismatched destination types: {1:?}")]
    BoundaryTypeMismatch(String, Vec<String>),
    #[error("boundary out-port '{0}' has {1} incoming edge(s), expected {2}")]
    BoundaryPortInvalidEdgeCount(String, usize, usize),
    #[error("boundary in-port '{0}' has no outgoing edges")]
    BoundaryInPortNoDestinations(String),
    #[error("boundary in-port '{0}' appears as edge destination (not allowed)")]
    BoundaryInPortUsedAsDestination(String),
    #[error("boundary out-port '{0}' appears as edge source (not allowed)")]
    BoundaryOutPortUsedAsSource(String),
    #[error(
        "boundary in-ports {in_port_ids:?} both target destination '{node_id}.{port}': \
         only one in-port may feed a given node port"
    )]
    BoundaryInPortDestinationConflict {
        node_id: String,
        port: String,
        in_port_ids: Vec<String>,
    },
    #[error("edge from boundary port '{0}' to boundary port '{1}' is not allowed")]
    BoundaryToBoundary(String, String),
}

/// Parsed edge endpoint. A bare string (no dot) is a boundary port; a dotted
/// string is a node port.
pub enum Endpoint<'a> {
    Boundary(&'a str),
    Node(&'a str, &'a str),
}

/// Parse an edge endpoint string.
///
/// - Contains `.` → `Node(node_id, port_name)`.
/// - No `.`       → `Boundary(id)`.
pub fn parse_endpoint(s: &str) -> Endpoint<'_> {
    if let Some((node_id, port_name)) = s.split_once('.') {
        Endpoint::Node(node_id, port_name)
    } else {
        Endpoint::Boundary(s)
    }
}

/// Parse a dotted `node_id.port_name` path (node-to-node connections only).
/// Returns `Err` if the string has no dot.
pub fn parse_port_path(path: &str) -> Result<(&str, &str), EngineError> {
    path.split_once('.')
        .ok_or_else(|| EngineError::InvalidPortPath(path.to_string()))
}

/// Validate a node id: must be non-empty.
fn validate_node_id(id: &str) -> Result<(), EngineError> {
    if id.is_empty() {
        return Err(EngineError::InvalidNodeId(
            id.to_string(),
            "id must not be empty".to_string(),
        ));
    }
    Ok(())
}

/// Opaque handle for a boundary input port. Resolved once at build time.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct InPortHandle(pub(crate) u32);

impl InPortHandle {
    /// Construct from a raw `u32`. Caller is responsible for ensuring the value
    /// originated from `resolve_in_port` on the same engine instance.
    pub fn from_raw(v: u32) -> Self {
        Self(v)
    }

    /// Extract the underlying `u32`.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Opaque handle for a boundary output port. Resolved once at build time.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OutPortHandle(pub(crate) u32);

impl OutPortHandle {
    /// Construct from a raw `u32`. Caller is responsible for ensuring the value
    /// originated from `resolve_out_port` on the same engine instance.
    pub fn from_raw(v: u32) -> Self {
        Self(v)
    }

    /// Extract the underlying `u32`.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Fully-resolved boundary port spec (id, name, description, derived type).
#[derive(Debug, Clone)]
pub struct BoundaryPortSpec {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub ty: PortType,
}

/// (node_idx, port_idx) — resolved at build time for zero-cost hot-path access.
type NodePortRef = (usize, usize);

/// A running engine: instantiated nodes in topological order, ready to process.
pub struct Engine {
    node_index: HashMap<String, usize>,
    topo_order_idx: Vec<usize>,
    topo_order_ids: Vec<String>,
    nodes: Vec<(String, Box<dyn Node>)>,
    // (src_node_idx, src_port_idx, dst_node_idx, dst_port_idx) — node-to-node only.
    connections: Vec<(usize, usize, usize, usize)>,
    block_size: usize,
    sample_rate: u32,
    // Per-node per-output-port buffers, allocated once.
    output_buffers: Vec<Vec<Vec<f32>>>,
    // Per-node per-input-port buffers, allocated once and reused across calls.
    input_buffers: Vec<Vec<Vec<f32>>>,
    // Per (node_idx, out_port_idx): does anything downstream consume it?
    output_has_downstream: Vec<Vec<bool>>,
    // Per-node list of output port names, in declaration order.
    output_port_names: Vec<Vec<String>>,
    // Pre-allocated scratches for materialising slice-of-slice views without
    // allocating each block. See safety comments in process_block().
    input_ptr_scratch: Vec<Vec<(*const f32, usize)>>,
    output_ptr_scratch: Vec<Vec<(*mut f32, usize)>>,

    // --- Boundary port support ---
    in_port_specs: Vec<BoundaryPortSpec>,
    out_port_specs: Vec<BoundaryPortSpec>,
    // Pre-allocated buffers sized to block_size. Index == handle value.
    in_port_buffers: Vec<Vec<f32>>,
    out_port_buffers: Vec<Vec<f32>>,
    // For each in-port: list of (dst_node_idx, dst_port_idx) destinations.
    in_port_destinations: Vec<Vec<NodePortRef>>,
    // For each out-port: the single source (node_idx, port_idx).
    out_port_sources: Vec<NodePortRef>,
    // Number of frames produced by the most recent process_block call.
    // Slices returned by out_port() and peek() are trimmed to this length so
    // callers never observe stale samples from a prior partial block.
    last_n_frames: usize,
}

// SAFETY: the pointer scratches alias into `input_buffers` / `output_buffers`,
// which the Engine owns and never reallocates after build(). They are only
// dereferenced inside `process_block`, which takes `&mut self`, so no concurrent
// access is possible. The Engine is otherwise Send (all Node trait objects are
// Send); these raw-pointer fields do not change that.
unsafe impl Send for Engine {}

impl Engine {
    /// Build and prepare an Engine from a World definition.
    pub fn build(
        world: &World,
        registry: &NodeRegistry,
        sample_rate: u32,
        block_size: usize,
    ) -> Result<Self, EngineError> {
        // --- Validate node ids ---
        for node_def in &world.nodes {
            validate_node_id(&node_def.id)?;
        }

        // --- Validate boundary port ids ---
        for bp in world.in_ports.iter().chain(world.out_ports.iter()) {
            if !boundary_port_id_valid(&bp.id) {
                return Err(EngineError::InvalidBoundaryPortId(bp.id.clone()));
            }
        }

        // --- Build node id set and check no collision with boundary port ids ---
        let node_id_set: HashSet<&str> = world.nodes.iter().map(|n| n.id.as_str()).collect();
        let mut all_boundary_ids: HashSet<&str> = HashSet::new();
        for bp in world.in_ports.iter().chain(world.out_ports.iter()) {
            if node_id_set.contains(bp.id.as_str()) {
                return Err(EngineError::BoundaryPortIdConflict(bp.id.clone()));
            }
            all_boundary_ids.insert(&bp.id);
        }

        // --- Instantiate nodes ---
        let mut nodes: Vec<(String, Box<dyn Node>)> = Vec::new();
        let mut node_index: HashMap<String, usize> = HashMap::new();

        for node_def in &world.nodes {
            if !registry.contains(&node_def.ty) {
                return Err(EngineError::UnknownNodeType(node_def.ty.clone()));
            }
            let instance = registry.create(&node_def.ty, &node_def.params).unwrap();
            node_index.insert(node_def.id.clone(), nodes.len());
            nodes.push((node_def.id.clone(), instance));
        }

        // --- Pre-compute per-node port declarations ---
        let node_ports: Vec<(Vec<crate::node::PortSpec>, Vec<crate::node::PortSpec>)> = world
            .nodes
            .iter()
            .map(|nd| registry.ports_for_params(&nd.ty, &nd.params).unwrap())
            .collect();

        // --- Build in-port / out-port id sets for validation ---
        let in_port_ids: HashSet<&str> = world.in_ports.iter().map(|p| p.id.as_str()).collect();
        let out_port_ids: HashSet<&str> = world.out_ports.iter().map(|p| p.id.as_str()).collect();

        // --- Validate edge endpoint semantics ---
        // Boundary in-ports are write-only (host→engine); they must not appear as edge
        // destinations. Boundary out-ports are read-only (engine→host); they must not
        // appear as edge sources. Boundary→boundary edges are never meaningful.
        for conn in &world.connections {
            // Reject boundary→boundary edges before the directional checks so the
            // error message is precise rather than surfacing as two unrelated errors.
            if let (Endpoint::Boundary(from_id), Endpoint::Boundary(to_id)) =
                (parse_endpoint(&conn.from), parse_endpoint(&conn.to))
            {
                return Err(EngineError::BoundaryToBoundary(
                    from_id.to_string(),
                    to_id.to_string(),
                ));
            }

            if let Endpoint::Boundary(id) = parse_endpoint(&conn.to)
                && in_port_ids.contains(id)
            {
                return Err(EngineError::BoundaryInPortUsedAsDestination(id.to_string()));
            }
            if let Endpoint::Boundary(id) = parse_endpoint(&conn.from)
                && out_port_ids.contains(id)
            {
                return Err(EngineError::BoundaryOutPortUsedAsSource(id.to_string()));
            }
        }

        // --- Resolve node-to-node connections to index pairs ---
        let mut connections: Vec<(usize, usize, usize, usize)> = Vec::new();
        for conn in &world.connections {
            let from_ep = parse_endpoint(&conn.from);
            let to_ep = parse_endpoint(&conn.to);

            // Only node→node edges go into the connections table.
            // Boundary edges are resolved separately below.
            if let (Endpoint::Node(src_id, src_port), Endpoint::Node(dst_id, dst_port)) =
                (&from_ep, &to_ep)
            {
                let src_idx = *node_index
                    .get(*src_id)
                    .ok_or_else(|| EngineError::NodeNotFound((*src_id).to_string()))?;
                let dst_idx = *node_index
                    .get(*dst_id)
                    .ok_or_else(|| EngineError::NodeNotFound((*dst_id).to_string()))?;

                let (_, src_outputs) = &node_ports[src_idx];
                let src_port_idx = src_outputs
                    .iter()
                    .position(|p| p.name == *src_port)
                    .ok_or_else(|| {
                        EngineError::PortNotFound((*src_port).to_string(), (*src_id).to_string())
                    })?;

                let (dst_inputs, _) = &node_ports[dst_idx];
                let dst_port_idx = dst_inputs
                    .iter()
                    .position(|p| p.name == *dst_port)
                    .ok_or_else(|| {
                        EngineError::PortNotFound((*dst_port).to_string(), (*dst_id).to_string())
                    })?;

                connections.push((src_idx, src_port_idx, dst_idx, dst_port_idx));
            }
        }

        // --- Topological sort (only node ids, boundary ports are not nodes) ---
        // For topo purposes boundary-in-port edges are roots; boundary-out-port
        // edges are leaves. The sort only touches node→node connections.
        let node_connections_only: Vec<Connection> = world
            .connections
            .iter()
            .filter(|c| {
                matches!(parse_endpoint(&c.from), Endpoint::Node(_, _))
                    && matches!(parse_endpoint(&c.to), Endpoint::Node(_, _))
            })
            .cloned()
            .collect();

        let topo_order = topo_sort(
            &world.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>(),
            &node_connections_only,
            &node_index,
        )?;

        // --- Allocate node buffers ---
        let mut output_buffers: Vec<Vec<Vec<f32>>> = Vec::with_capacity(nodes.len());
        let mut input_buffers: Vec<Vec<Vec<f32>>> = Vec::with_capacity(nodes.len());
        for (inputs, outputs) in &node_ports {
            output_buffers.push(outputs.iter().map(|_| vec![0.0f32; block_size]).collect());
            input_buffers.push(inputs.iter().map(|_| vec![0.0f32; block_size]).collect());
        }

        // --- Per-output-port downstream flag ---
        let mut output_has_downstream: Vec<Vec<bool>> = node_ports
            .iter()
            .map(|(_, outputs)| vec![false; outputs.len()])
            .collect();
        for &(s_node, s_port, _, _) in &connections {
            output_has_downstream[s_node][s_port] = true;
        }

        // --- Persist output port names for peek() lookups ---
        let output_port_names: Vec<Vec<String>> = node_ports
            .iter()
            .map(|(_, outputs)| outputs.iter().map(|p| p.name.to_string()).collect())
            .collect();

        // --- Pre-size pointer scratches ---
        let input_ptr_scratch: Vec<Vec<(*const f32, usize)>> = input_buffers
            .iter()
            .map(|node_inputs| vec![(std::ptr::null(), 0usize); node_inputs.len()])
            .collect();
        let output_ptr_scratch: Vec<Vec<(*mut f32, usize)>> = output_buffers
            .iter()
            .map(|node_outputs| vec![(std::ptr::null_mut(), 0usize); node_outputs.len()])
            .collect();

        // --- Resolve topo order ids to indices ---
        let topo_order_idx: Vec<usize> = topo_order.iter().map(|id| node_index[id]).collect();

        // --- Resolve boundary in-ports ---
        let mut in_port_specs: Vec<BoundaryPortSpec> = Vec::new();
        let mut in_port_buffers: Vec<Vec<f32>> = Vec::new();
        let mut in_port_destinations: Vec<Vec<NodePortRef>> = Vec::new();

        for bp in &world.in_ports {
            // Collect all destinations for this boundary in-port.
            let mut dests: Vec<NodePortRef> = Vec::new();
            let mut types_seen: Vec<PortType> = Vec::new();

            for conn in &world.connections {
                if let Endpoint::Boundary(src_id) = parse_endpoint(&conn.from) {
                    if src_id != bp.id {
                        continue;
                    }
                    // from == this in-port; resolve destination.
                    match parse_endpoint(&conn.to) {
                        Endpoint::Node(dst_id, dst_port) => {
                            let dst_idx = *node_index
                                .get(dst_id)
                                .ok_or_else(|| EngineError::NodeNotFound(dst_id.to_string()))?;
                            let (dst_inputs, _) = &node_ports[dst_idx];
                            let dst_port_idx = dst_inputs
                                .iter()
                                .position(|p| p.name == dst_port)
                                .ok_or_else(|| {
                                EngineError::PortNotFound(dst_port.to_string(), dst_id.to_string())
                            })?;
                            let ty = dst_inputs[dst_port_idx].ty.clone();
                            dests.push((dst_idx, dst_port_idx));
                            types_seen.push(ty);
                        }
                        Endpoint::Boundary(_) => {
                            // Boundary→boundary edges are rejected before this loop.
                        }
                    }
                }
            }

            if dests.is_empty() {
                return Err(EngineError::BoundaryInPortNoDestinations(bp.id.clone()));
            }

            // All destinations must share one PortType.
            // Also verify no two destinations point to the same (node_idx, port_idx)
            // within this in-port's fan-out. Cross-in-port conflicts are checked below.
            let first_ty = types_seen[0].clone();
            if !types_seen.iter().all(|t| *t == first_ty) {
                let type_names: Vec<String> = types_seen.iter().map(|t| format!("{t:?}")).collect();
                return Err(EngineError::BoundaryTypeMismatch(bp.id.clone(), type_names));
            }

            // Mark destinations as having upstream (prevent zeroing their accumulator from
            // swallowing boundary input — actually handled in staging, not here).
            // We do need to set output_has_downstream for any node whose output feeds
            // a boundary out-port later. Nothing to do here.

            in_port_specs.push(BoundaryPortSpec {
                id: bp.id.clone(),
                name: bp.name.clone(),
                description: bp.description.clone(),
                ty: first_ty,
            });
            in_port_buffers.push(vec![0.0f32; block_size]);
            in_port_destinations.push(dests);
        }

        // --- Reject two boundary in-ports targeting the same destination ---
        // Build a map from (dst_node_idx, dst_port_idx) → Vec of in-port ids.
        // A silent last-writer-wins overwrite would be the worst possible semantic.
        {
            let mut dest_to_in_ports: HashMap<NodePortRef, Vec<String>> = HashMap::new();
            for (spec, dests) in in_port_specs.iter().zip(in_port_destinations.iter()) {
                for &dest in dests {
                    dest_to_in_ports
                        .entry(dest)
                        .or_default()
                        .push(spec.id.clone());
                }
            }
            for ((dst_node_idx, dst_port_idx), in_port_ids) in &dest_to_in_ports {
                if in_port_ids.len() > 1 {
                    let node_id = nodes[*dst_node_idx].0.clone();
                    let (dst_inputs, _) = &node_ports[*dst_node_idx];
                    let port = dst_inputs[*dst_port_idx].name.to_string();
                    return Err(EngineError::BoundaryInPortDestinationConflict {
                        node_id,
                        port,
                        in_port_ids: in_port_ids.clone(),
                    });
                }
            }
        }

        // --- Resolve boundary out-ports ---
        let mut out_port_specs: Vec<BoundaryPortSpec> = Vec::new();
        let mut out_port_buffers: Vec<Vec<f32>> = Vec::new();
        let mut out_port_sources: Vec<NodePortRef> = Vec::new();

        for bp in &world.out_ports {
            // Count incoming edges for this out-port.
            let sources: Vec<NodePortRef> = world
                .connections
                .iter()
                .filter_map(|conn| {
                    if let Endpoint::Boundary(dst_id) = parse_endpoint(&conn.to) {
                        if dst_id != bp.id {
                            return None;
                        }
                        match parse_endpoint(&conn.from) {
                            Endpoint::Node(src_id, src_port) => {
                                let src_idx = node_index.get(src_id).copied()?;
                                let (_, src_outputs) = &node_ports[src_idx];
                                let src_port_idx =
                                    src_outputs.iter().position(|p| p.name == src_port)?;
                                Some((src_idx, src_port_idx))
                            }
                            Endpoint::Boundary(_) => None,
                        }
                    } else {
                        None
                    }
                })
                .collect();

            if sources.len() != 1 {
                return Err(EngineError::BoundaryPortInvalidEdgeCount(
                    bp.id.clone(),
                    sources.len(),
                    1,
                ));
            }

            let (src_idx, src_port_idx) = sources[0];
            let (_, src_outputs) = &node_ports[src_idx];
            let ty = src_outputs[src_port_idx].ty.clone();

            // Mark source output as having a downstream consumer so the engine
            // copies it during the fan-out phase.
            output_has_downstream[src_idx][src_port_idx] = true;

            out_port_specs.push(BoundaryPortSpec {
                id: bp.id.clone(),
                name: bp.name.clone(),
                description: bp.description.clone(),
                ty,
            });
            out_port_buffers.push(vec![0.0f32; block_size]);
            out_port_sources.push((src_idx, src_port_idx));
        }

        let mut engine = Engine {
            node_index,
            topo_order_idx,
            topo_order_ids: topo_order,
            nodes,
            connections,
            block_size,
            sample_rate,
            output_buffers,
            input_buffers,
            output_has_downstream,
            output_port_names,
            input_ptr_scratch,
            output_ptr_scratch,
            in_port_specs,
            out_port_specs,
            in_port_buffers,
            out_port_buffers,
            in_port_destinations,
            out_port_sources,
            last_n_frames: 0,
        };

        // Call prepare on every node.
        for i in 0..engine.nodes.len() {
            let id = engine.nodes[i].0.clone();
            engine.nodes[i]
                .1
                .prepare(&id, engine.sample_rate, engine.block_size);
        }

        Ok(engine)
    }

    // -------------------------------------------------------------------------
    // Build-time resolution (NOT realtime-safe; string lookups allowed)
    // -------------------------------------------------------------------------

    /// Resolve a boundary input port id to an `InPortHandle`.
    pub fn resolve_in_port(&self, id: &str) -> Result<InPortHandle, EngineError> {
        self.in_port_specs
            .iter()
            .position(|s| s.id == id)
            .map(|i| InPortHandle(i as u32))
            .ok_or_else(|| EngineError::BoundaryPortNotFound(id.to_string()))
    }

    /// Resolve a boundary output port id to an `OutPortHandle`.
    pub fn resolve_out_port(&self, id: &str) -> Result<OutPortHandle, EngineError> {
        self.out_port_specs
            .iter()
            .position(|s| s.id == id)
            .map(|i| OutPortHandle(i as u32))
            .ok_or_else(|| EngineError::BoundaryPortNotFound(id.to_string()))
    }

    /// All boundary input port specs.
    pub fn in_port_specs(&self) -> &[BoundaryPortSpec] {
        &self.in_port_specs
    }

    /// All boundary output port specs.
    pub fn out_port_specs(&self) -> &[BoundaryPortSpec] {
        &self.out_port_specs
    }

    // -------------------------------------------------------------------------
    // Hot-path / realtime-safe / handle-keyed
    // -------------------------------------------------------------------------

    /// Mutable slice into the boundary input buffer for `h`. Length == `block_size`.
    ///
    /// Realtime-safe: no allocation, no lock, no string lookup.
    pub fn in_port(&mut self, h: InPortHandle) -> &mut [f32] {
        let idx = h.0 as usize;
        debug_assert!(
            idx < self.in_port_buffers.len(),
            "InPortHandle out of range"
        );
        &mut self.in_port_buffers[idx]
    }

    /// Immutable slice of the boundary output buffer for `h`.
    ///
    /// Length equals the `n_frames` passed to the most recent `process_block` call
    /// (0 before any call). Samples beyond that length are stale from a prior block
    /// and are not visible here, so a naive `to_vec()` on a partial block is safe.
    ///
    /// Realtime-safe: no allocation, no lock, no string lookup.
    pub fn out_port(&self, h: OutPortHandle) -> &[f32] {
        let idx = h.0 as usize;
        debug_assert!(
            idx < self.out_port_buffers.len(),
            "OutPortHandle out of range"
        );
        &self.out_port_buffers[idx][..self.last_n_frames]
    }

    /// Process one block of up to `n_frames` samples.
    ///
    /// Hot path discipline: this function must not allocate. Per-node input/output
    /// slice arrays are materialised into pre-sized `(ptr, len)` scratches owned by
    /// the Engine, then reborrowed as `&[&[f32]]` / `&mut [&mut [f32]]` via the
    /// stable layout of fat slice pointers. See `tests/no_alloc.rs` for the
    /// regression check.
    pub fn process_block(&mut self, n_frames: usize) {
        debug_assert!(
            n_frames <= self.block_size,
            "n_frames ({}) > block_size ({})",
            n_frames,
            self.block_size
        );

        // Record before doing any work so out_port() / peek() return the correct
        // slice length even if called mid-panic during development.
        self.last_n_frames = n_frames;

        // --- Zero input accumulator buffers ---
        for node_inputs in &mut self.input_buffers {
            for buf in node_inputs {
                buf[..n_frames].fill(0.0);
            }
        }

        // --- Stage boundary inputs: copy each in-port buffer into its destinations ---
        // No allocation: iterate pre-resolved (node_idx, port_idx) pairs.
        for (handle_idx, dests) in self.in_port_destinations.iter().enumerate() {
            let src: &[f32] = &self.in_port_buffers[handle_idx][..n_frames];
            for &(dst_node, dst_port) in dests {
                let dst = &mut self.input_buffers[dst_node][dst_port][..n_frames];
                dst.copy_from_slice(src);
            }
        }

        // --- Run nodes in topo order ---
        for topo_pos in 0..self.topo_order_idx.len() {
            let node_idx = self.topo_order_idx[topo_pos];

            // Populate (ptr, len) scratches — no allocation.
            for (port_idx, buf) in self.input_buffers[node_idx].iter().enumerate() {
                self.input_ptr_scratch[node_idx][port_idx] = (buf.as_ptr(), n_frames);
            }
            for (port_idx, buf) in self.output_buffers[node_idx].iter_mut().enumerate() {
                self.output_ptr_scratch[node_idx][port_idx] = (buf.as_mut_ptr(), n_frames);
            }

            let in_scratch = &self.input_ptr_scratch[node_idx];
            let out_scratch = &mut self.output_ptr_scratch[node_idx];
            let (_, node) = &mut self.nodes[node_idx];

            // SAFETY: `&[f32]` and `&mut [f32]` have the documented layout
            // `(ptr, len)` (Rust slice ABI). The scratches hold valid `(ptr, len)`
            // pairs into buffers owned by this Engine; those buffers are not
            // realloc'd during process_block and inputs vs outputs are stored in
            // disjoint fields so the views do not alias. The reborrowed slices
            // outlive only `node.process()`, which returns before we touch the
            // buffers again.
            unsafe {
                let in_refs: &[&[f32]] = std::slice::from_raw_parts(
                    in_scratch.as_ptr() as *const &[f32],
                    in_scratch.len(),
                );
                let out_refs: &mut [&mut [f32]] = std::slice::from_raw_parts_mut(
                    out_scratch.as_mut_ptr() as *mut &mut [f32],
                    out_scratch.len(),
                );
                node.process(in_refs, out_refs, n_frames);
            }

            // Fan-out: copy node outputs into downstream input accumulators.
            for port_idx in 0..self.output_has_downstream[node_idx].len() {
                if self.output_has_downstream[node_idx][port_idx] {
                    let src: &[f32] = &self.output_buffers[node_idx][port_idx][..n_frames];
                    for &(s_node, s_port, d_node, d_port) in &self.connections {
                        if s_node == node_idx && s_port == port_idx {
                            let dst = &mut self.input_buffers[d_node][d_port][..n_frames];
                            for (d, s) in dst.iter_mut().zip(src.iter()) {
                                *d += *s;
                            }
                        }
                    }
                }
            }
        }

        // --- Capture boundary outputs ---
        for (handle_idx, &(src_node, src_port)) in self.out_port_sources.iter().enumerate() {
            let src: &[f32] = &self.output_buffers[src_node][src_port][..n_frames];
            let dst: &mut [f32] = &mut self.out_port_buffers[handle_idx][..n_frames];
            dst.copy_from_slice(src);
        }
    }

    /// Run for `n_blocks` full blocks.
    ///
    /// Wrapper around `process_block(self.block_size)` for backwards compatibility.
    pub fn run_blocks(&mut self, n_blocks: u64) {
        let block_size = self.block_size;
        for _ in 0..n_blocks {
            self.process_block(block_size);
        }
    }

    /// Reset all node state and zero boundary port buffers.
    ///
    /// Call after an audio interruption (route change, phone call, OS pause) to prevent
    /// stale internal state from corrupting the next run. This is not realtime-safe —
    /// it is called off the audio thread before restarting.
    pub fn reset(&mut self) {
        for (_, node) in &mut self.nodes {
            node.reset();
        }
        for buf in &mut self.in_port_buffers {
            buf.fill(0.0);
        }
        for buf in &mut self.out_port_buffers {
            buf.fill(0.0);
        }
        // After reset there are no "last produced frames" to read.
        self.last_n_frames = 0;
    }

    // -------------------------------------------------------------------------
    // Finish / debug
    // -------------------------------------------------------------------------

    /// Call `finish()` on each node in topological order and return (node_id, result) pairs.
    pub fn finish(&mut self) -> Vec<(String, Result<(), NodeError>)> {
        self.topo_order_idx
            .iter()
            .map(|&idx| {
                let id = self.nodes[idx].0.clone();
                let result = self.nodes[idx].1.finish();
                (id, result)
            })
            .collect()
    }

    /// Peek at the last block written to an output port on a named node.
    ///
    /// This is a debug / test affordance for inspecting internal ports without
    /// splicing a Tracer node into the graph. NOT the production read API —
    /// do not build host code on this.
    ///
    /// Lifetime contract: `output_buffers` is sized once during `build` and
    /// never reallocated thereafter — `process_block` writes in place. The
    /// returned slice is therefore valid until the next `process_block`
    /// (which overwrites the contents) or engine drop. The engine-ffi
    /// `engine_read_port` function relies on this invariant.
    pub fn peek(&self, node_id: &str, port: &str) -> Result<&[f32], EngineError> {
        let node_idx = self
            .node_index
            .get(node_id)
            .copied()
            .ok_or_else(|| EngineError::NodeNotFound(node_id.to_string()))?;

        let port_idx = self.output_port_names[node_idx]
            .iter()
            .position(|n| n == port)
            .ok_or_else(|| EngineError::PortNotFound(port.to_string(), node_id.to_string()))?;

        Ok(&self.output_buffers[node_idx][port_idx][..self.last_n_frames])
    }

    /// Backwards-compatible alias for `peek`.
    pub fn last_block(&self, node_id: &str, port_name: &str) -> Result<&[f32], EngineError> {
        self.peek(node_id, port_name)
    }

    /// Copy the last block written to `(node_id, port)` into `dst`. Returns
    /// the number of frames written (`min(dst.len(), last_n_frames)`).
    ///
    /// This is the prescriptive-border read API: the caller owns the
    /// destination buffer, the engine copies into it, and the engine's
    /// internal storage never escapes through the FFI. Once this call
    /// returns, the caller's data is independent of subsequent
    /// `process_block` / `reset` / drop activity on the engine.
    ///
    /// If `dst` is shorter than the available frames, the leading prefix
    /// is copied and any remaining samples are silently dropped — the
    /// caller is expected to size `dst` to at least `block_size`.
    pub fn read_into(
        &self,
        node_id: &str,
        port: &str,
        dst: &mut [f32],
    ) -> Result<usize, EngineError> {
        let src = self.peek(node_id, port)?;
        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        Ok(n)
    }

    // -------------------------------------------------------------------------
    // Accessors
    // -------------------------------------------------------------------------

    pub fn node_index(&self) -> &HashMap<String, usize> {
        &self.node_index
    }

    pub fn topo_order(&self) -> &[String] {
        &self.topo_order_ids
    }

    /// All node ids in the engine, in topological (process) order.
    ///
    /// Cheap borrowed view — no allocation. Intended for host code that needs
    /// to enumerate every node (debug panels, port inspectors) without going
    /// back to the source `World`.
    pub fn node_ids(&self) -> &[String] {
        &self.topo_order_ids
    }

    /// Output port names for `node_id`, in declaration order.
    ///
    /// Returns borrowed `&str`s — one small `Vec` allocation per call. Combined
    /// with `node_ids`, this is the runtime port-enumeration surface a host
    /// uses to drive pickers in a debug UI or wire up `peek` calls without
    /// knowing the world JSON at compile time.
    ///
    /// Inputs are deliberately not exposed by a parallel `in_port_names`:
    /// post-mux input buffers carry the same samples as their upstream
    /// output port, so inspecting inputs adds no information over inspecting
    /// the upstream output, and "the upstream output" is already addressable
    /// by `(source_node_id, source_port_name)` via this method.
    pub fn out_port_names(&self, node_id: &str) -> Result<Vec<&str>, EngineError> {
        let node_idx = self
            .node_index
            .get(node_id)
            .copied()
            .ok_or_else(|| EngineError::NodeNotFound(node_id.to_string()))?;
        Ok(self.output_port_names[node_idx]
            .iter()
            .map(String::as_str)
            .collect())
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }
}

/// Kahn's algorithm topological sort. Returns node ids in processing order.
fn topo_sort(
    node_ids: &[String],
    connections: &[Connection],
    node_index: &HashMap<String, usize>,
) -> Result<Vec<String>, EngineError> {
    let n = node_ids.len();
    let mut in_degree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];

    for conn in connections {
        let (src_id, _) = parse_port_path(&conn.from)?;
        let (dst_id, _) = parse_port_path(&conn.to)?;

        let src_idx = *node_index
            .get(src_id)
            .ok_or_else(|| EngineError::NodeNotFound(src_id.to_string()))?;
        let dst_idx = *node_index
            .get(dst_id)
            .ok_or_else(|| EngineError::NodeNotFound(dst_id.to_string()))?;

        adj[src_idx].push(dst_idx);
        in_degree[dst_idx] += 1;
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d == 0)
        .map(|(i, _)| i)
        .collect();

    // Sort for determinism.
    queue.make_contiguous().sort();

    let mut order = Vec::with_capacity(n);
    let mut visited = HashSet::new();

    while let Some(idx) = queue.pop_front() {
        if visited.contains(&idx) {
            continue;
        }
        visited.insert(idx);
        order.push(node_ids[idx].clone());

        let mut next: Vec<usize> = adj[idx].clone();
        next.sort();
        for next_idx in next {
            in_degree[next_idx] -= 1;
            if in_degree[next_idx] == 0 {
                queue.push_back(next_idx);
            }
        }
    }

    if order.len() != n {
        return Err(EngineError::CycleDetected);
    }

    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::Connection;

    fn make_conn(from: &str, to: &str) -> Connection {
        Connection {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    #[test]
    fn topo_sort_dag() {
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let conns = vec![make_conn("a.out", "b.in"), make_conn("b.out", "c.in")];
        let index: HashMap<String, usize> = ids
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();

        let order = topo_sort(&ids, &conns, &index).unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_sort_rejects_cycle() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let conns = vec![make_conn("a.out", "b.in"), make_conn("b.out", "a.in")];
        let index: HashMap<String, usize> = ids
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();

        let result = topo_sort(&ids, &conns, &index);
        assert!(matches!(result, Err(EngineError::CycleDetected)));
    }

    #[test]
    fn topo_sort_single_node() {
        let ids = vec!["solo".to_string()];
        let index: HashMap<String, usize> = [("solo".to_string(), 0)].into();
        let order = topo_sort(&ids, &[], &index).unwrap();
        assert_eq!(order, vec!["solo"]);
    }

    #[test]
    fn peek_returns_output_after_run() {
        use crate::node::Node;
        use crate::world::{NodeDef, World};

        let mut registry = NodeRegistry::new();

        struct ConstantSource;
        impl Node for ConstantSource {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, _: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if let Some(out) = outputs.first_mut() {
                    out[..nframes].fill(0.25);
                }
            }
        }

        use crate::node::{PortSpec, PortType};
        registry.register_full(
            "ConstantSource",
            vec![],
            vec![PortSpec {
                name: "audio_out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(ConstantSource) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![],
            out_ports: vec![],
            nodes: vec![NodeDef {
                id: "src".to_string(),
                ty: "ConstantSource".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![],
        };

        let block_size = 256;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        engine.run_blocks(1);

        let buf = engine.peek("src", "audio_out").unwrap();
        assert_eq!(buf.len(), block_size);
        assert!(
            buf.iter().all(|&s| (s - 0.25).abs() < 1e-9),
            "expected all samples to be 0.25"
        );
    }

    #[test]
    fn last_block_returns_output_after_run() {
        use crate::node::Node;
        use crate::world::{NodeDef, World};

        let mut registry = NodeRegistry::new();

        struct ConstantSource;
        impl Node for ConstantSource {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, _: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if let Some(out) = outputs.first_mut() {
                    out[..nframes].fill(0.25);
                }
            }
        }

        use crate::node::{PortSpec, PortType};
        registry.register_full(
            "ConstantSource",
            vec![],
            vec![PortSpec {
                name: "audio_out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(ConstantSource) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![],
            out_ports: vec![],
            nodes: vec![NodeDef {
                id: "src".to_string(),
                ty: "ConstantSource".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![],
        };

        let block_size = 256;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        engine.run_blocks(1);

        let buf = engine.last_block("src", "audio_out").unwrap();
        assert_eq!(buf.len(), block_size);
        assert!(
            buf.iter().all(|&s| (s - 0.25).abs() < 1e-9),
            "expected all samples to be 0.25"
        );
    }

    #[test]
    fn last_block_errors_on_missing_node() {
        use crate::node::Node;
        use crate::world::{NodeDef, World};

        let mut registry = NodeRegistry::new();
        struct Dummy;
        impl Node for Dummy {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, _: &[&[f32]], _: &mut [&mut [f32]], _: usize) {}
        }
        registry.register_full(
            "Dummy",
            vec![],
            vec![],
            vec![],
            Box::new(|_| Box::new(Dummy) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![],
            out_ports: vec![],
            nodes: vec![NodeDef {
                id: "n".to_string(),
                ty: "Dummy".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![],
        };

        let engine = Engine::build(&world, &registry, 48000, 512).unwrap();
        assert!(matches!(
            engine.last_block("missing", "x"),
            Err(EngineError::NodeNotFound(_))
        ));
        assert!(matches!(
            engine.last_block("n", "no_such_port"),
            Err(EngineError::PortNotFound(_, _))
        ));
    }

    #[test]
    fn empty_node_id_rejected() {
        use crate::node::Node;
        use crate::world::{NodeDef, World};
        let mut registry = NodeRegistry::new();
        struct Dummy;
        impl Node for Dummy {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, _: &[&[f32]], _: &mut [&mut [f32]], _: usize) {}
        }
        registry.register_full(
            "Dummy",
            vec![],
            vec![],
            vec![],
            Box::new(|_| Box::new(Dummy) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![],
            out_ports: vec![],
            nodes: vec![NodeDef {
                id: String::new(),
                ty: "Dummy".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![],
        };

        let result = Engine::build(&world, &registry, 48000, 512);
        assert!(
            matches!(result, Err(EngineError::InvalidNodeId(ref id, _)) if id.is_empty()),
            "expected InvalidNodeId error for empty id"
        );
    }

    #[test]
    fn boundary_ports_round_trip_data() {
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        // A node that copies input to output.
        struct Passthrough;
        impl Node for Passthrough {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if !inputs.is_empty() && !outputs.is_empty() {
                    outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
                }
            }
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Passthrough",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Passthrough) as Box<dyn Node>),
        );

        // World: in_port "x" → passthrough → out_port "y"
        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![BoundaryPort {
                id: "x".to_string(),
                name: None,
                description: None,
            }],
            out_ports: vec![BoundaryPort {
                id: "y".to_string(),
                name: None,
                description: None,
            }],
            nodes: vec![NodeDef {
                id: "pt".to_string(),
                ty: "Passthrough".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![
                Connection {
                    from: "x".to_string(),
                    to: "pt.in".to_string(),
                },
                Connection {
                    from: "pt.out".to_string(),
                    to: "y".to_string(),
                },
            ],
        };

        let block_size = 8;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();

        let h_in = engine.resolve_in_port("x").unwrap();
        let h_out = engine.resolve_out_port("y").unwrap();

        // Write known signal into the in-port.
        let signal: Vec<f32> = (0..block_size).map(|i| i as f32).collect();
        engine.in_port(h_in).copy_from_slice(&signal);
        engine.process_block(block_size);

        // Out-port should carry the same signal.
        let result = engine.out_port(h_out).to_vec();
        assert_eq!(
            result, signal,
            "boundary passthrough should preserve samples"
        );
    }

    #[test]
    fn boundary_in_port_fans_out() {
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        // A node that copies input to output.
        struct Passthrough;
        impl Node for Passthrough {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if !inputs.is_empty() && !outputs.is_empty() {
                    outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
                }
            }
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Passthrough",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Passthrough) as Box<dyn Node>),
        );

        // World: in_port "x" fans out to pt1 and pt2.
        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![BoundaryPort {
                id: "x".to_string(),
                name: None,
                description: None,
            }],
            out_ports: vec![
                BoundaryPort {
                    id: "y1".to_string(),
                    name: None,
                    description: None,
                },
                BoundaryPort {
                    id: "y2".to_string(),
                    name: None,
                    description: None,
                },
            ],
            nodes: vec![
                NodeDef {
                    id: "pt1".to_string(),
                    ty: "Passthrough".to_string(),
                    params: Default::default(),
                    name: None,
                    description: None,
                },
                NodeDef {
                    id: "pt2".to_string(),
                    ty: "Passthrough".to_string(),
                    params: Default::default(),
                    name: None,
                    description: None,
                },
            ],
            connections: vec![
                Connection {
                    from: "x".to_string(),
                    to: "pt1.in".to_string(),
                },
                Connection {
                    from: "x".to_string(),
                    to: "pt2.in".to_string(),
                },
                Connection {
                    from: "pt1.out".to_string(),
                    to: "y1".to_string(),
                },
                Connection {
                    from: "pt2.out".to_string(),
                    to: "y2".to_string(),
                },
            ],
        };

        let block_size = 4;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        let h_in = engine.resolve_in_port("x").unwrap();
        let h_out1 = engine.resolve_out_port("y1").unwrap();
        let h_out2 = engine.resolve_out_port("y2").unwrap();

        let signal = vec![1.0f32, 2.0, 3.0, 4.0];
        engine.in_port(h_in).copy_from_slice(&signal);
        engine.process_block(block_size);

        assert_eq!(engine.out_port(h_out1).to_vec(), signal);
        assert_eq!(engine.out_port(h_out2).to_vec(), signal);
    }

    #[test]
    fn partial_block_out_port_len() {
        // Regression: out_port() must return exactly n_frames samples after a
        // partial process_block call, not the full block_size buffer.
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        struct Passthrough;
        impl Node for Passthrough {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if !inputs.is_empty() && !outputs.is_empty() {
                    outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
                }
            }
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Passthrough",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Passthrough) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![BoundaryPort {
                id: "x".to_string(),
                name: None,
                description: None,
            }],
            out_ports: vec![BoundaryPort {
                id: "y".to_string(),
                name: None,
                description: None,
            }],
            nodes: vec![NodeDef {
                id: "pt".to_string(),
                ty: "Passthrough".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![
                Connection {
                    from: "x".to_string(),
                    to: "pt.in".to_string(),
                },
                Connection {
                    from: "pt.out".to_string(),
                    to: "y".to_string(),
                },
            ],
        };

        let block_size = 512;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        let h_out = engine.resolve_out_port("y").unwrap();

        engine.process_block(100);
        assert_eq!(
            engine.out_port(h_out).len(),
            100,
            "out_port must return exactly n_frames samples after a partial block"
        );
    }

    #[test]
    fn zero_frame_process_block_is_no_op() {
        // Regression: process_block(0) must not panic and out_port must return
        // an empty slice (Task 6 smoke test).
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        struct Passthrough;
        impl Node for Passthrough {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if !inputs.is_empty() && !outputs.is_empty() {
                    outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
                }
            }
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Passthrough",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Passthrough) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![BoundaryPort {
                id: "x".to_string(),
                name: None,
                description: None,
            }],
            out_ports: vec![BoundaryPort {
                id: "y".to_string(),
                name: None,
                description: None,
            }],
            nodes: vec![NodeDef {
                id: "pt".to_string(),
                ty: "Passthrough".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![
                Connection {
                    from: "x".to_string(),
                    to: "pt.in".to_string(),
                },
                Connection {
                    from: "pt.out".to_string(),
                    to: "y".to_string(),
                },
            ],
        };

        let block_size = 512;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        let h_out = engine.resolve_out_port("y").unwrap();

        // Must not panic.
        engine.process_block(0);
        assert_eq!(
            engine.out_port(h_out).len(),
            0,
            "out_port must return empty slice after process_block(0)"
        );
    }

    #[test]
    fn boundary_in_port_destination_conflict_rejected() {
        // Two in-ports wired to the same destination node port must be rejected.
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        struct Passthrough;
        impl Node for Passthrough {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if !inputs.is_empty() && !outputs.is_empty() {
                    outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
                }
            }
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Passthrough",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Passthrough) as Box<dyn Node>),
        );

        // Two in-ports "a" and "b" both wired to "pt.in".
        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![
                BoundaryPort {
                    id: "a".to_string(),
                    name: None,
                    description: None,
                },
                BoundaryPort {
                    id: "b".to_string(),
                    name: None,
                    description: None,
                },
            ],
            out_ports: vec![BoundaryPort {
                id: "y".to_string(),
                name: None,
                description: None,
            }],
            nodes: vec![NodeDef {
                id: "pt".to_string(),
                ty: "Passthrough".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![
                Connection {
                    from: "a".to_string(),
                    to: "pt.in".to_string(),
                },
                Connection {
                    from: "b".to_string(),
                    to: "pt.in".to_string(),
                },
                Connection {
                    from: "pt.out".to_string(),
                    to: "y".to_string(),
                },
            ],
        };

        let result = Engine::build(&world, &registry, 48000, 512);
        assert!(
            matches!(
                result,
                Err(EngineError::BoundaryInPortDestinationConflict { .. })
            ),
            "expected BoundaryInPortDestinationConflict"
        );
    }

    #[test]
    fn boundary_to_boundary_edge_rejected() {
        // An edge from one boundary port to another must be rejected immediately.
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        struct Dummy;
        impl Node for Dummy {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, _: &[&[f32]], _: &mut [&mut [f32]], _: usize) {}
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Dummy",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Dummy) as Box<dyn Node>),
        );

        // Edge from in-port "x_in" to out-port "y_out": boundary→boundary.
        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![BoundaryPort {
                id: "x_in".to_string(),
                name: None,
                description: None,
            }],
            out_ports: vec![BoundaryPort {
                id: "y_out".to_string(),
                name: None,
                description: None,
            }],
            nodes: vec![NodeDef {
                id: "d".to_string(),
                ty: "Dummy".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![Connection {
                from: "x_in".to_string(),
                to: "y_out".to_string(),
            }],
        };

        let result = Engine::build(&world, &registry, 48000, 512);
        assert!(
            matches!(result, Err(EngineError::BoundaryToBoundary(_, _))),
            "expected BoundaryToBoundary error"
        );
    }

    #[test]
    fn reset_zeros_boundary_buffers() {
        use crate::node::{Node, PortSpec, PortType};
        use crate::world::{BoundaryPort, NodeDef, World};

        struct Passthrough;
        impl Node for Passthrough {
            fn prepare(&mut self, _: &str, _: u32, _: usize) {}
            fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], nframes: usize) {
                if !inputs.is_empty() && !outputs.is_empty() {
                    outputs[0][..nframes].copy_from_slice(&inputs[0][..nframes]);
                }
            }
        }

        let mut registry = NodeRegistry::new();
        registry.register_full(
            "Passthrough",
            vec![PortSpec {
                name: "in",
                ty: PortType::Audio,
            }],
            vec![PortSpec {
                name: "out",
                ty: PortType::Audio,
            }],
            vec![],
            Box::new(|_| Box::new(Passthrough) as Box<dyn Node>),
        );

        let world = World {
            schema: None,
            world_version: 1,
            in_ports: vec![BoundaryPort {
                id: "x".to_string(),
                name: None,
                description: None,
            }],
            out_ports: vec![BoundaryPort {
                id: "y".to_string(),
                name: None,
                description: None,
            }],
            nodes: vec![NodeDef {
                id: "pt".to_string(),
                ty: "Passthrough".to_string(),
                params: Default::default(),
                name: None,
                description: None,
            }],
            connections: vec![
                Connection {
                    from: "x".to_string(),
                    to: "pt.in".to_string(),
                },
                Connection {
                    from: "pt.out".to_string(),
                    to: "y".to_string(),
                },
            ],
        };

        let block_size = 4;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        let h_in = engine.resolve_in_port("x").unwrap();
        let h_out = engine.resolve_out_port("y").unwrap();

        engine.in_port(h_in).copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
        engine.process_block(block_size);
        assert_ne!(engine.out_port(h_out)[0], 0.0);

        engine.reset();
        assert!(engine.in_port(h_in).iter().all(|&v| v == 0.0));
        assert!(engine.out_port(h_out).iter().all(|&v| v == 0.0));
    }
}
