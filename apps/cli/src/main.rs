//! gurukul-cli entry point.
//!
//! The app's only job here is wiring: call each adapter's `new()` to
//! get an `impl Clock` / `impl Telemetry` / ... and hand them to the
//! domain code. As more ports land, this stays a short list of
//! `let x = domain_adapter_x::new();` followed by the domain entry.

use domain_ports::clock::Clock;
use domain_ports::tel_info;
use domain_ports::telemetry::{Event, Telemetry};
use std::sync::Arc;

fn greet(clock: &dyn Clock, tel: &dyn Telemetry) {
    tel_info!(tel, "gurukul-cli: hello", t_ms = clock.now_ms());
}

fn main() {
    let clock: Arc<dyn Clock> = Arc::new(domain_adapter_clock::new());
    let tel = domain_adapter_telemetry::new(Arc::clone(&clock));

    tel.event(&Event::Boot {
        app_version: env!("CARGO_PKG_VERSION"),
    });
    let boot_ms = clock.now_ms();

    greet(&*clock, &tel);

    tel.event(&Event::Shutdown {
        uptime_ms: clock.now_ms().saturating_sub(boot_ms),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::clock::TestClock;
    use domain_ports::telemetry::{Level, TestTelemetry, Value};

    #[test]
    fn greet_logs_info_with_t_ms_from_clock() {
        let c = TestClock::new(42_000_000); // 42 ms in ns
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        let t = TestTelemetry::new(clock);
        greet(&c, &t);
        let cap = t.logs();
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].level, Level::Info);
        assert_eq!(cap[0].msg, "gurukul-cli: hello");
        assert_eq!(cap[0].fields.get("t_ms"), Some(&Value::U64(42)));
    }
}
