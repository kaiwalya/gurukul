use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct World {
    /// Path to the JSON Schema for this file (used by editors).
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,

    /// Schema version. Always 1 for worlds targeting this engine.
    #[serde(default = "default_world_version")]
    pub world_version: u32,

    /// Boundary input ports: the host writes samples into these before each block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_ports: Vec<BoundaryPort>,

    /// Boundary output ports: the host reads samples from these after each block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_ports: Vec<BoundaryPort>,

    pub nodes: Vec<NodeDef>,
    pub connections: Vec<Connection>,
}

fn default_world_version() -> u32 {
    1
}

/// A boundary port on the engine's faceplate.
///
/// The port's signal type is not declared here — it is derived at engine-build
/// time from the connected node port(s). `id` must match `^[a-z][a-z0-9_]*$`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BoundaryPort {
    /// Stable identifier. Used by edges and host APIs. Must match `^[a-z][a-z0-9_]*$`.
    pub id: String,
    /// Human-readable label (optional, cosmetic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form tooltip / API doc (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Regex for boundary port ids: starts with lowercase letter, followed by lowercase letters,
/// digits, or underscores.
pub fn boundary_port_id_valid(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NodeDef {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub params: HashMap<String, f64>,
    /// Human-readable label (optional, cosmetic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form tooltip / API doc (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Connection {
    /// Source: either `<node_id>.<port_name>` (dotted) or a bare boundary port id.
    pub from: String,
    /// Destination: either `<node_id>.<port_name>` (dotted) or a bare boundary port id.
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_round_trips() {
        let json = r#"{
            "world_version": 1,
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

        assert_eq!(world.world_version, 1);
        assert_eq!(world.nodes.len(), world2.nodes.len());
        assert_eq!(world.connections.len(), world2.connections.len());
        assert_eq!(world.nodes[0].id, world2.nodes[0].id);
        assert_eq!(world.nodes[0].ty, world2.nodes[0].ty);
    }

    #[test]
    fn world_round_trips_with_boundary_ports() {
        let json = r#"{
            "world_version": 1,
            "in_ports": [{"id": "mic", "name": "Microphone", "description": "Audio input"}],
            "out_ports": [{"id": "pitch_hz"}],
            "nodes": [
                {"id": "yin", "type": "PitchYin", "name": "Pitch Detector"}
            ],
            "connections": [
                {"from": "mic", "to": "yin.audio_in"},
                {"from": "yin.f0_hz", "to": "pitch_hz"}
            ]
        }"#;

        let world: World = serde_json::from_str(json).expect("deserialize");
        assert_eq!(world.world_version, 1);
        assert_eq!(world.in_ports.len(), 1);
        assert_eq!(world.in_ports[0].id, "mic");
        assert_eq!(world.in_ports[0].name.as_deref(), Some("Microphone"));
        assert_eq!(world.out_ports.len(), 1);
        assert_eq!(world.out_ports[0].id, "pitch_hz");

        let back = serde_json::to_string(&world).expect("serialize");
        let world2: World = serde_json::from_str(&back).expect("re-deserialize");
        assert_eq!(world2.in_ports.len(), 1);
        assert_eq!(world2.out_ports.len(), 1);
    }

    #[test]
    fn world_version_defaults_to_1() {
        let json = r#"{"nodes": [], "connections": []}"#;
        let world: World = serde_json::from_str(json).expect("deserialize");
        assert_eq!(world.world_version, 1);
    }

    #[test]
    fn boundary_port_id_validation() {
        // Valid ids.
        assert!(boundary_port_id_valid("mic"));
        assert!(boundary_port_id_valid("mic_in"));
        assert!(boundary_port_id_valid("a"));
        assert!(boundary_port_id_valid("f0_hz"));
        assert!(boundary_port_id_valid("x2"));

        // Invalid ids.
        assert!(!boundary_port_id_valid("")); // empty
        assert!(!boundary_port_id_valid("1start")); // starts with digit
        assert!(!boundary_port_id_valid("MicIn")); // uppercase
        assert!(!boundary_port_id_valid("mic-in")); // hyphen
        assert!(!boundary_port_id_valid("mic in")); // space
        assert!(!boundary_port_id_valid("_mic")); // starts with underscore
    }
}
