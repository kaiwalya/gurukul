use crate::node::{Node, ParamSpec, PortSpec};
use std::collections::HashMap;

/// A factory closure that creates a fresh Node instance with the given parameters.
pub type Factory = Box<dyn Fn(&HashMap<String, f64>) -> Box<dyn Node> + Send + Sync>;

/// A closure that returns port declarations given parameters. For most nodes the ports
/// are static (params ignored); variadic nodes like MixSum compute ports from params.
pub type PortFactory =
    Box<dyn Fn(&HashMap<String, f64>) -> (Vec<PortSpec>, Vec<PortSpec>) + Send + Sync>;

struct NodeEntry {
    /// Default ports (computed from default params) — used by CLI introspection.
    default_inputs: Vec<PortSpec>,
    default_outputs: Vec<PortSpec>,
    params: Vec<ParamSpec>,
    port_factory: PortFactory,
    factory: Factory,
}

pub struct NodeRegistry {
    entries: HashMap<String, NodeEntry>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a node type whose ports are static (don't depend on params).
    pub fn register_full(
        &mut self,
        name: impl Into<String>,
        inputs: Vec<PortSpec>,
        outputs: Vec<PortSpec>,
        params: Vec<ParamSpec>,
        factory: Factory,
    ) {
        // Port factory clones the static declarations.
        let static_inputs = inputs.clone();
        let static_outputs = outputs.clone();
        let port_factory: PortFactory =
            Box::new(move |_params| (static_inputs.clone(), static_outputs.clone()));
        self.entries.insert(
            name.into(),
            NodeEntry {
                default_inputs: inputs,
                default_outputs: outputs,
                params,
                port_factory,
                factory,
            },
        );
    }

    /// Register a node type whose ports depend on the given params (variadic nodes).
    pub fn register_full_variadic(
        &mut self,
        name: impl Into<String>,
        default_inputs: Vec<PortSpec>,
        default_outputs: Vec<PortSpec>,
        params: Vec<ParamSpec>,
        port_factory: PortFactory,
        factory: Factory,
    ) {
        self.entries.insert(
            name.into(),
            NodeEntry {
                default_inputs,
                default_outputs,
                params,
                port_factory,
                factory,
            },
        );
    }

    pub fn create(&self, ty: &str, params: &HashMap<String, f64>) -> Option<Box<dyn Node>> {
        self.entries.get(ty).map(|e| (e.factory)(params))
    }

    /// Returns `(inputs, outputs)` port declarations for the given node type and params.
    /// Used by Engine::build to get per-instance port layout.
    pub fn ports_for_params(
        &self,
        ty: &str,
        params: &HashMap<String, f64>,
    ) -> Option<(Vec<PortSpec>, Vec<PortSpec>)> {
        self.entries.get(ty).map(|e| (e.port_factory)(params))
    }

    /// Returns the default `(inputs, outputs)` port declarations for CLI introspection.
    /// For variadic nodes this reflects the default param values.
    pub fn ports(&self, ty: &str) -> Option<(&[PortSpec], &[PortSpec])> {
        self.entries
            .get(ty)
            .map(|e| (e.default_inputs.as_slice(), e.default_outputs.as_slice()))
    }

    /// Returns parameter declarations for the given node type.
    pub fn parameters(&self, ty: &str) -> Option<&[ParamSpec]> {
        self.entries.get(ty).map(|e| e.params.as_slice())
    }

    pub fn node_types(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.entries.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn contains(&self, ty: &str) -> bool {
        self.entries.contains_key(ty)
    }
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new()
    }
}
