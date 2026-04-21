use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct World {
    /// Path to the JSON Schema for this file (used by editors).
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,

    pub sample_rate: u32,
    pub block_size: usize,
    pub nodes: Vec<NodeDef>,
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NodeDef {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub params: HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Connection {
    /// Source port path: `<node_id>.<port_name>`
    pub from: String,
    /// Destination port path: `<node_id>.<port_name>`
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_round_trips() {
        let json = r#"{
            "sample_rate": 48000,
            "block_size": 512,
            "nodes": [
                {"id": "src", "type": "SineSource", "params": {"freq": 440.0, "amplitude": 0.5}},
                {"id": "out", "type": "NullSink"}
            ],
            "connections": [
                {"from": "src.audio_out", "to": "out.audio_in"}
            ]
        }"#;

        let world: World = serde_json::from_str(json).expect("deserialize");
        let back = serde_json::to_string(&world).expect("serialize");
        let world2: World = serde_json::from_str(&back).expect("re-deserialize");

        assert_eq!(world.sample_rate, world2.sample_rate);
        assert_eq!(world.block_size, world2.block_size);
        assert_eq!(world.nodes.len(), world2.nodes.len());
        assert_eq!(world.connections.len(), world2.connections.len());
        assert_eq!(world.nodes[0].id, world2.nodes[0].id);
        assert_eq!(world.nodes[0].ty, world2.nodes[0].ty);
    }
}
