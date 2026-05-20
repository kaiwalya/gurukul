# Working in `dsp/`

The dsp layer: the engine, its nodes, the world schema, and example worlds.
Project-wide rules are in [`../AGENTS.md`](../AGENTS.md); this file covers
what's specific to `dsp/`.

## Where information lives

- DSP architecture → [`ARCHITECTURE.md`](ARCHITECTURE.md)
- Node types, ports, parameters → `cargo run -p dsp-cli -- list-nodes` and `describe-node <name>`
- CLI commands and flags → `cargo run -p dsp-cli -- --help`. Don't restate semantics in Markdown.
- World file format → [`schema/world.schema.json`](schema/world.schema.json), derived from Rust types.

## Rules that aren't obvious from the code

- **The world JSON Schema is the interface contract,** not a debug dump. Editors, agents, and humans author against it. Treat the JSON format as a public API; breaking it is a versioned interface change.
- **One crate per node: `node-<name>/`.** Not grouped under `nodes-core/` or similar. Each node earns its own workspace member — matches `ARCHITECTURE.md`'s single-file-scope rule and gives moon finer-grained caching.
- **Realtime discipline is phase-gated.** `ARCHITECTURE.md` says no allocations / no locks in `process()`, but that's aspirational until phase 1.2+ when real DSP lands on a hot path. Before then, ergonomics wins. From 1.2, hold the line.
