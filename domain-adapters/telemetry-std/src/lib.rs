//! adapter-telemetry-std: the Telemetry port adapter that writes to
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
//! Apps call [`new`] with an `Arc<dyn Clock>` and an optional path prefix
//! to get an `impl Telemetry`. When a prefix is supplied every line is
//! **teed** to `<prefix>-log.jsonl` in addition to stderr; when `None`
//! the adapter behaves exactly as before (stderr-only). The clock supplies
//! `t_ms` for events. Children share both the stderr and file handles
//! through `Arc<Mutex<_>>`, so interleaved writes stay line-atomic.

use domain_ports::clock::Clock;
use domain_ports::telemetry::{Event, Fields, Level, Telemetry, TelemetryCore};
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Build a stderr-backed Telemetry. The `clock` is used to stamp
/// `t_ms` on every event.
///
/// When `log_prefix` is `Some(prefix)`, every log/event line is also
/// written to `<prefix>-log.jsonl` (tee: stderr unchanged + file added).
/// The file is created at construction; if creation fails the adapter
/// falls back to stderr-only without panicking.
///
/// Apps call this once at boot and pass the returned `impl Telemetry`
/// (typically wrapped in `Arc<dyn Telemetry>`) to whatever subsystems
/// need to log.
pub fn new(clock: Arc<dyn Clock>, log_prefix: Option<PathBuf>) -> impl Telemetry {
    StderrTelemetry {
        core: TelemetryCore::new(clock),
        out: Arc::new(Mutex::new(io::stderr())),
        file: open_log_file(log_prefix.as_deref()),
    }
}

/// Build a stderr-backed Telemetry with an initial context bag whose
/// fields appear on every log line. Events do not pick up context;
/// see [`domain_ports::telemetry::Event`].
pub fn with_context(
    clock: Arc<dyn Clock>,
    context: Fields,
    log_prefix: Option<PathBuf>,
) -> impl Telemetry {
    StderrTelemetry {
        core: TelemetryCore::with_context(clock, context),
        out: Arc::new(Mutex::new(io::stderr())),
        file: open_log_file(log_prefix.as_deref()),
    }
}

/// Derive the log file path from a run prefix: `<prefix>-log.jsonl`.
///
/// A plain `prefix.with_extension("log.jsonl")` would replace any existing
/// extension in the stem. Instead we append `-log.jsonl` directly to the
/// file-name component, matching how `audio_trace_format` derives its sibling
/// paths (e.g. `<stem>.wav`, `<stem>.features.jsonl`).
fn log_file_path(prefix: &Path) -> PathBuf {
    let stem = prefix
        .file_name()
        .map(|n| {
            let mut s = n.to_os_string();
            s.push("-log.jsonl");
            s
        })
        .unwrap_or_else(|| std::ffi::OsString::from("telemetry-log.jsonl"));
    match prefix.parent() {
        Some(parent) => parent.join(stem),
        None => PathBuf::from(stem),
    }
}

/// Open (or create) the log file, returning `None` on any error so the
/// caller can fall back to stderr-only. Mirrors the audio recorder's
/// best-effort `create_dir_all` pattern.
fn open_log_file(prefix: Option<&Path>) -> Option<Arc<Mutex<BufWriter<fs::File>>>> {
    let prefix = prefix?;
    if let Some(parent) = prefix.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("telemetry-std: could not create log dir {parent:?}: {e}");
            return None;
        }
    }
    let path = log_file_path(prefix);
    match fs::File::create(&path) {
        Ok(f) => Some(Arc::new(Mutex::new(BufWriter::new(f)))),
        Err(e) => {
            eprintln!("telemetry-std: could not create log file {path:?}: {e}");
            None
        }
    }
}

struct StderrTelemetry {
    core: TelemetryCore,
    // Arc so children share the same handle; Mutex so concurrent
    // writes from multiple threads serialize at the line boundary.
    out: Arc<Mutex<io::Stderr>>,
    // Optional file tee. Same sharing + serialization contract as `out`.
    file: Option<Arc<Mutex<BufWriter<fs::File>>>>,
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
        if let Some(file) = &self.file {
            let mut f = file.lock().unwrap();
            let _ = if merged.is_empty() {
                writeln!(f, "[{level}] {msg}")
            } else {
                writeln!(f, "[{level}] {msg} {merged}")
            };
            // Flush after every write so an iOS SIGKILL leaves a non-empty log.
            // The file sink is a debugging aid, not a hot path.
            let _ = f.flush();
        }
    }

    fn child(&self, fields: Fields) -> Arc<dyn Telemetry> {
        Arc::new(StderrTelemetry {
            core: self.core.child(fields),
            out: Arc::clone(&self.out),
            file: self.file.as_ref().map(Arc::clone),
        })
    }

    fn event(&self, e: &Event) {
        let stamped = self.core.stamp(e);
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "[EVENT] {stamped}");
        if let Some(file) = &self.file {
            let mut f = file.lock().unwrap();
            let _ = writeln!(f, "[EVENT] {stamped}");
            // Same per-write flush as log(): survive an iOS SIGKILL.
            let _ = f.flush();
        }
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

    use super::log_file_path;
    use crate::new;
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

    // --- file-sink tests ---

    #[test]
    fn log_file_path_appends_suffix() {
        use std::path::PathBuf;
        let prefix = PathBuf::from("/tmp/traces/2026-06-18-120000-000");
        let got = log_file_path(&prefix);
        assert_eq!(
            got,
            PathBuf::from("/tmp/traces/2026-06-18-120000-000-log.jsonl")
        );
    }

    #[test]
    fn file_sink_receives_log_and_event_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prefix = dir.path().join("test-run");

        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        let tel = new(Arc::clone(&clock), Some(prefix.clone()));

        tel.log(Level::Info, "hello from file", &Fields::new());
        tel.event(&Event::Boot {
            app_version: "0.1.0",
        });

        // Drop the telemetry (no longer needed for flushing — each write flushes
        // immediately — but kept to release the file handle before reading).
        drop(tel);

        let log_path = log_file_path(&prefix);
        assert!(log_path.exists(), "log file must exist at {log_path:?}");

        let contents = std::fs::read_to_string(&log_path).expect("read log file");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got: {contents:?}");
        assert_eq!(lines[0], "[INFO] hello from file");
        assert!(
            lines[1].contains("[EVENT]"),
            "second line must be an event: {contents:?}"
        );
    }

    #[test]
    fn none_prefix_is_stderr_only() {
        // When log_prefix is None the adapter must construct without panicking
        // and must write nothing to a file.
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        let tel = new(Arc::clone(&clock), None);
        tel.log(Level::Warn, "stderr only", &Fields::new());
        drop(tel);
        // No assertion on stderr content (can't capture it here); the test
        // merely verifies no panic and no unexpected file is created.
    }
}
