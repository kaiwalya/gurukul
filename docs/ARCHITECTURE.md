# Architecture

Gurukul is built in three architectural layers, deliberately separated:

1. **Engine** — a dataflow-graph runtime that schedules and executes analysis nodes on streaming audio/sensor data.
2. **Plugins (nodes)** — the unit of extension: sources, feature extractors, diagnostic analyzers, aligners, coaches, sinks.
3. **Worlds (graphs)** — declarative topologies (JSON/YAML) describing which nodes are wired together for a given mode of operation.

This is the same separation that game engines (Unity, Unreal), DAWs (Reaper, Ableton, Live, Bitwig), and visual-effects pipelines (TouchDesigner, Max/MSP) all converged on independently. The reason to adopt it here is that it decouples three things with very different change rates and authors:

| Layer | Change rate | Author | Deployment |
|---|---|---|---|
| Engine | Rarely | Core team | Ships in the binary |
| Plugins | Often | Core team + third parties | Library, versioned |
| Worlds | Constantly | Non-programmers (voice coaches) | Data files |

Conflating these is the single biggest way similar products ossify.

## The engine

The engine is the realtime dataflow runtime. It is responsible for:

- **Scheduling** — topological sort of the graph, parallelising where possible.
- **Clock** — sample-accurate at a configurable block size (expected default: 512 samples at 48 kHz, ~10.7ms blocks). Events are timestamped.
- **Typed ports** — nodes declare input/output ports by type: `audio`, `control`, `event`, `feature`, `articulatory-state`. Engine wires them with lock-free ring buffers.
- **Parameter system** — every node exposes typed parameters (range, curve, default). Engine owns automation, undo/redo, presets, remote control.
- **Plugin Delay Compensation (PDC)** — each node declares its inherent latency; engine aligns downstream consumers so timestamps stay coherent.
- **Threading discipline** — the DSP thread is sacred: no allocations, no locks, no syscalls, no logging. UI / coaching-layer threads are separate. Cross-boundary communication via lock-free queues.
- **Capability negotiation** — nodes declare required features (sample rate, block size, sensor inputs); engine either provides or fails cleanly. Modelled after CLAP's extension mechanism.

The engine is the **only** place that owns shared state and threading. Nothing else should.

### What the engine is not

- **Not an ECS.** The DSP graph has tens of heterogeneous nodes with distinct identity and data dependencies between them. ECS (component-major storage, uniform iteration) solves a different problem and would give worse cache behaviour and worse ergonomics here. See "ECS layering" below for where ECS *does* fit.
- **Not running on microcontrollers.** The engine runs on phone / desktop / server tiers.

## Plugins (nodes)

Every node implements a small, stable contract:

```
class Node:
    declare_ports() -> {inputs, outputs}              # audio/control/event/feature
    declare_parameters() -> [ParamSpec]               # name, range, curve, default
    declare_latency() -> samples
    prepare(sample_rate, block_size) -> None          # called once
    process(inputs, outputs, events, nframes)         # called every block
    serialize_state() / restore_state()               # presets, sessions
```

This is VST3/CLAP/AU with the names changed. Do not invent a new protocol — scope the existing pattern to this domain.

### Node categories

- **Sources** — mic, file, test-signal generator, synthesized reference.
- **Sensors** — front-camera lip/jaw, watch accelerometer (breath mechanics), chin ultrasound, EGG collar, airflow sensor. Each is a source emitting a different typed stream.
- **Feature extractors** — pitch (YIN, CREPE, pYIN as alternative implementations of the same interface), formants, spectral tilt, onset, vibrato analyzer, H1–H2, glottal inverse filter, articulatory inverter.
- **Analyzers / diagnostics** — registration classifier, passaggio-transition detector, tension detector, breath-support estimator.
- **Aligners** — DTW against reference, per-feature diff emitter.
- **Coaches** — rule-based cue generator, LLM-backed cue generator, curriculum selector. Emit structured *coaching events*, not audio.
- **Sinks** — visualizer, haptic output, TTS feedback output, log, test-oracle comparator.

### The synthesizer/analyzer duality

A feature *extractor* and a feature *synthesizer* are the same interface running in opposite directions. A vibrato synthesizer takes `(rate, depth)` parameters and emits audio; a vibrato analyzer takes audio and emits `(rate, depth)`. Connect them in a loop and you have an automated test (see `TESTING.md`). Connect the analyzer to a visualizer and you have the product. Same engine, different topology.

This duality is load-bearing for the testing strategy and should be preserved as a deliberate design property.

## Worlds (graphs)

A *world* is a declarative JSON/YAML file describing a node graph with parameter bindings. Multiple worlds solve different problems on the same engine:

- **Practice-mode world** — mic → pitch + vibrato + onset → visualizer + haptic. Minimal, low-latency.
- **Lesson-mode world** — mic + reference → feature extractors → DTW aligner → coach → LLM cue generator → TTS output. Heavier, 500ms–2s latency acceptable.
- **Diagnostic-mode world** — mic + camera + watch → full feature stack → articulatory inverter → registration classifier → detailed report. Offline, runs post-performance.
- **Test-mode world** — synth source (parameterised) → impairments → analyzer under test → oracle comparator → pass/fail. The CI harness.
- **Calibration-mode world** — calibrated tone source → mic → frequency-response estimator → per-device calibration profile.

A new mode ships as a new world file. No engine change, no plugin change. A voice coach with a graph editor can author lessons as data.

## ECS layering

