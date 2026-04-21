use crate::node::Node;
use crate::registry::NodeRegistry;
use crate::subscription::SubscriptionHub;
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
}

/// Parsed port address.
pub fn parse_port_path(path: &str) -> Result<(&str, &str), EngineError> {
    let (node_id, port_name) = path
        .split_once('.')
        .ok_or_else(|| EngineError::InvalidPortPath(path.to_string()))?;
    Ok((node_id, port_name))
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
    // Cached port names per node (parallel to output_buffers / input_buffers).
    output_port_names: Vec<Vec<String>>,
    // Per (node_idx, out_port_idx): does anything downstream consume it?
    output_has_downstream: Vec<Vec<bool>>,
}

impl Engine {
    /// Build and prepare an Engine from a World definition.
    pub fn build(world: &World, registry: &NodeRegistry) -> Result<Self, EngineError> {
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

        // Resolve connections to index pairs
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

            let (_, src_node) = &nodes[src_idx];
            let (_, outputs) = src_node.declare_ports();
            let src_port_idx =
                outputs
                    .iter()
                    .position(|p| p.name == src_port)
                    .ok_or_else(|| {
                        EngineError::PortNotFound(src_port.to_string(), src_id.to_string())
                    })?;

            let (_, dst_node) = &nodes[dst_idx];
            let (inputs, _) = dst_node.declare_ports();
            let dst_port_idx = inputs
                .iter()
                .position(|p| p.name == dst_port)
                .ok_or_else(|| {
                    EngineError::PortNotFound(dst_port.to_string(), dst_id.to_string())
                })?;

            connections.push((src_idx, src_port_idx, dst_idx, dst_port_idx));
        }

        // Cache port layouts and allocate buffers up front; declare_ports() is never called per-block.
        let mut output_buffers: Vec<Vec<Vec<f32>>> = Vec::with_capacity(nodes.len());
        let mut input_buffers: Vec<Vec<Vec<f32>>> = Vec::with_capacity(nodes.len());
        let mut output_port_names: Vec<Vec<String>> = Vec::with_capacity(nodes.len());
        for (_, node) in &nodes {
            let (inputs, outputs) = node.declare_ports();
            output_buffers.push(
                outputs
                    .iter()
                    .map(|_| vec![0.0f32; world.block_size])
                    .collect(),
            );
            input_buffers.push(
                inputs
                    .iter()
                    .map(|_| vec![0.0f32; world.block_size])
                    .collect(),
            );
            output_port_names.push(outputs.iter().map(|p| p.name.to_string()).collect());
        }

        // Per-output-port downstream flag, for skipping clones when no one listens.
        let mut output_has_downstream: Vec<Vec<bool>> = output_port_names
            .iter()
            .map(|ports| vec![false; ports.len()])
            .collect();
        for &(s_node, s_port, _, _) in &connections {
            output_has_downstream[s_node][s_port] = true;
        }

        // Resolve topo order ids to indices once.
        let topo_order_idx: Vec<usize> = topo_order.iter().map(|id| node_index[id]).collect();

        let mut engine = Engine {
            node_index,
            topo_order_idx,
            topo_order_ids: topo_order,
            nodes,
            connections,
            block_size: world.block_size,
            sample_rate: world.sample_rate,
            output_buffers,
            input_buffers,
            output_port_names,
            output_has_downstream,
        };

        for (_, node) in &mut engine.nodes {
            node.prepare(engine.sample_rate, engine.block_size);
        }

        Ok(engine)
    }

    /// Run for `n_blocks` blocks, calling the subscription hub for any subscribed ports.
    pub fn run_blocks(&mut self, n_blocks: u64, hub: &SubscriptionHub) {
        for block_num in 0..n_blocks {
            let timestamp = block_num * self.block_size as u64;

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

                let node_id = &self.nodes[node_idx].0;
                for port_idx in 0..self.output_port_names[node_idx].len() {
                    let port_name = &self.output_port_names[node_idx][port_idx];
                    let has_downstream = self.output_has_downstream[node_idx][port_idx];

                    // Only format the port path (and possibly clone the block) when needed.
                    let port_path = format!("{}.{}", node_id, port_name);
                    if hub.has_subscribers(&port_path) {
                        hub.send(
                            &port_path,
                            (timestamp, self.output_buffers[node_idx][port_idx].clone()),
                        );
                    }

                    if has_downstream {
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
    let mut sorted_queue: Vec<usize> = queue.drain(..).collect();
    sorted_queue.sort();
    queue.extend(sorted_queue);

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
}
