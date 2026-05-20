//! domain-ports: the trait contracts (ports) that adapters implement.
//!
//! Each domain (Clock, AudioInput, DeviceCatalog, ...) lives in its
//! own module. lib.rs is just the index — open the module file to see
//! the contract.

pub mod clock;
