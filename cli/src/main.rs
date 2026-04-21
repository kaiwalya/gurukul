use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use engine::{Connection, Engine, NodeDef, NodeRegistry, World};
use schemars::schema_for;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "gurukul", about = "Gurukul audio engine CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List all registered node types.
    ListNodes,

    /// Describe a node's ports and parameters.
    DescribeNode {
        /// The node type name.
        name: String,
    },

    /// Validate a world JSON file against the schema and graph rules.
    Validate {
        /// Path to the world JSON file.
        world: PathBuf,
    },

    /// Run a world for a fixed simulated duration.
    Run {
        /// Path to the world JSON file.
        world: PathBuf,

        /// Duration of simulated audio time (e.g. 100ms, 2s).
        #[arg(long)]
        duration: String,

        /// Port paths to splice a Tracer node onto (e.g. src.audio_out).
        /// May be repeated: --trace src.audio_out --trace mid.audio_out
        #[arg(long = "trace", value_name = "PORT_PATH", num_args = 1..)]
        trace: Vec<String>,

        /// Sample rate in Hz.
        #[arg(long, default_value_t = 48000)]
        sample_rate: u32,

        /// Block size in frames.
        #[arg(long, default_value_t = 512)]
        block_size: usize,
    },

    /// Render a world as Graphviz .dot output (pipe to `dot -Tsvg`).
    Render {
        /// Path to the world JSON file.
        world: PathBuf,

        /// Sample rate in Hz.
        #[arg(long, default_value_t = 48000)]
        sample_rate: u32,

        /// Block size in frames.
        #[arg(long, default_value_t = 512)]
        block_size: usize,
    },

    /// Emit the JSON Schema for world files to stdout.
    EmitSchema,
}

fn build_registry() -> NodeRegistry {
    let mut registry = NodeRegistry::new();
    node_sine_source::register(&mut registry);
    node_passthrough::register(&mut registry);
    node_null_sink::register(&mut registry);
    node_tracer::register(&mut registry);
    registry
}

fn load_world(path: &PathBuf) -> Result<World> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let world: World =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(world)
}

/// Parse a human duration string like "100ms", "2s", "500ms" into sample count.
fn parse_duration_samples(s: &str, sample_rate: u32) -> Result<u64> {
    if let Some(ms_str) = s.strip_suffix("ms") {
        let ms: f64 = ms_str.parse().context("invalid milliseconds value")?;
        Ok((ms / 1000.0 * sample_rate as f64).ceil() as u64)
    } else if let Some(s_str) = s.strip_suffix('s') {
        let secs: f64 = s_str.parse().context("invalid seconds value")?;
        Ok((secs * sample_rate as f64).ceil() as u64)
    } else {
        anyhow::bail!("duration must end with 'ms' or 's' (e.g. '100ms', '2s')");
    }
}

fn cmd_list_nodes(registry: &NodeRegistry) {
    for name in registry.node_types() {
        println!("{name}");
    }
}

fn cmd_describe_node(registry: &NodeRegistry, name: &str) -> Result<()> {
    // Instantiate a default instance to call declare_* on it.
    let node = registry
        .create(name, &Default::default())
        .with_context(|| format!("unknown node type '{name}'"))?;

    let (inputs, outputs) = node.declare_ports();
    let params = node.declare_parameters();

    println!("Node: {name}");
    println!();

    println!("Inputs ({}):", inputs.len());
    if inputs.is_empty() {
        println!("  (none)");
    }
    for p in &inputs {
        println!("  {} [{:?}]", p.name, p.ty);
    }

    println!("Outputs ({}):", outputs.len());
    if outputs.is_empty() {
        println!("  (none)");
    }
    for p in &outputs {
        println!("  {} [{:?}]", p.name, p.ty);
    }

    println!("Parameters ({}):", params.len());
    if params.is_empty() {
        println!("  (none)");
    }
    for p in &params {
        println!(
            "  {} (default={}, min={}, max={})",
            p.name, p.default, p.min, p.max
        );
    }

    Ok(())
}

fn cmd_validate(world_path: &PathBuf, registry: &NodeRegistry) -> Result<()> {
    let raw = std::fs::read_to_string(world_path)
        .with_context(|| format!("reading {}", world_path.display()))?;

    // Schema validation first
    let schema_json = serde_json::to_value(schema_for!(World)).unwrap();
    let instance: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing JSON in {}", world_path.display()))?;

    let compiled = jsonschema::validator_for(&schema_json).context("compiling JSON Schema")?;
    let errors: Vec<_> = compiled.iter_errors(&instance).collect();
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("Schema error at {}: {}", e.instance_path, e);
        }
        anyhow::bail!("world file failed schema validation");
    }

    // Structural parse
    let world: World =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", world_path.display()))?;

    // Graph build validates node types, port references, and topo sort (cycle detection).
    // sample_rate and block_size don't affect graph validity; use defaults.
    Engine::build(&world, registry, 48000, 512).context("building engine graph")?;

    println!("OK");
    Ok(())
}

