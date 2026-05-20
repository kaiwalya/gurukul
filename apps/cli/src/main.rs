//! gurukul-cli entry point.
//!
//! The app's only job here is wiring: construct platform impls and
//! hand them to app-core. As more components (audio, devices, ...)
//! land, this stays a short list of `let x = PlatformX::new();`
//! followed by an `app_core::run(...)` call.

use platform_cli::SystemClock;

fn main() {
    let clock = SystemClock::new();
    println!("{}", app_core::greet(&clock));
}
