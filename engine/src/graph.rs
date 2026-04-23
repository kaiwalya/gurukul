use crate::node::{Node, NodeError};
use crate::registry::NodeRegistry;
use crate::world::{Connection, World};
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
}

/// Parsed port address.
pub fn parse_port_path(path: &str) -> Result<(&str, &str), EngineError> {
    let (node_id, port_name) = path
        .split_once('.')
        .ok_or_else(|| EngineError::InvalidPortPath(path.to_string()))?;
    Ok((node_id, port_name))
}

/// Validate a user-chosen node id. Rejects empty ids, ids containing "__", and ids
/// starting with "__trace__" (reserved for splice_tracers).
fn validate_node_id(id: &str) -> Result<(), EngineError> {
    if id.is_empty() {
        return Err(EngineError::InvalidNodeId(
            id.to_string(),
            "id must not be empty".to_string(),
        ));
    }
    if id.contains("__") {
        return Err(EngineError::InvalidNodeId(
            id.to_string(),
            "id must not contain double underscores ('__') — reserved for internal tracer ids"
                .to_string(),
        ));
    }
    Ok(())
}

/// A running engine: instantiated nodes in topological order, ready to process.
pub struct Engine {
    node_index: HashMap<String, usize>,
    topo_order_idx: Vec<usize>,
    topo_order_ids: Vec<String>,
    nodes: Vec<(String, Box<dyn Node>)>,
    // (src_node_idx, src_port_idx, dst_node_idx, dst_port_idx)
    connections: Vec<(usize, usize, usize, usize)>,
    block_size: usize,
    sample_rate: u32,
    // Per-node per-output-port buffers, allocated once.
    output_buffers: Vec<Vec<Vec<f32>>>,
    // Per-node per-input-port buffers, allocated once and reused across run_blocks calls.
    input_buffers: Vec<Vec<Vec<f32>>>,
    // Per (node_idx, out_port_idx): does anything downstream consume it?
    output_has_downstream: Vec<Vec<bool>>,
    // Per-node list of output port names, in declaration order. Populated during build().
    // Used by last_block() to resolve a port name to a buffer index.
    output_port_names: Vec<Vec<String>>,
}

impl Engine {
    /// Build and prepare an Engine from a World definition.
    pub fn build(
        world: &World,
        registry: &NodeRegistry,
        sample_rate: u32,
        block_size: usize,
    ) -> Result<Self, EngineError> {
        // Validate node ids before doing anything else.
        for node_def in &world.nodes {
            // Allow __trace__ ids since splice_tracers produces them. User-authored ids
            // are rejected if they contain "__" at all.
            // splice_tracers ids always start with "__trace__"; we only validate ids that
            // don't start with "__trace__".
            if !node_def.id.starts_with("__trace__") {
                validate_node_id(&node_def.id)?;
            }
        }

        // Instantiate nodes
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

        // Topological sort
        let topo_order = topo_sort(
            &world.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>(),
            &world.connections,
            &node_index,
        )?;

        // Pre-compute per-node port declarations using actual params (needed for variadic nodes).
        let node_ports: Vec<(Vec<crate::node::PortSpec>, Vec<crate::node::PortSpec>)> = world
            .nodes
            .iter()
            .map(|nd| registry.ports_for_params(&nd.ty, &nd.params).unwrap())
            .collect();

        // Resolve connections to index pairs — use per-node port declarations.
        let mut connections = Vec::new();
        for conn in &world.connections {
            let (src_id, src_port) = parse_port_path(&conn.from)?;
            let (dst_id, dst_port) = parse_port_path(&conn.to)?;

            let src_idx = *node_index
                .get(src_id)
                .ok_or_else(|| EngineError::NodeNotFound(src_id.to_string()))?;
            let dst_idx = *node_index
                .get(dst_id)
                .ok_or_else(|| EngineError::NodeNotFound(dst_id.to_string()))?;

            let (_, src_outputs) = &node_ports[src_idx];
            let src_port_idx = src_outputs
                .iter()
                .position(|p| p.name == src_port)
                .ok_or_else(|| {
                    EngineError::PortNotFound(src_port.to_string(), src_id.to_string())
                })?;

            let (dst_inputs, _) = &node_ports[dst_idx];
            let dst_port_idx = dst_inputs
                .iter()
                .position(|p| p.name == dst_port)
                .ok_or_else(|| {
                    EngineError::PortNotFound(dst_port.to_string(), dst_id.to_string())
                })?;

            connections.push((src_idx, src_port_idx, dst_idx, dst_port_idx));
        }

        // Allocate buffers using per-node port declarations.
        let mut output_buffers: Vec<Vec<Vec<f32>>> = Vec::with_capacity(nodes.len());
        let mut input_buffers: Vec<Vec<Vec<f32>>> = Vec::with_capacity(nodes.len());
        for (inputs, outputs) in &node_ports {
            output_buffers.push(outputs.iter().map(|_| vec![0.0f32; block_size]).collect());
            input_buffers.push(inputs.iter().map(|_| vec![0.0f32; block_size]).collect());
        }

        // Per-output-port downstream flag, for skipping copies when no one listens.
        let mut output_has_downstream: Vec<Vec<bool>> = node_ports
            .iter()
            .map(|(_, outputs)| vec![false; outputs.len()])
            .collect();
        for &(s_node, s_port, _, _) in &connections {
            output_has_downstream[s_node][s_port] = true;
        }

        // Persist output port names for last_block() lookups.
        let output_port_names: Vec<Vec<String>> = node_ports
            .iter()
            .map(|(_, outputs)| outputs.iter().map(|p| p.name.to_string()).collect())
            .collect();

        // Resolve topo order ids to indices once.
        let topo_order_idx: Vec<usize> = topo_order.iter().map(|id| node_index[id]).collect();

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
        };