/// Splice tracer nodes into a cloned world for each requested port path.
///
/// Node id format: `__trace__{node_id}__{port_name}__{counter}` where `__` separates components.
/// Assumption: user-chosen node ids and port names do not contain double underscores.
fn splice_tracers(world: &World, trace_ports: &[String], registry: &NodeRegistry) -> Result<World> {
    let mut world = world.clone();

    for (counter, port_path) in trace_ports.iter().enumerate() {
        let (node_id, port_name) = port_path.split_once('.').with_context(|| {
            format!("invalid port path '{port_path}': expected '<node_id>.<port_name>'")
        })?;

        // Verify the node exists.
        if !world.nodes.iter().any(|n| n.id == node_id) {
            anyhow::bail!("node '{node_id}' not found");
        }

        // Determine if the port is an input or output of that node.
        let node_def = world.nodes.iter().find(|n| n.id == node_id).unwrap();
        let instance = registry
            .create(&node_def.ty, &node_def.params)
            .with_context(|| format!("unknown node type '{}'", node_def.ty))?;
        let (inputs, outputs) = instance.declare_ports();

        let is_output = outputs.iter().any(|p| p.name == port_name);
        let is_input = inputs.iter().any(|p| p.name == port_name);

        // If it's both (shouldn't happen by design), prefer output.
        if !is_output && !is_input {
            anyhow::bail!("port '{port_name}' not found on node '{node_id}'");
        }

        // Build the tracer node id. Replace `.` in node_id/port_name with `__` would be
        // ambiguous if they already contain `__`, but we assume they don't (documented above).
        let tracer_id = format!("__trace__{node_id}__{port_name}__{counter}");

        world.nodes.push(NodeDef {
            id: tracer_id.clone(),
            ty: "Tracer".to_string(),
            params: HashMap::new(),
        });

        if is_output {
            // Redirect all existing connections FROM this output through the tracer.
            for conn in &mut world.connections {
                if conn.from == *port_path {
                    conn.from = format!("{tracer_id}.audio_out");
                }
            }
            // Feed the source output into the tracer.
            world.connections.push(Connection {
                from: port_path.clone(),
                to: format!("{tracer_id}.audio_in"),
            });
        } else {
            // is_input: redirect all existing connections TO this input through the tracer.
            for conn in &mut world.connections {
                if conn.to == *port_path {
                    conn.to = format!("{tracer_id}.audio_in");
                }
            }
            // Feed the tracer output into the destination input.
            world.connections.push(Connection {
                from: format!("{tracer_id}.audio_out"),
                to: port_path.clone(),
            });
        }
    }

    Ok(world)
}

fn cmd_run(
    world_path: &PathBuf,
    duration_str: &str,
    trace_ports: &[String],
    registry: &NodeRegistry,
    sample_rate: u32,
    block_size: usize,
) -> Result<()> {
    let world = load_world(world_path)?;
    let world = if trace_ports.is_empty() {
        world
    } else {
        splice_tracers(&world, trace_ports, registry)?
    };

    let mut engine =
        Engine::build(&world, registry, sample_rate, block_size).context("building engine")?;

    let total_samples = parse_duration_samples(duration_str, sample_rate)?;
    let n_blocks = total_samples.div_ceil(block_size as u64);

    engine.run_blocks(n_blocks);

    Ok(())
}

fn cmd_render(
    world_path: &PathBuf,
    registry: &NodeRegistry,
    sample_rate: u32,
    block_size: usize,
) -> Result<()> {
    let world = load_world(world_path)?;

    // Validate first
    Engine::build(&world, registry, sample_rate, block_size).context("validating world")?;

    println!("digraph gurukul {{");
    println!("  rankdir=LR;");
    println!("  node [shape=box, fontname=\"monospace\"];");
    println!();

    for node_def in &world.nodes {
        let instance = registry.create(&node_def.ty, &node_def.params).unwrap();
        let (inputs, outputs) = instance.declare_ports();
        let label = format!(
            "{}\\n[{}]\\nin: {}\\nout: {}",
            node_def.id,
            node_def.ty,
            inputs.iter().map(|p| p.name).collect::<Vec<_>>().join(", "),
            outputs
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
                .join(", "),
        );
        println!("  \"{}\" [label=\"{}\"];", node_def.id, label);
    }

    println!();

    for conn in &world.connections {
        let (src_node, src_port) = conn.from.split_once('.').unwrap();
        let (dst_node, dst_port) = conn.to.split_once('.').unwrap();
        println!(
            "  \"{}\" -> \"{}\" [label=\"{} → {}\"];",
            src_node, dst_node, src_port, dst_port
        );
    }

    println!("}}");
    Ok(())
}

fn cmd_emit_schema() -> Result<()> {
    let schema = schema_for!(World);
    let json = serde_json::to_string_pretty(&schema).context("serializing schema")?;
    println!("{json}");
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let registry = build_registry();

    match &cli.command {
        Command::ListNodes => {
            cmd_list_nodes(&registry);
        }
        Command::DescribeNode { name } => {
            cmd_describe_node(&registry, name)?;
        }
        Command::Validate { world } => {
            cmd_validate(world, &registry)?;
        }
        Command::Run {
            world,
            duration,
            trace,
            sample_rate,
            block_size,
        } => {
            cmd_run(world, duration, trace, &registry, *sample_rate, *block_size)?;
        }
        Command::Render {
            world,
            sample_rate,
            block_size,
        } => {
            cmd_render(world, &registry, *sample_rate, *block_size)?;
        }
        Command::EmitSchema => {
            cmd_emit_schema()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_ms() {
        assert_eq!(parse_duration_samples("100ms", 48000).unwrap(), 4800);
    }

    #[test]
    fn parse_duration_s() {
        assert_eq!(parse_duration_samples("2s", 48000).unwrap(), 96000);
    }

    #[test]
    fn parse_duration_fractional() {
        // 500ms at 48000 = 24000
        assert_eq!(parse_duration_samples("500ms", 48000).unwrap(), 24000);
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration_samples("100", 48000).is_err());
    }

    #[test]
    fn registry_has_all_nodes() {
        let registry = build_registry();
        let types = registry.node_types();
        assert!(types.contains(&"SineSource"));
        assert!(types.contains(&"Passthrough"));
        assert!(types.contains(&"NullSink"));
        assert!(types.contains(&"Tracer"));
    }
}
