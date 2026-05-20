# Working in `domain-ports/`

This crate defines the trait contracts (ports) that `domain-adapters/` implement.

**Before adding or modifying a port, read [`src/lib.rs`](src/lib.rs).** Its
module docs are the source of truth for the port convention: one trait per
port module (named after the domain, no `Trait` suffix), the adapter
`pub fn new(...) -> impl <Domain>` factory shape, the rule that the port —
not the app — owns the lifecycle policy, and the `test-util` Cargo feature
that gates per-port test fakes.

Layout:

- `src/lib.rs` — convention doc + one-line index (`pub mod <domain>;`)
- `src/<domain>.rs` — one file per port. Open the module file to see the
  contract.

Project-wide rules: [`../AGENTS.md`](../AGENTS.md).
