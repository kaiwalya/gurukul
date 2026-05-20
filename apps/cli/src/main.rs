//! gurukul-cli entry point.
//!
//! The app's only job here is wiring: call each adapter's `new()` to
//! get an `impl Clock` / `impl AudioInput` / ... and hand it to the
//! domain code. As more ports land, this stays a short list of
//! `let x = domain_adapter_x::new();` followed by the domain entry.

use domain_ports::clock::Clock;

fn greet(clock: &dyn Clock) -> String {
    format!("gurukul-cli: t={}ms", clock.now_ms())
}

fn main() {
    let clock = domain_adapter_clock::new();
    println!("{}", greet(&clock));
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::clock::TestClock;

    #[test]
    fn greet_uses_clock() {
        let c = TestClock::new(42_000_000); // 42 ms in ns
        assert_eq!(greet(&c), "gurukul-cli: t=42ms");
    }
}
