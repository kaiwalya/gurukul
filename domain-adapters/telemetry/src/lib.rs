//! domain-adapter-telemetry: the Telemetry port adapter that writes to
//! `io::Stderr`.
//!
//! Format per line:
//!
//! - logs: `[LEVEL] msg {k=v, k2=v2}` — keys sorted by `Fields`'
//!   `BTreeMap` order, the trailing `{}` omitted when there are no
//!   fields.
//! - events: `[EVENT] {event="<name>", t_ms=N, ...}` — the variant
//!   name and timestamp are stamped into the fields bag by
//!   `TelemetryCore::stamp`; the adapter just renders the bag.
//!
//! Greppable, no JSON dep.
//!
//! Apps call [`new`] with an `Arc<dyn Clock>` to get an `impl Telemetry`.
//! The clock supplies `t_ms` for events. Children share the parent's
//! `Stderr` handle through an `Arc<Mutex<_>>`, so interleaved writes
//! from multiple threads stay line-atomic.

use domain_ports::clock::Clock;
use domain_ports::telemetry::{Event, Fields, Level, Telemetry, TelemetryCore};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Build a stderr-backed Telemetry. Cheap — does not touch the stderr
/// handle until the first log call. The `clock` is used to stamp
/// `t_ms` on every event.
///
/// Apps call this once at boot and pass the returned `impl Telemetry`
/// (typically wrapped in `Arc<dyn Telemetry>`) to whatever subsystems
/// need to log.
pub fn new(clock: Arc<dyn Clock>) -> impl Telemetry {
    StderrTelemetry {
        core: TelemetryCore::new(clock),
        out: Arc::new(Mutex::new(io::stderr())),
    }
}

/// Build a stderr-backed Telemetry with an initial context bag whose
/// fields appear on every log line. Events do not pick up context;
/// see [`domain_ports::telemetry::Event`].
pub fn with_context(clock: Arc<dyn Clock>, context: Fields) -> impl Telemetry {
    StderrTelemetry {
        core: TelemetryCore::with_context(clock, context),
        out: Arc::new(Mutex::new(io::stderr())),
    }
}

struct StderrTelemetry {
    core: TelemetryCore,
    // Arc so children share the same handle; Mutex so concurrent
    // writes from multiple threads serialize at the line boundary.
    out: Arc<Mutex<io::Stderr>>,
}

impl Telemetry for StderrTelemetry {
    fn log(&self, level: Level, msg: &str, fields: &Fields) {
        let merged = self.core.merge(fields);
        let mut out = self.out.lock().unwrap();
        let _ = if merged.is_empty() {
            writeln!(out, "[{level}] {msg}")
        } else {
            writeln!(out, "[{level}] {msg} {merged}")
        };
    }

    fn child(&self, fields: Fields) -> Arc<dyn Telemetry> {
        Arc::new(StderrTelemetry {
            core: self.core.child(fields),
            out: Arc::clone(&self.out),
        })
    }

    fn event(&self, e: &Event) {
        let stamped = self.core.stamp(e);
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "[EVENT] {stamped}");
    }
}

#[cfg(test)]
mod tests {
    //! The adapter's I/O target is `io::Stderr` which we can't easily
    //! capture in-process without going through OS file descriptors —
    //! out of scope for a unit test. Instead we exercise the merge +
    //! child wiring against a tiny `Write` substitute, sharing the
    //! adapter's `impl Telemetry` body.
    //!
    //! End-to-end "did the line actually hit stderr" is covered by the
    //! integration test in `tests/stderr_smoke.rs`.

    use domain_ports::clock::{Clock, TestClock};
    use domain_ports::fields;
    use domain_ports::telemetry::{Event, Fields, Level, Telemetry, TelemetryCore};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    struct BufTelemetry {
        core: TelemetryCore,
        out: Arc<Mutex<Vec<u8>>>,
    }

    impl Telemetry for BufTelemetry {
        fn log(&self, level: Level, msg: &str, fields: &Fields) {
            let merged = self.core.merge(fields);
            let mut out = self.out.lock().unwrap();
            let _ = if merged.is_empty() {
                writeln!(out, "[{level}] {msg}")
            } else {
                writeln!(out, "[{level}] {msg} {merged}")
            };
        }

        fn child(&self, fields: Fields) -> Arc<dyn Telemetry> {
            Arc::new(BufTelemetry {
                core: self.core.child(fields),
                out: Arc::clone(&self.out),
            })
        }

        fn event(&self, e: &Event) {
            let stamped = self.core.stamp(e);
            let mut out = self.out.lock().unwrap();
            let _ = writeln!(out, "[EVENT] {stamped}");
        }
    }

    fn buf_at(start_ns: u64) -> (BufTelemetry, Arc<Mutex<Vec<u8>>>) {
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(start_ns));
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        (
            BufTelemetry {
                core: TelemetryCore::new(clock),
                out: Arc::clone(&out),
            },
            out,
        )
    }

    fn buf() -> (BufTelemetry, Arc<Mutex<Vec<u8>>>) {
        buf_at(0)
    }

    fn dump(out: &Mutex<Vec<u8>>) -> String {
        String::from_utf8(out.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn formats_with_no_fields() {
        let (tel, out) = buf();
        tel.log(Level::Info, "hello", &Fields::new());
        assert_eq!(dump(&out), "[INFO] hello\n");
    }

    #[test]
    fn formats_with_sorted_fields() {
        let (tel, out) = buf();
        tel.log(Level::Warn, "x", &fields! { b = 2u32, a = "v" });
        // BTreeMap → keys come out alphabetical.
        assert_eq!(dump(&out), "[WARN] x {a=\"v\", b=2}\n");
    }

    #[test]
    fn child_inherits_context_and_shares_sink() {
        let (parent, out) = buf();
        let child = parent.child(fields! { scope = "boot" });
        parent.log(Level::Info, "p", &Fields::new());
        child.log(Level::Info, "c", &fields! { k = 1u32 });
        let s = dump(&out);
        // Both lines hit the same buffer; child's context appears on its line.
        assert_eq!(s, "[INFO] p\n[INFO] c {k=1, scope=\"boot\"}\n");
    }

    #[test]
    fn level_names_match_port() {
        let (tel, out) = buf();
        tel.log(Level::Trace, "t", &Fields::new());
        tel.log(Level::Debug, "d", &Fields::new());
        tel.log(Level::Info, "i", &Fields::new());
        tel.log(Level::Warn, "w", &Fields::new());
        tel.log(Level::Error, "e", &Fields::new());
        assert_eq!(
            dump(&out),
            "[TRACE] t\n[DEBUG] d\n[INFO] i\n[WARN] w\n[ERROR] e\n"
        );
    }

    #[test]
    fn event_renders_stamped_fields() {
        // 42 ms in ns.
        let (tel, out) = buf_at(42_000_000);
        tel.event(&Event::Boot {
            app_version: "0.1.0",
        });
        tel.event(&Event::Shutdown { uptime_ms: 1458 });
        assert_eq!(
            dump(&out),
            "[EVENT] {app_version=\"0.1.0\", event=\"boot\", t_ms=42}\n\
             [EVENT] {event=\"shutdown\", t_ms=42, uptime_ms=1458}\n"
        );
    }
}
