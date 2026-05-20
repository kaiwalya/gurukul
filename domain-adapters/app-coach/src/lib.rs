//! adapter-app-coach: the canonical [`AppCoach`] implementation.
//!
//! Hosts wire peripheral adapters (clock, telemetry, ...) into an
//! [`AppCoachDeps`] and call [`AppCoach::main`]. The behaviour is
//! intentionally trivial today (Boot event, greeting log, Shutdown
//! event with uptime) — this crate is the seam where future coaching
//! logic (mic capture, pitch tracking, session state) will land.
//!
//! Why is this an adapter and not the trait's default? The port
//! defines the *contract*; alternative implementations may exist for
//! testing or specialised hosts. Keeping the canonical impl in a
//! crate lets hosts depend on a name (`adapter-app-coach`) rather
//! than a struct from inside `domain-ports`.

use domain_ports::app_coach::{AppCoach, AppCoachDeps};
use domain_ports::tel_info;
use domain_ports::telemetry::Event;

/// Build the canonical [`AppCoach`]. Cheap — no allocation, no I/O.
/// Hosts call this once and invoke `.main(deps)`.
pub fn new() -> impl AppCoach {
    CoachApp
}

struct CoachApp;

impl AppCoach for CoachApp {
    fn main(&self, deps: AppCoachDeps) {
        deps.telemetry.event(&Event::Boot {
            app_version: deps.host_version,
        });
        let boot_ms = deps.clock.now_ms();

        tel_info!(
            &*deps.telemetry,
            "gurukul: hello",
            t_ms = deps.clock.now_ms()
        );

        deps.telemetry.event(&Event::Shutdown {
            uptime_ms: deps.clock.now_ms().saturating_sub(boot_ms),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::clock::{Clock, TestClock};
    use domain_ports::telemetry::{Level, Telemetry, TestTelemetry, Value};
    use std::sync::Arc;

    #[test]
    fn main_emits_boot_then_log_then_shutdown() {
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(42_000_000)); // 42 ms
        let tel = Arc::new(TestTelemetry::new(Arc::clone(&clock)));
        let coach = new();

        coach.main(AppCoachDeps {
            clock: Arc::clone(&clock),
            telemetry: tel.clone() as Arc<dyn Telemetry>,
            host_version: "1.2.3",
        });

        let logs = tel.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, Level::Info);
        assert_eq!(logs[0].msg, "gurukul: hello");
        assert_eq!(logs[0].fields.get("t_ms"), Some(&Value::U64(42)));

        let events = tel.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "boot");
        assert_eq!(
            events[0].fields.get("app_version"),
            Some(&Value::Str("1.2.3".into()))
        );
        assert_eq!(events[1].name, "shutdown");
        // Same TestClock for boot+shutdown stamps → uptime is 0.
        assert_eq!(events[1].fields.get("uptime_ms"), Some(&Value::U64(0)));
    }
}
