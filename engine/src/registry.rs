use crate::node::Node;
use std::collections::HashMap;

/// A factory closure that creates a fresh Node instance with the given parameters.
pub type Factory =
    Box<dyn Fn(&std::collections::HashMap<String, f64>) -> Box<dyn Node> + Send + Sync>;

pub struct NodeRegistry {
    factories: HashMap<String, Factory>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, factory: Factory) {
        self.factories.insert(name.into(), factory);
    }

    pub fn create(
        &self,
        ty: &str,
        params: &std::collections::HashMap<String, f64>,
    ) -> Option<Box<dyn Node>> {
        self.factories.get(ty).map(|f| f(params))
    }

    pub fn node_types(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.factories.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn contains(&self, ty: &str) -> bool {
        self.factories.contains_key(ty)
    }
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new()
    }
}
