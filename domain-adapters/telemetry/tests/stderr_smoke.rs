//! Real-stderr smoke. Proves `new()` returns a working Telemetry and
//! that a log call goes through without panicking. The detailed format
//! and child semantics are covered by the unit tests in `src/lib.rs`
//! against an in-memory parallel impl.

use domain_ports::clock::Clock;
use domain_ports::fields;
use domain_ports::telemetry::{Event, Fields, Level, Telemetry};
use std::sync::Arc;

#[test]
fn new_logs_to_stderr_without_panicking() {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock::new());
    let tel = adapter_telemetry::new(clock);
    tel.log(Level::Info, "smoke", &Fields::new());
    tel.log(Level::Info, "smoke-with-fields", &fields! { k = 1u32 });
    let child = tel.child(fields! { scope = "boot" });
    child.log(Level::Warn, "child-line", &Fields::new());
    tel.event(&Event::Boot {
        app_version: env!("CARGO_PKG_VERSION"),
    });
    tel.event(&Event::Shutdown { uptime_ms: 0 });
}
