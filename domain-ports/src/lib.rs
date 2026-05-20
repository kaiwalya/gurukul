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
//! 1. **One trait per port module, named after the domain.** No
//!    `Trait` suffix, no `Root` prefix. The trait *is* the domain's
//!    full contract: instance methods, factory methods for sub-types
//!    it hands out, lifecycle operations, error types. Everything the
//!    domain offers hangs off this trait or off types its methods
//!    return.
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
//! # Shape sketch
//!
//! ```ignore
//! // domain-ports/src/foo.rs
//! pub trait Foo: Send + Sync {
//!     // ...whatever the Foo domain needs...
//! }
//!
//! // domain-adapters/foo/src/lib.rs
//! pub fn new() -> impl Foo { /* private concrete impl */ }
//!
//! // apps/<host>/src/main.rs
//! let foo = domain_adapter_foo::new();   // once, at boot
//! ```
//!
//! Domains that need richer shapes (sub-instances, lifecycle gates,
//! state machines) encode that *inside the trait*. The outer shape —
//! one trait, one adapter `new()`, called once at boot — stays the
//! same.

pub mod clock;
