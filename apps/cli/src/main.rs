//! gurukul-cli entry point.
//!
//! The app's only job here is wiring: call each adapter's `new()` to
//! get an `impl Clock` / `impl Telemetry` / ... and hand them to the
//! domain code. As more ports land, this stays a short list of
//! `let x = domain_adapter_x::new();` followed by the domain entry.

use domain_ports::clock::Clock;
use domain_ports::tel_info;
use domain_ports::telemetry::Telemetry;

fn greet(clock: &dyn Clock, tel: &dyn Telemetry) {
    tel_info!(tel, "gurukul-cli: hello", t_ms = clock.now_ms());
}

fn main() {
    let clock = domain_adapter_clock::new();
    let tel = domain_adapter_telemetry::new();
    greet(&clock, &tel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::clock::TestClock;
    use domain_ports::telemetry::{Level, TestTelemetry, Value};

    #[test]
    fn greet_logs_info_with_t_ms_from_clock() {
        let c = TestClock::new(42_000_000); // 42 ms in ns
        let t = TestTelemetry::new();
        greet(&c, &t);
        let cap = t.captured();
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].level, Level::Info);
        assert_eq!(cap[0].msg, "gurukul-cli: hello");
        assert_eq!(cap[0].fields.get("t_ms"), Some(&Value::U64(42)));
    }
}
