# Working in `domain-ports/`

This crate defines the trait contracts (ports) that `domain-adapters/` implement.

**Before adding or modifying a port, read [`src/lib.rs`](src/lib.rs).** Its
module docs are the source of truth for the port convention: one app-facing
trait per port module (named after the domain, no `Trait` suffix), the adapter
`pub fn new(...) -> impl <Domain>` factory shape, the rule that the port —
not the app — owns the lifecycle policy, and the `test-util` Cargo feature
that gates per-port test fakes.

Layout:

- `src/lib.rs` — convention doc + one-line index (`pub mod <domain>;`)
- `src/<domain>.rs` — one file per port. Open the module file to see the
  contract.

## The `<Domain>Core` pattern

Some ports need shared logic that every adapter would otherwise re-implement —
context merging for telemetry, retry/buffering for network ports, lifecycle
state machines, contention rules, etc. Without help, each adapter would
reproduce that logic and drift over time.

The escape hatch: expose a **`<Domain>Core` struct** alongside the trait.

- **`trait <Domain>`** — the contract apps depend on. Stays small and stable.
- **`struct <Domain>Core`** — a *struct, not a trait*. Holds the shared
  state (a context bag, a retry budget, a state machine) and exposes helper
  methods adapters call. Lives in the same `src/<domain>.rs` as the trait.

Adapters compose `Core` into themselves as a field and call its helpers from
their `impl <Domain>`:

```rust,ignore
// in an adapter crate:
struct StderrTelemetry {
    core: TelemetryCore,
    out: io::Stderr,
}

impl Telemetry for StderrTelemetry {
    fn log(&self, level: Level, msg: &str, fields: &Fields) {
        let merged = self.core.merge(fields);   // shared logic, port-side
        writeln!(self.out, "[{level}] {msg} {merged}").ok();   // adapter-side I/O
    }
    // ...
}
```

Apps never see `<Domain>Core` — they hold `Arc<dyn <Domain>>` (or whatever
the trait returns from its adapter factory). The Core is a private contract
between the port and its adapters.

**Why a struct, not another trait:** the goal is *shared code*, not another
contract for adapters to satisfy. A struct gives one canonical implementation;
a trait would force every adapter to either implement it again or wrap a
canonical implementor, which is exactly the boilerplate we're avoiding.

**Adapters retain ownership of `impl <Domain>`.** An unusual adapter
(asynchronous, batched, broadcasting to multiple sinks) can ignore parts of
Core or substitute its own state. The trait-impl is the adapter's, not the
port's.

**Use this only when shared logic actually exists.** Clock has none — its
adapter is a one-liner. Telemetry has plenty (context merging, level
filtering). Most ports will fall on one side or the other clearly.

## Project-wide rules

[`../AGENTS.md`](../AGENTS.md).
