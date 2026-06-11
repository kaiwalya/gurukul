# Gurukul

An AI singing coach. Detection, diagnosis, and pedagogical feedback for singers.

Gurukul is not a tuner. A tuner tells you that you were flat. A coach tells you *why* — that your tongue root retracted on the high note, that your chest voice was holding too high into the passaggio, that your vibrato is 7Hz (fast, likely a tension tremor), and that your breath support collapsed two beats before the phrase ended. Gurukul is being built to close that gap.

## Orientation

- [`docs/VISION.md`](docs/VISION.md) — what gurukul is and who it's for
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — engine / plugin / world split, ECS layering, port addressing
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — staged build plan and current phase status
- [`docs/TESTING.md`](docs/TESTING.md) — synthesis-as-oracle testing strategy
- [`docs/RESEARCH.md`](docs/RESEARCH.md) — voice modelling frontier notes + references

## Quick start

```
cargo run -p dsp-bench -- --help                       # all commands
cargo run -p dsp-bench -- list-nodes                   # registered node types
cargo run -p dsp-bench -- describe-node <name>         # ports and parameters
cargo run -p dsp-bench -- validate dsp/worlds/hello.json
cargo run -p dsp-bench -- run dsp/worlds/hello.json --duration 2s --dump-events <port>
cargo run -p dsp-bench -- render dsp/worlds/hello.json | dot -Tsvg > graph.svg
cargo test --workspace --release                      # full test suite
```

See [`dsp/worlds/hello.json`](dsp/worlds/hello.json) for the canonical demo graph — hand-edit it to rewire, add, or remove nodes.

The Bevy coach app:

```
./scripts/fetch-assets.sh    # once: fetch the Devanagari UI font (not in git)
cargo run -p coach-game
cargo run -p coach-game -- --replay   # re-run the newest UX trace (no mic/engine)
```

Skipping the fetch is harmless — the app falls back to its built-in font, and only the Sargam-Devanagari note labels render as missing glyphs.

Every run records a UX trace to `traces/` (gitignored); mechanics in [`apps/coach-game/AGENTS.md`](apps/coach-game/AGENTS.md), debugging workflow in [`apps/coach-game/CONTRIBUTING.md`](apps/coach-game/CONTRIBUTING.md).
