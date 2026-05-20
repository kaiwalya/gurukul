# Working in `domain-adapters/`

Each subdirectory is a crate that implements one port from
[`../domain-ports/`](../domain-ports/).

**Before adding or modifying an adapter, read the port convention in
[`../domain-ports/src/lib.rs`](../domain-ports/src/lib.rs).** Key rules
that affect this directory:

- Expose exactly one factory: `pub fn new(...) -> impl <Domain>`. The
  concrete type stays private — callers only see `impl <Domain>`.
- Apps call `new()` once at boot in real usage; tests may call freely or
  substitute fakes via `domain-ports`' `test-util` feature.
- The trait — not the adapter — owns lifecycle policy. The adapter
  *enforces* whatever the port's trait shape requires; it doesn't invent
  its own.

Naming: directory is the bare port name (`clock/`, not `clock-std/`); crate
name is `adapter-<port>` (matches the moon project ID so the
inherited `cargo build -p $project` task just works). The directory
tree (`domain-adapters/`) preserves the layer; the crate name stays
short because call sites read it on every line (`adapter_clock::new()`).

Project-wide rules: [`../AGENTS.md`](../AGENTS.md).
