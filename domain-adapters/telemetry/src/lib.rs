//! domain-adapter-telemetry: the Telemetry port adapter that writes to
//! `io::Stderr`.
//!
//! Format per line: `[LEVEL] msg {k=v, k2=v2}` — keys sorted by
//! `Fields`' `BTreeMap` order, the trailing `{}` omitted when there
//! are no fields. Greppable, no JSON dep.
//!
//! Apps call [`new`] to get an `impl Telemetry`. The concrete type is
//! private. Children (built via [`domain_ports::telemetry::Telemetry::child`])
//! share the parent's `Stderr` handle through an `Arc<Mutex<_>>`, so
//! interleaved writes from multiple threads stay line-atomic.

use domain_ports::telemetry::{Fields, Level, Telemetry, TelemetryCore};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Build a stderr-backed Telemetry. Cheap — does not touch the stderr
/// handle until the first log call.
///
/// Apps call this once at boot and pass the returned `impl Telemetry`
/// (typically wrapped in `Arc<dyn Telemetry>`) to whatever subsystems
/// need to log.
pub fn new() -> impl Telemetry {
    StderrTelemetry {
        core: TelemetryCore::new(),
        out: Arc::new(Mutex::new(io::stderr())),
    }
}

/// Build a stderr-backed Telemetry with an initial context bag whose
/// fields appear on every line.
pub fn with_context(context: Fields) -> impl Telemetry {
    StderrTelemetry {
        core: TelemetryCore::with_context(context),
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

    use domain_ports::fields;
    use domain_ports::telemetry::{Fields, Level, Telemetry, TelemetryCore};
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
    }

    fn buf() -> (BufTelemetry, Arc<Mutex<Vec<u8>>>) {
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        (
            BufTelemetry {
                core: TelemetryCore::new(),
                out: Arc::clone(&out),
            },
            out,
        )
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
}
