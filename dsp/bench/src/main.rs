use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dsp_bench::build_registry;
use engine::{Connection, Engine, NodeDef, NodeRegistry, World};
use schemars::schema_for;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "dsp-bench",
    about = "Gurukul DSP engine bench: inspect nodes, run and test worlds"
)]
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

        /// Internal node ports to read via the engine's peek API (e.g. pitch_yin.f0).
        /// May be repeated. Unlike --trace, --peek does not modify the graph; it
        /// reads internal buffers after each block. CSV rows are written to stdout:
        /// `block,frame,<peek_path_1>,<peek_path_2>,...`
        #[arg(long = "peek", value_name = "NODE.PORT", num_args = 1..)]
        peek: Vec<String>,

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
    let (inputs, outputs) = registry
        .ports(name)
        .with_context(|| format!("unknown node type '{name}'"))?;
    let params = registry
        .parameters(name)
        .with_context(|| format!("unknown node type '{name}'"))?;

    println!("Node: {name}");
    println!();

    println!("Inputs ({}):", inputs.len());
    if inputs.is_empty() {
        println!("  (none)");
    }
    for p in inputs {
        println!("  {} [{:?}]", p.name, p.ty);
    }

    println!("Outputs ({}):", outputs.len());
    if outputs.is_empty() {
        println!("  (none)");
    }
    for p in outputs {
        println!("  {} [{:?}]", p.name, p.ty);
    }

    println!("Parameters ({}):", params.len());
    if params.is_empty() {
        println!("  (none)");
    }
    for p in params {
        if p.unit.is_empty() {
            println!(
                "  {} (default={}, min={}, max={})",
                p.name, p.default, p.min, p.max
            );
        } else {
            println!(
                "  {} [{}] (default={}, min={}, max={})",
                p.name, p.unit, p.default, p.min, p.max
            );
        }
    }

    Ok(())
}

