# Working on gurukul

An AI singing coach. See [`docs/VISION.md`](docs/VISION.md).

The repo splits into layers, each with its own `AGENTS.md` where local
rules apply:

- [`dsp/AGENTS.md`](dsp/AGENTS.md) — engine, nodes, world schema, example worlds
- `domain-ports/`, `domain-adapters/` — port traits and their adapter impls. The port convention lives in [`domain-ports/src/lib.rs`](domain-ports/src/lib.rs) (code is the source of truth).
- `apps/` — host applications (CLI, mac)

## Where information lives

Every fact has one source of truth. Don't duplicate it elsewhere; link instead.

- Product vision, roadmap, testing, research → [`docs/*.md`](docs/)
- Phase status → [`docs/ROADMAP.md`](docs/ROADMAP.md) (the "Current phase" line). Never restate phase state in `README.md`, commits, or other docs.
- Workspace / crate list → [`Cargo.toml`](Cargo.toml) workspace members.
- Quick-start commands → [`README.md`](README.md).

Corollary: **no per-directory `README.md`.** Code + tool output is the authoritative surface. Per-crate READMEs restate what `--help`, `describe-node`, and `lib.rs` already say, and drift the moment signatures change.

## Rules that aren't obvious from the code

- **Scope discipline: honor the "Explicitly deferred" lists.** Each phase in `ROADMAP.md` (and each plan file) names what *not* to build. Don't wander into the visual editor, synth library, or UI before their phase. If unsure, ask.

## Conventions

- Run `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, and `cargo test --workspace` clean before committing. Don't skip hooks.
- Never create documentation files unless explicitly asked. This includes per-crate READMEs, design docs, and summary files.
- Commit messages: short imperative subject, body explains *why*. Use multi-line HEREDOCs for formatting (see recent history).
