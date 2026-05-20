//! domain-ports: the trait contracts (ports) that adapters implement.
//!
//! Each domain lives in its own module. lib.rs is just the index —
//! open the module file to see the contract.
//!
//! # Port convention
//!
//! Every port follows the same outer shape. The shape of the *trait*
//! inside is the port author's call — singleton resource, factory of
//! sub-instances, plain reader, whatever fits — but the boot-time
//! ceremony is uniform.
//!
//! 1. **One app-facing trait per port module, named after the
//!    domain.** No `Trait` suffix, no `Root` prefix. The trait is the
//!    contract apps depend on. Beyond that, the port may expose
//!    additional adapter-facing types (a `<Domain>Core` helper
//!    struct, error types, etc.) as the domain warrants — these are
//!    contracts between the port and its adapters, not part of the
//!    app-facing surface. Simple ports (Clock) won't need them; rich
//!    ports often will. See `domain-ports/AGENTS.md` for the
//!    `<Domain>Core` pattern.
//!
//! 2. **The matching adapter crate exposes one factory:**
//!    `pub fn new(...) -> impl <Domain>`.
//!    Apps call it **once** at boot in real usage. (Tests may call it
//!    freely, or substitute a fake `impl <Domain>`.) The adapter's
//!    concrete type stays private — callers only see `impl <Domain>`.
//!
//! 3. **The port — not the app — decides the lifecycle policy.** If
//!    a domain should only have one live instance, the port's trait
//!    encodes that (e.g. an `acquire()` that returns an error if one
//!    is already out). The adapter enforces; the app consumes.
//!
//! 4. **Test fakes ship behind the `test-util` Cargo feature.** A
//!    port may include a deterministic fake for consumer tests
//!    (e.g. an in-memory implementation of the trait). Gate it with
//!    `#[cfg(any(test, feature = "test-util"))]` so the fake is
//!    visible to this crate's own tests and to downstream crates that
//!    opt in via dev-dependencies, but does **not** ship in
//!    production builds (default features, non-test). Downstream
//!    `Cargo.toml`:
//!
//!    ```toml
//!    [dev-dependencies]
//!    domain-ports = { path = "...", features = ["test-util"] }
//!    ```
//!
//! # Shape sketch
//!
//! ```ignore
//! // domain-ports/src/foo.rs
//! pub trait Foo: Send + Sync {
//!     // ...whatever the Foo domain needs...
//! }
//!
//! // domain-adapters/foo/src/lib.rs  (crate name: adapter-foo)
//! pub fn new() -> impl Foo { /* private concrete impl */ }
//!
//! // apps/<host>/src/main.rs
//! let foo = adapter_foo::new();   // once, at boot
//! ```
//!
//! Domains that need richer shapes (sub-instances, lifecycle gates,
//! state machines) encode that *inside the trait*. The outer shape —
//! one trait, one adapter `new()`, called once at boot — stays the
//! same.

pub mod app_coach;
pub mod clock;
pub mod telemetry;