        // Call prepare on every node, passing the node's own id.
        for i in 0..engine.nodes.len() {
            let id = engine.nodes[i].0.clone();
            engine.nodes[i]
                .1
                .prepare(&id, engine.sample_rate, engine.block_size);
        }

        Ok(engine)
    }

    /// Run for `n_blocks` blocks.
    pub fn run_blocks(&mut self, n_blocks: u64) {
        for _ in 0..n_blocks {
            for node_inputs in &mut self.input_buffers {
                for buf in node_inputs {
                    buf.fill(0.0);
                }
            }

            for &node_idx in &self.topo_order_idx {
                let input_slices: Vec<&[f32]> = self.input_buffers[node_idx]
                    .iter()
                    .map(|v| v.as_slice())
                    .collect();

                {
                    let output_bufs = &mut self.output_buffers[node_idx];
                    let mut output_slices: Vec<&mut [f32]> =
                        output_bufs.iter_mut().map(|v| v.as_mut_slice()).collect();
                    let (_, node) = &mut self.nodes[node_idx];
                    node.process(&input_slices, &mut output_slices, self.block_size);
                }

                for port_idx in 0..self.output_has_downstream[node_idx].len() {
                    if self.output_has_downstream[node_idx][port_idx] {
                        // output_buffers and input_buffers are disjoint fields — borrow safely.
                        let src: &[f32] = self.output_buffers[node_idx][port_idx].as_slice();
                        for &(s_node, s_port, d_node, d_port) in &self.connections {
                            if s_node == node_idx && s_port == port_idx {
                                let dst = &mut self.input_buffers[d_node][d_port];
                                for (d, s) in dst.iter_mut().zip(src.iter()) {
                                    *d += *s;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Call `finish()` on each node in topological order and return (node_id, result) pairs.
    ///
    /// Not called automatically by `run_blocks` — the caller invokes this explicitly after the
    /// last block. This keeps `run_blocks` a pure hot-loop and allows worlds that stream forever
    /// without ever finishing.
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

    pub fn node_index(&self) -> &HashMap<String, usize> {
        &self.node_index
    }

    pub fn topo_order(&self) -> &[String] {
        &self.topo_order_ids
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Read the last block written to an output port on a node.
    ///
    /// Valid only after `finish()` or the final `run_blocks()` call completes — this is a
    /// test/debug affordance for inspecting terminal state, NOT the subscription API
    /// (see `ARCHITECTURE.md` § "Port addressing and subscription"). The real read-side will be
    /// stream-based and carry timestamps; do not build production tools on this accessor.
    ///
    /// The returned slice has length up to `block_size()`; future variable-tail-block runs may
    /// return a shorter slice, so callers must check `.len()` rather than indexing unconditionally.
    pub fn last_block(&self, node_id: &str, port_name: &str) -> Result<&[f32], EngineError> {
        let node_idx = self
            .node_index
            .get(node_id)
            .copied()
            .ok_or_else(|| EngineError::NodeNotFound(node_id.to_string()))?;

        let port_idx = self.output_port_names[node_idx]
            .iter()
            .position(|n| n == port_name)
            .ok_or_else(|| EngineError::PortNotFound(port_name.to_string(), node_id.to_string()))?;

        Ok(&self.output_buffers[node_idx][port_idx])
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

    // Sort for determinism
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
    fn last_block_returns_output_after_run() {
        use crate::node::Node;
        use crate::world::{NodeDef, World};

        let mut registry = NodeRegistry::new();

        // A node with one output that fills it with a known constant on every process() call.
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
            nodes: vec![NodeDef {
                id: "src".to_string(),
                ty: "ConstantSource".to_string(),
                params: Default::default(),
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
            nodes: vec![NodeDef {
                id: "n".to_string(),
                ty: "Dummy".to_string(),
                params: Default::default(),
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
    fn invalid_node_id_double_underscore() {
        use crate::node::Node;
        use crate::world::{NodeDef, World};
        let mut registry = NodeRegistry::new();
        // Register a minimal node type for the test.
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
            nodes: vec![NodeDef {
                id: "foo__bar".to_string(),
                ty: "Dummy".to_string(),
                params: Default::default(),
            }],
            connections: vec![],
        };

        let result = Engine::build(&world, &registry, 48000, 512);
        assert!(
            matches!(result, Err(EngineError::InvalidNodeId(ref id, _)) if id == "foo__bar"),
            "expected InvalidNodeId error"
        );
    }
}
