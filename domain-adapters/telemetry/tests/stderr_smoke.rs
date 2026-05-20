//! Real-stderr smoke. Proves `new()` returns a working Telemetry and
//! that a log call goes through without panicking. The detailed format
//! and child semantics are covered by the unit tests in `src/lib.rs`
//! against an in-memory parallel impl.

use domain_ports::fields;
use domain_ports::telemetry::{Fields, Level, Telemetry};

#[test]
fn new_logs_to_stderr_without_panicking() {
    let tel = domain_adapter_telemetry::new();
    tel.log(Level::Info, "smoke", &Fields::new());
    tel.log(Level::Info, "smoke-with-fields", &fields! { k = 1u32 });
    let child = tel.child(fields! { scope = "boot" });
    child.log(Level::Warn, "child-line", &Fields::new());
}
