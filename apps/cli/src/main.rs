//! gurukul-cli entry point.
//!
//! The app's only job here is wiring: construct adapter impls and
//! hand them to domain code. As more ports (audio, devices, ...) land,
//! this stays a short list of `let x = adapter_x::...;` followed by
//! the domain entry call.

use domain_adapter_clock::SystemClock;

fn main() {
    let clock = SystemClock::new();
    println!("{}", domain_ports::greet(&clock));
}