fn cmd_validate(world_path: &PathBuf, registry: &NodeRegistry) -> Result<()> {
    let raw = std::fs::read_to_string(world_path)
        .with_context(|| format!("reading {}", world_path.display()))?;

    // Schema validation first — use the registry-augmented schema so unknown
    // node types and unknown params are rejected here, not in Engine::build.
    let schema_json = build_world_schema(registry)?;
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
/// Returns the rewritten world and a legend `Vec<(tracer_id, observed_port_path)>`
/// so the caller can print a human-readable mapping. Tracer ids are simple
/// `trace_N` strings — they carry no encoded information.
fn splice_tracers(
    world: &World,
    trace_ports: &[String],
    registry: &NodeRegistry,
) -> Result<(World, Vec<(String, String)>)> {
    let mut world = world.clone();
    let mut legend: Vec<(String, String)> = Vec::with_capacity(trace_ports.len());

    // Pick a unique prefix that doesn't collide with any existing node id.
    let mut prefix = "trace_".to_string();
    while world.nodes.iter().any(|n| n.id.starts_with(&prefix)) {
        prefix.insert(0, '_');
    }

    for (counter, port_path) in trace_ports.iter().enumerate() {
        let (node_id, port_name) = port_path.split_once('.').with_context(|| {
            format!("invalid port path '{port_path}': expected '<node_id>.<port_name>'")
        })?;

        // Verify the node exists.
        if !world.nodes.iter().any(|n| n.id == node_id) {
            anyhow::bail!("node '{node_id}' not found");
        }

        // Determine if the port is an input or output of that node.
        // Use params-aware lookup so variadic nodes (e.g. MixSum with channels=3) resolve correctly.
        let node_def = world.nodes.iter().find(|n| n.id == node_id).unwrap();
        let node_ty = node_def.ty.clone();
        let node_params = node_def.params.clone();
        let (inputs, outputs) = registry
            .ports_for_params(&node_ty, &node_params)
            .with_context(|| format!("unknown node type '{node_ty}'"))?;

        let is_output = outputs.iter().any(|p| p.name == port_name);
        let is_input = inputs.iter().any(|p| p.name == port_name);

        // If it's both (shouldn't happen by design), prefer output.
        if !is_output && !is_input {
            anyhow::bail!("port '{port_name}' not found on node '{node_id}'");
        }

        let tracer_id = format!("{prefix}{counter}");
        legend.push((tracer_id.clone(), port_path.clone()));

        world.nodes.push(NodeDef {
            id: tracer_id.clone(),
            ty: "Tracer".to_string(),
            params: HashMap::new(),
            name: None,
            description: None,
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

    Ok((world, legend))
}

/// Build an engine for a world file, optionally splicing tracers. Returns the
/// engine and (if tracers were spliced) the tracer legend.
fn build_engine_for_world(
    world_path: &PathBuf,
    trace_ports: &[String],
    registry: &NodeRegistry,
    sample_rate: u32,
    block_size: usize,
) -> Result<(Engine, Vec<(String, String)>)> {
    let world = load_world(world_path)?;
    let (world, legend) = if trace_ports.is_empty() {
        (world, Vec::new())
    } else {
        splice_tracers(&world, trace_ports, registry)?
    };
    let engine =
        Engine::build(&world, registry, sample_rate, block_size).context("building engine")?;
    Ok((engine, legend))
}

fn cmd_run(
    world_path: &PathBuf,
    duration_str: &str,
    trace_ports: &[String],
    peek_ports: &[String],
    registry: &NodeRegistry,
    sample_rate: u32,
    block_size: usize,
) -> Result<()> {
    let (mut engine, legend) =
        build_engine_for_world(world_path, trace_ports, registry, sample_rate, block_size)?;
    for (tracer_id, port_path) in &legend {
        eprintln!("# {tracer_id} = {port_path}");
    }

    // Pre-validate peek paths: every NODE.PORT must resolve before we run.
    // peek() returns Err if missing, but we want to fail upfront, not mid-stream.
    let peek_targets: Vec<(String, String)> = peek_ports
        .iter()
        .map(|p| {
            p.split_once('.')
                .map(|(n, port)| (n.to_string(), port.to_string()))
                .with_context(|| {
                    format!("invalid --peek path '{p}': expected '<node_id>.<port_name>'")
                })
        })
        .collect::<Result<_>>()?;
    for (node_id, port) in &peek_targets {
        engine
            .peek(node_id, port)
            .with_context(|| format!("--peek {node_id}.{port}"))?;
    }

    let total_samples = parse_duration_samples(duration_str, sample_rate)?;
    let n_blocks = total_samples.div_ceil(block_size as u64);

    if peek_targets.is_empty() {
        engine.run_blocks(n_blocks);
    } else {
        // CSV header: block,frame,<path_1>,<path_2>,...
        // Format is f32 only; revisit if peek ever returns non-numeric data.
        let header_paths: Vec<String> = peek_ports.to_vec();
        println!("block,frame,{}", header_paths.join(","));

        let mut stdout = std::io::stdout().lock();
        use std::io::Write;
        // Debug-only path: per-block run_blocks(1) + per-block string lookup in
        // engine.peek() is deliberate. Slower than a bulk run_blocks(n) by
        // design, since we need to snapshot internal state between blocks. Not
        // for realtime / hot-path use.
        for block_idx in 0..n_blocks {
            engine.run_blocks(1);
            // Snapshot peek buffers. unwrap() is justified: paths were
            // pre-validated above before the run loop.
            let snapshots: Vec<&[f32]> = peek_targets
                .iter()
                .map(|(n, p)| engine.peek(n, p).unwrap())
                .collect();
            // All snapshots must share a length — same block, different ports.
            // Today every node port returns block_size samples, but a future
            // sparse Feature port could break this; assert loudly rather than
            // index out of bounds.
            let n_frames = snapshots[0].len();
            for (i, snap) in snapshots.iter().enumerate().skip(1) {
                if snap.len() != n_frames {
                    anyhow::bail!(
                        "--peek buffer length mismatch: '{}' = {}, '{}' = {}",
                        peek_ports[0],
                        n_frames,
                        peek_ports[i],
                        snap.len()
                    );
                }
            }
            for frame in 0..n_frames {
                write!(stdout, "{block_idx},{frame}")?;
                for snap in &snapshots {
                    write!(stdout, ",{}", snap[frame])?;
                }
                writeln!(stdout)?;
            }
        }
    }
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

    println!("digraph dsp {{");
    println!("  rankdir=LR;");
    println!("  node [shape=box, fontname=\"monospace\"];");
    println!();

    for node_def in &world.nodes {
        let (inputs, outputs) = registry.ports(&node_def.ty).unwrap();
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

/// Build the world JSON Schema with per-node-type `NodeDef` variants drawn
/// from the registry. Each variant pins `type` to a `const` string and
/// constrains `params` to that node's declared parameters (name + min/max).
/// This is what makes the world schema the actual interface contract: an
/// editor or agent authoring a world file gets per-type completion and
/// validation out of the box.
fn build_world_schema(registry: &NodeRegistry) -> Result<serde_json::Value> {
    let schema = schema_for!(World);
    let mut value = serde_json::to_value(&schema).context("serializing schema")?;

    let definitions = value
        .get_mut("definitions")
        .and_then(|d| d.as_object_mut())
        .context("schema missing 'definitions'")?;

    // Inject `pattern` onto BoundaryPort.id. The regex is enforced by the
    // engine but schemars 0.8 has no derive attribute for emitting `pattern`,
    // so we add it in post-processing. This keeps editors and the visual editor
    // in sync with the runtime validation rule.
    if let Some(boundary_port) = definitions.get_mut("BoundaryPort")
        && let Some(id_schema) = boundary_port
            .get_mut("properties")
            .and_then(|p| p.get_mut("id"))
        && let Some(obj) = id_schema.as_object_mut()
    {
        obj.insert(
            "pattern".into(),
            serde_json::Value::String("^[a-z][a-z0-9_]*$".into()),
        );
    }

    let mut variants: Vec<serde_json::Value> = Vec::new();
    for ty in registry.node_types() {
        let params = registry.parameters(ty).unwrap_or(&[]);

        let mut param_props = serde_json::Map::new();
        for p in params {
            let mut spec = serde_json::Map::new();
            spec.insert("type".into(), serde_json::Value::String("number".into()));
            if p.default.is_finite() {
                spec.insert("default".into(), serde_json::Value::from(p.default));
            }
            if p.min.is_finite() {
                spec.insert("minimum".into(), serde_json::Value::from(p.min));
            }
            if p.max.is_finite() {
                spec.insert("maximum".into(), serde_json::Value::from(p.max));
            }
            if !p.unit.is_empty() {
                spec.insert(
                    "description".into(),
                    serde_json::Value::String(format!("unit: {}", p.unit)),
                );
            }
            param_props.insert(p.name.to_string(), serde_json::Value::Object(spec));
        }

        let params_schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": serde_json::Value::Object(param_props),
        });

        let variant = serde_json::json!({
            "type": "object",
            "required": ["id", "type"],
            "properties": {
                "id": { "type": "string", "minLength": 1 },
                "type": { "const": ty },
                "params": params_schema,
                "name": { "type": "string", "description": "Human-readable label (optional, cosmetic)." },
                "description": { "type": "string", "description": "Free-form tooltip / API doc (optional)." },
            },
            "additionalProperties": false,
        });
        variants.push(variant);
    }

    definitions.insert(
        "NodeDef".to_string(),
        serde_json::json!({ "oneOf": variants }),
    );

    Ok(value)
}

fn cmd_emit_schema(registry: &NodeRegistry) -> Result<()> {
    let value = build_world_schema(registry)?;
    let json = serde_json::to_string_pretty(&value).context("serializing schema")?;
    println!("{json}");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let registry = build_registry();

    let result = match &cli.command {
        Command::ListNodes => {
            cmd_list_nodes(&registry);
            Ok(())
        }
        Command::DescribeNode { name } => cmd_describe_node(&registry, name),
        Command::Validate { world } => cmd_validate(world, &registry),
        Command::Run {
            world,
            duration,
            trace,
            peek,
            sample_rate,
            block_size,
        } => cmd_run(
            world,
            duration,
            trace,
            peek,
            &registry,
            *sample_rate,
            *block_size,
        ),
        Command::Render {
            world,
            sample_rate,
            block_size,
        } => cmd_render(world, &registry, *sample_rate, *block_size),
        Command::EmitSchema => cmd_emit_schema(&registry),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
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
        assert!(types.contains(&"SynthSine"));
        assert!(types.contains(&"SynthVibratoSine"));
        assert!(types.contains(&"SynthPinkNoise"));
        assert!(types.contains(&"MixSum"));
        assert!(types.contains(&"RmsMeter"));
        assert!(types.contains(&"GainNode"));
        assert!(types.contains(&"Passthrough"));
        assert!(types.contains(&"NullSink"));
        assert!(types.contains(&"PitchError"));
        assert!(types.contains(&"PitchYin"));
        assert!(types.contains(&"Tracer"));
        assert!(types.contains(&"Vibrato"));
        assert!(types.contains(&"SynthOnsets"));
        assert!(types.contains(&"Onset"));
        assert!(types.contains(&"SynthBreath"));
        assert!(types.contains(&"Breath"));
    }

    #[test]
    fn schema_rejects_unknown_node_type() {
        let registry = build_registry();
        let schema = build_world_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let bad: serde_json::Value =
            serde_json::from_str(r#"{"nodes":[{"id":"x","type":"NoSuchNode"}],"connections":[]}"#)
                .unwrap();
        assert!(validator.iter_errors(&bad).next().is_some());
    }

    #[test]
    fn schema_rejects_unknown_param() {
        let registry = build_registry();
        let schema = build_world_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let bad: serde_json::Value = serde_json::from_str(
            r#"{"nodes":[{"id":"x","type":"SynthSine","params":{"chickens":17}}],"connections":[]}"#,
        )
        .unwrap();
        assert!(validator.iter_errors(&bad).next().is_some());
    }

    #[test]
    fn peek_returns_expected_samples() {
        // Build a SynthSine world programmatically, run one block, peek the
        // sine output, confirm the buffer is non-zero and finite.
        let registry = build_registry();
        let world: World = serde_json::from_str(
            r#"{"nodes":[{"id":"src","type":"SynthSine","params":{"freq":440.0,"amplitude":0.5}}],"connections":[]}"#,
        )
        .unwrap();

        let mut engine = Engine::build(&world, &registry, 48000, 64).unwrap();
        engine.run_blocks(1);

        let buf = engine.peek("src", "audio_out").unwrap();
        assert_eq!(buf.len(), 64);
        assert!(
            buf.iter().any(|&s| s.abs() > 1e-6),
            "expected non-zero sine"
        );
        assert!(buf.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn peek_two_ports_same_length() {
        // Two SynthSine sources, peek both, confirm peek buffers stay aligned.
        let registry = build_registry();
        let world: World = serde_json::from_str(
            r#"{
                "nodes":[
                    {"id":"a","type":"SynthSine","params":{"freq":440.0,"amplitude":0.3}},
                    {"id":"b","type":"SynthSine","params":{"freq":880.0,"amplitude":0.3}}
                ],
                "connections":[]
            }"#,
        )
        .unwrap();

        let block_size = 32;
        let mut engine = Engine::build(&world, &registry, 48000, block_size).unwrap();
        engine.run_blocks(2);
        let buf_a = engine.peek("a", "audio_out").unwrap();
        let buf_b = engine.peek("b", "audio_out").unwrap();
        assert_eq!(buf_a.len(), buf_b.len());
        assert_eq!(buf_a.len(), block_size);
    }

    #[test]
    fn peek_unknown_node_errors() {
        let registry = build_registry();
        let world: World =
            serde_json::from_str(r#"{"nodes":[{"id":"src","type":"SynthSine"}],"connections":[]}"#)
                .unwrap();
        let engine = Engine::build(&world, &registry, 48000, 64).unwrap();
        assert!(engine.peek("nope", "audio_out").is_err());
        assert!(engine.peek("src", "no_such_port").is_err());
    }

    #[test]
    fn schema_accepts_known_world() {
        let registry = build_registry();
        let schema = build_world_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let good: serde_json::Value = serde_json::from_str(
            r#"{"nodes":[{"id":"src","type":"SynthSine","params":{"freq":440.0,"amplitude":0.5}}],"connections":[]}"#,
        )
        .unwrap();
        let errors: Vec<_> = validator.iter_errors(&good).collect();
        assert!(errors.is_empty(), "unexpected schema errors: {errors:?}");
    }
}
