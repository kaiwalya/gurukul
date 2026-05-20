//! coach-cli entry point.
//!
//! The host's job is wiring: call each peripheral adapter's `new()`
//! to get an `impl <Port>`, build [`AppCoachDeps`], hand it to the
//! coach. The coach behaviour lives in `adapter-app-coach`.

use domain_ports::app_coach::{AppCoach, AppCoachDeps};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::sync::Arc;

fn main() {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));
    let coach = adapter_app_coach::new();
    coach.main(AppCoachDeps {
        clock,
        telemetry,
        host_version: env!("CARGO_PKG_VERSION"),
    });
}
