use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use engine::{Connection, Engine, NodeDef, NodeRegistry, World};
use schemars::schema_for;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

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

    /// Run all world files in a directory (or a single file) and check AssertNear assertions.
    Test {
        /// Path to a world file or directory of world files.
        path: PathBuf,

        /// Duration of simulated audio time (default 1s).
        #[arg(long, default_value = "1s")]
        duration: String,

        #[arg(long, default_value_t = 48000)]
        sample_rate: u32,

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
    node_synth_sine::register(&mut registry);
    node_synth_vibrato_sine::register(&mut registry);
    node_synth_pink_noise::register(&mut registry);
    node_mix_sum::register(&mut registry);
    node_rms_meter::register(&mut registry);
    node_assert_near::register(&mut registry);
    node_gain::register(&mut registry);
    node_passthrough::register(&mut registry);
    node_null_sink::register(&mut registry);
    node_pitch_error::register(&mut registry);
    node_pitch_yin::register(&mut registry);
    node_tracer::register(&mut registry);
    node_vibrato::register(&mut registry);
    node_synth_onsets::register(&mut registry);
    node_onset::register(&mut registry);
    node_synth_breath::register(&mut registry);
    node_breath::register(&mut registry);
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
    registry: &NodeRegistry,
    sample_rate: u32,
    block_size: usize,
) -> Result<()> {
    let (mut engine, legend) =
        build_engine_for_world(world_path, trace_ports, registry, sample_rate, block_size)?;
    for (tracer_id, port_path) in &legend {
        eprintln!("# {tracer_id} = {port_path}");
    }
    let total_samples = parse_duration_samples(duration_str, sample_rate)?;
    let n_blocks = total_samples.div_ceil(block_size as u64);
    engine.run_blocks(n_blocks);
    Ok(())
}

struct TestSummary {
    #[allow(dead_code)]
    passed: usize,
    failed: usize,
}

fn cmd_test(
    path: &PathBuf,
    duration_str: &str,
    sample_rate: u32,
    block_size: usize,
    registry: &NodeRegistry,
) -> Result<TestSummary> {
    // Collect world files: directory → sorted *.json; file → just that file.
    let world_files: Vec<PathBuf> = if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("reading directory {}", path.display()))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let p = entry.path();
                if p.extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
                {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        files.sort();
        files
    } else {
        vec![path.clone()]
    };

    let mut passed = 0usize;
    let mut failed = 0usize;

    for world_path in &world_files {
        let result = run_test_world(world_path, duration_str, sample_rate, block_size, registry);
        match result {
            Ok(()) => passed += 1,
            Err(e) => {
                eprintln!("FAILED {}: {e}", world_path.display());
                failed += 1;
            }
        }
    }

    println!("test summary: {passed} passed, {failed} failed");

    Ok(TestSummary { passed, failed })
}

fn run_test_world(
    world_path: &PathBuf,
    duration_str: &str,
    sample_rate: u32,
    block_size: usize,
    registry: &NodeRegistry,
) -> Result<()> {
    let (mut engine, _legend) =
        build_engine_for_world(world_path, &[], registry, sample_rate, block_size)?;

    let total_samples = parse_duration_samples(duration_str, sample_rate)?;
    let n_blocks = total_samples.div_ceil(block_size as u64);
    engine.run_blocks(n_blocks);

    // finish() prints the per-node summary lines; collect any failures.
    let results = engine.finish();
    let node_failures: Vec<String> = results
        .into_iter()
        .filter_map(|(id, r)| r.err().map(|e| format!("{id}: {e}")))
        .collect();

    if node_failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", node_failures.join("; "))
    }
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
            sample_rate,
            block_size,
        } => cmd_run(world, duration, trace, &registry, *sample_rate, *block_size),
        Command::Test {
            path,
            duration,
            sample_rate,
            block_size,
        } => match cmd_test(path, duration, *sample_rate, *block_size, &registry) {
            Ok(summary) if summary.failed > 0 => return ExitCode::FAILURE,
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        },
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
        assert!(types.contains(&"AssertNear"));
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