Entity-Component-System is the right architecture for several layers of the product that are **not** the realtime DSP graph:

- **Visualization layer.** Pitch-trail dots, onset markers, breath indicators, vibrato envelope traces, tension warnings, timing-grid ticks, per-phrase annotations, coaching-cue balloons. Hundreds to thousands of short-lived entities with composable capabilities, uniformly processed per frame. Archetypal ECS territory (Bevy if Rust, an ECS lib in TS if web).
- **Stories / multi-track lesson editor.** Clips on tracks are entities with `TimeRange`, `AudioSource`, `Visualization`, `Annotation`, `Selectable`, `Trimmable` components. Playback, render, selection systems query by component signature.
- **Session / performance state.** In-memory model of a practice session — takes, annotations, measurements — as entities with components. Persisted to a database for long-term tracking.
- **Coaching event stream.** Diagnostic events (`PitchFlatEvent`, `TensionEvent`, `BreathCollapseEvent`, etc.) as entities; systems prioritise, deduplicate, curriculum-filter, build LLM prompts.
- **Test-fixture worlds.** Each parameterised synthesised phrase is an entity; synth-params, ground-truth, analyzer-output, pass-fail-result are components; systems run detectors and score.

The bridge between the DSP graph and the ECS world is the event stream: DSP nodes emit structured events, ECS systems consume them and spawn/update entities. This mirrors how modern game engines split their audio/render pipelines (stage-based / graph-based) from their simulation world (ECS). The architecture is a *pair of worlds*, not a monolith.

## The editor layer

Once engine + plugins + worlds are separated, the editor becomes obvious:

- Node palette (available plugins)
- Graph canvas (drag, connect, parameterise)
- Inspector (selected node's parameters)
- Transport (start, stop, step, record)
- Monitoring (scopes on any port — DAW-style metering, scope, spectrum analyzer taps)

**Do not build this on day one.** Design the engine so it's possible later. The rule: the editor is a client of the engine's introspection API, equivalent to the runtime being a client of its execution API. Any graph representable in the editor is representable as a file; any graph representable as a file is loadable into the editor. No special runtime-only graphs.

## Cross-cutting principles

- **The event stream is the seam.** DSP emits events; ECS consumes events; coaching layer consumes events; UI consumes events. Don't let components downstream of DSP peek into DSP node internals.
- **Realtime thread is sacred.** Violations cause audible dropouts and non-reproducible bugs. Enforce with lints, sanitizers, and a strict-realtime test harness.
- **Graphs are data.** Text format (JSON/YAML) first, visual editor later. Stable file format is a prerequisite for AI-agent graph authoring.
- **Introspection from day one.** Every node, port, parameter, and event type is queryable by name and type. Required for editor, for tests, and for agent-driven development.
- **Version the interface.** Once there's more than one plugin author, the `Node` contract can't casually change. Start versioned even with one version.
- **Don't premature-generalise.** Build for the ~20 nodes needed for the first three worlds. Abstractions should earn their place.

## Port addressing and subscription

Every port on every node is reachable by a stable string path — e.g. `yin.f0`, `vibrato.rate`, `onset.events`. Paths are assigned by the world file, not by graph-construction order, so they survive reloads, edits, and agent-authored rewrites.

The read side of the seam is a subscription API: `subscribe(path) → Stream<TimestampedValue>`. Many consumers can subscribe to the same port; the producing node does not know or care who is listening. Visualiser widgets, loggers, test oracles, and remote debuggers are all equivalent consumers. In ECS terms (see the visualization layer above), a widget is an entity carrying a `PortBinding(path)` component plus whatever view components it needs; the visualization system reads the subscribed stream each frame and updates the entity's render state.

This is the separation that lets a user — or an agent — say "track this variable" and drop a visualiser onto it without touching the DSP graph. It is also what makes a scope, a meter, a logger, and a test assertion the same kind of object: a typed consumer of a named port.

Two mental models, deliberately:

- **Reason Rack is the runtime model.** Typed jacks on the back, patchable cables, scopes-as-devices, turn-the-rack-around split between wiring (engine graph) and view (visualiser entities). A cable is an in-memory ring buffer between a producer port and a consumer port — continuous dataflow in one runtime, not request/response.
- **INDI is the addressing-layer model.** Stable, string-addressable, self-describing properties that a late-binding client can discover and subscribe to without holding a handle from construction time. This is what makes graphs authorable as data and consumable by tools the engine did not know about at compile time.

Deliberately deferred (decide when the first two nodes exist, not now): exact ring-buffer shape, path-naming grammar, hierarchical vs flat namespaces, the schema for `TimestampedValue`, backpressure policy when a slow consumer can't keep up.

## Why this architecture for AI-driven development

A clean engine/plugin/world split is the single highest-leverage architectural choice for AI-agent-authored development on this product:

- **Graphs are terse, typed, self-describing.** An agent can read/write world files with high confidence — it's JSON with a schema, not sprawling code. Diffs are interpretable.
- **Adding a capability = adding a node.** Strict interface, single-file scope, no "understand the whole app to change one thing."
- **Tests are graphs.** Agent authors synthesizer node + oracle sink + test world, runs it. No human judgment in the loop.
- **Regression debugging = graph diffing.** "Which node's output changed between green and red?" is a structured query, not log-sifting.
- **Capabilities compose.** Agent reasons about "I need a world that does X," searches the node library, writes missing nodes. Closer to how agents actually think than "edit lines 47–93."
