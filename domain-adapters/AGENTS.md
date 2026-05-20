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

Naming: each adapter is qualified by a *flavor* suffix identifying the
backing tech / platform (`-std` for std-library / host OS defaults,
`-mac` for CoreAudio / AppKit / etc., `-watch` for watchOS, ...). One
port can have multiple sibling adapters (`telemetry-std`,
`telemetry-mac`), so flavors are required even when only one exists
today.

- Directory: `<port>-<flavor>/` (e.g. `clock-std/`, `telemetry-std/`).
- Crate name: `adapter-<port>-<flavor>` (e.g. `adapter-clock-std`).
  Matches the moon project ID so the inherited `cargo build -p $project`
  task just works.
- Call sites: `adapter_clock_std::new()`. Verbose but unambiguous —
  hosts wire adapters once at boot, so the cost is paid in one place.

The directory tree (`domain-adapters/`) preserves the layer; the
flavor suffix makes sibling adapters distinguishable on disk and in
`Cargo.toml` without resorting to nested directories.

Project-wide rules: [`../AGENTS.md`](../AGENTS.md).
