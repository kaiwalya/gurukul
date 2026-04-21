# Working on gurukul

An AI singing coach. See [`docs/VISION.md`](docs/VISION.md).

## Where information lives

Every fact has one source of truth. Don't duplicate it elsewhere; link instead.

- Product vision, architecture, roadmap, testing, research → [`docs/*.md`](docs/)
- Phase status → [`docs/ROADMAP.md`](docs/ROADMAP.md) (the "Current phase" line). Never restate phase state in `README.md`, commits, or other docs.
- Workspace / crate list → [`Cargo.toml`](Cargo.toml) workspace members.
- Node types, ports, parameters → `cargo run -p cli -- list-nodes` and `describe-node <name>`.
- CLI commands and flags → `cargo run -p cli -- --help`. Don't restate semantics in Markdown.
- World file format → [`schema/world.schema.json`](schema/world.schema.json), derived from Rust types.
- Quick-start commands → [`README.md`](README.md).

Corollary: **no per-directory `README.md`.** Code + tool output is the authoritative surface. Per-crate READMEs restate what `describe-node`, `--help`, and `lib.rs` already say, and drift the moment signatures change.

## Rules that aren't obvious from the code

- **The world JSON Schema is the interface contract,** not a debug dump. Editors, agents, and humans author against it. Treat the JSON format as a public API; breaking it is a versioned interface change.
- **One crate per node: `node-<name>/`.** Not grouped under `nodes-core/` or similar. Each node earns its own workspace member — matches `ARCHITECTURE.md`'s single-file-scope rule and gives moon finer-grained caching.
- **Realtime discipline is phase-gated.** `ARCHITECTURE.md` says no allocations / no locks in `process()`, but that's aspirational until phase 1.2+ when real DSP lands on a hot path. Before then, ergonomics wins. From 1.2, hold the line.
- **Scope discipline: honor the "Explicitly deferred" lists.** Each phase in `ROADMAP.md` (and each plan file) names what *not* to build. Don't wander into the visual editor, synth library, or UI before their phase. If unsure, ask.

## Conventions

- Run `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, and `cargo test --workspace` clean before committing. Don't skip hooks.
- Never create documentation files unless explicitly asked. This includes per-crate READMEs, design docs, and summary files.
- Commit messages: short imperative subject, body explains *why*. Use multi-line HEREDOCs for formatting (see recent history).
