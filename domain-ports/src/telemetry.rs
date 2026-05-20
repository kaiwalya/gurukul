//! Telemetry port: structured logging.
//!
//! The port provides a *cross-platform structured-log interface* that
//! adapters bridge to whatever the host's observability stack is
//! (stderr, `os_log`, a remote endpoint, a buffered file, etc.).
//!
//! # Surface
//!
//! - [`Telemetry`] — the app-facing trait. Apps log through this.
//! - [`TelemetryCore`] — adapter-side helper holding the shared logic
//!   (currently: a context bag and the field-merge rule). See
//!   `domain-ports/AGENTS.md` for the `<Domain>Core` pattern.
//! - [`Fields`], [`Value`], [`Level`] — the data types that flow
//!   through the trait.
//! - [`tel_trace!`], [`tel_debug!`], [`tel_info!`], [`tel_warn!`],
//!   [`tel_error!`] — call-site macros for tracing-style ergonomics.
//!
//! # Field semantics
//!
//! Fields are a **flat, deduped key→value bag**. Backed by
//! `BTreeMap`: deterministic ordering for stable output, last-write
//! wins on duplicate keys (caller fields override adapter context).
//!
//! Nested structure is intentionally not supported. If callers need
//! it, they flatten with dotted keys (`device.name`, `device.rate`).
//! This keeps the wire format greppable and aligns with what every
//! production logger / metrics backend actually does.
//!
//! # Levels
//!
//! [`Level::Trace`], [`Debug`], [`Info`], [`Warn`], [`Error`] — same
//! five levels as `log`, `tracing`, and `slog`. Adapters may filter.
//!
//! # No errors on log calls
//!
//! `log()` returns `()`. Telemetry must not fail loudly: an adapter
//! that can't write swallows the error and reports the failure
//! through its own backchannel (counter, stderr, etc.). Logging
//! should never crash production code.

use core::fmt;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------

/// Severity of a log line. Adapters may filter on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        })
    }
}

/// A single field value. JSON-renderable subset chosen deliberately —
/// no `Object`, no `Array`. Nested data flattens to dotted keys at the
/// call site.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{s:?}"),
            Value::I64(n) => write!(f, "{n}"),
            Value::U64(n) => write!(f, "{n}"),
            Value::F64(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Null => f.write_str("null"),
        }
    }
}

// From impls for common Rust types — lets callers write
// `fields.set("rate", 48_000)` without manual `Value::I64(...)`.

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::I64(n)
    }
}
impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::I64(n as i64)
    }
}
impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::U64(n)
    }
}
impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::U64(n as u64)
    }
}
impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Value::U64(n as u64)
    }
}
impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::F64(n)
    }
}
impl From<f32> for Value {
    fn from(n: f32) -> Self {
        Value::F64(n as f64)
    }
}

/// A flat, deduped bag of key→value pairs.
///
/// Built up at the call site (usually via the `tel_*!` macros), then
/// passed by reference to a `Telemetry` method.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fields {
    inner: BTreeMap<String, Value>,
}

impl Fields {
    /// Empty bag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a field. Last write wins on duplicate keys. Returns
    /// `&mut self` for builder chaining.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        self.inner.insert(key.into(), value.into());
        self
    }

    /// True if the bag has no fields.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Iterate (key, value) pairs in sorted key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Look up a value by key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.inner.get(key)
    }

    /// Return a new `Fields` with `other`'s entries layered on top of
    /// self's. Used by [`TelemetryCore`] to merge call-site fields
    /// over the accumulated context.
    pub fn merged_with(&self, other: &Fields) -> Fields {
        let mut out = self.clone();
        for (k, v) in &other.inner {
            out.inner.insert(k.clone(), v.clone());
        }
        out
    }
}

impl fmt::Display for Fields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        let mut first = true;
        for (k, v) in &self.inner {
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "{k}={v}")?;
            first = false;
        }
        f.write_str("}")
    }
}

// ---------------------------------------------------------------------
// App-facing trait
// ---------------------------------------------------------------------

/// Structured logger. Apps hold an `Arc<dyn Telemetry>` and call
/// [`log`](Self::log) (usually via the `tel_*!` macros).
///
/// `Send + Sync` because audio, UI, and worker threads all log.
///
/// Implementations must not panic on log calls and must not block
/// indefinitely. See the module-level docs for error handling.
pub trait Telemetry: Send + Sync {
    /// Emit a log line at `level` with `msg` and the given `fields`.
    /// The adapter is responsible for merging in any context it
    /// carries (typically via [`TelemetryCore::merge`]).
    fn log(&self, level: Level, msg: &str, fields: &Fields);
}

// ---------------------------------------------------------------------
// Adapter-side helper
// ---------------------------------------------------------------------

/// Shared logic adapters can compose into their `impl Telemetry`.
///
/// Today it holds the immutable context bag and the field-merge rule.
/// As the port grows (child loggers, level filters, event
/// serialization), additional shared mechanics live here so they're
/// implemented once across all adapters.
///
/// See `domain-ports/AGENTS.md` for the `<Domain>Core` pattern this
/// participates in.
#[derive(Debug, Clone, Default)]
pub struct TelemetryCore {
    context: Fields,
}

impl TelemetryCore {
    /// New Core with empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// New Core with a starting context. Used by adapter constructors
    /// when they accept caller-supplied initial context.
    pub fn with_context(context: Fields) -> Self {
        Self { context }
    }

    /// Read-only view of the carried context.
    pub fn context(&self) -> &Fields {
        &self.context
    }

    /// Merge the call-site `fields` over the carried context.
    /// Call-site fields override context on key collision (this is
    /// the rule callers expect: more-specific scopes win).
    pub fn merge(&self, fields: &Fields) -> Fields {
        self.context.merged_with(fields)
    }
}

// ---------------------------------------------------------------------
// Call-site macros
// ---------------------------------------------------------------------

/// Build a [`Fields`] from a comma-separated `key = value` list.
///
/// Used internally by the `tel_*!` macros, exposed for callers that
/// want to construct fields outside a log call (e.g. for a
/// `TelemetryCore::with_context` argument).
#[macro_export]
macro_rules! fields {
    ($($k:ident = $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut f = $crate::telemetry::Fields::new();
        $( f.set(stringify!($k), $v); )*
        f
    }};
}

/// `tel_<level>!(tel, "msg", k = v, ...)` — tracing-shape call site
/// that calls `tel.log(Level::<Level>, msg, &fields)`.
#[macro_export]
macro_rules! tel_trace {
    ($tel:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $tel.log(
            $crate::telemetry::Level::Trace,
            $msg,
            &$crate::fields!($($k = $v),*),
        )
    };
}

#[macro_export]
macro_rules! tel_debug {
    ($tel:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $tel.log(
            $crate::telemetry::Level::Debug,
            $msg,
            &$crate::fields!($($k = $v),*),
        )
    };
}

#[macro_export]
macro_rules! tel_info {
    ($tel:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $tel.log(
            $crate::telemetry::Level::Info,
            $msg,
            &$crate::fields!($($k = $v),*),
        )
    };
}

#[macro_export]
macro_rules! tel_warn {
    ($tel:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $tel.log(
            $crate::telemetry::Level::Warn,
            $msg,
            &$crate::fields!($($k = $v),*),
        )
    };
}

#[macro_export]
macro_rules! tel_error {
    ($tel:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $tel.log(
            $crate::telemetry::Level::Error,
            $msg,
            &$crate::fields!($($k = $v),*),
        )
    };
}

// ---------------------------------------------------------------------
// Test fake — gated. See crate-root docs ("test fakes").
// ---------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub use fakes::{Captured, TestTelemetry};

#[cfg(any(test, feature = "test-util"))]
mod fakes {
    use super::{Fields, Level, Telemetry, TelemetryCore};
    use std::sync::Mutex;

    /// A single captured log call. Used by [`TestTelemetry`] for
    /// consumer-side assertions.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Captured {
        pub level: Level,
        pub msg: String,
        pub fields: Fields,
    }

    /// In-memory `Telemetry` that records every log call. Composes
    /// `TelemetryCore` exactly the way a real adapter would — so
    /// tests using `TestTelemetry::with_context(...)` exercise the
    /// same context-merge code path as production adapters.
    pub struct TestTelemetry {
        core: TelemetryCore,
        captured: Mutex<Vec<Captured>>,
    }

    impl TestTelemetry {
        pub fn new() -> Self {
            Self {
                core: TelemetryCore::new(),
                captured: Mutex::new(Vec::new()),
            }
        }

        pub fn with_context(context: Fields) -> Self {
            Self {
                core: TelemetryCore::with_context(context),
                captured: Mutex::new(Vec::new()),
            }
        }

        /// Snapshot of everything logged so far.
        pub fn captured(&self) -> Vec<Captured> {
            self.captured.lock().unwrap().clone()
        }
    }

    impl Default for TestTelemetry {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Telemetry for TestTelemetry {
        fn log(&self, level: Level, msg: &str, fields: &Fields) {
            let merged = self.core.merge(fields);
            self.captured.lock().unwrap().push(Captured {
                level,
                msg: msg.to_owned(),
                fields: merged,
            });
        }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn fields_dedup_and_override() {
        let mut f = Fields::new();
        f.set("k", "a");
        f.set("k", "b");
        assert_eq!(f.len(), 1);
        assert_eq!(f.get("k"), Some(&Value::Str("b".into())));
    }

    #[test]
    fn fields_iter_is_sorted() {
        let mut f = Fields::new();
        f.set("c", 1);
        f.set("a", 2);
        f.set("b", 3);
        let keys: Vec<&str> = f.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_lets_call_site_win_over_context() {
        let core = TelemetryCore::with_context({
            let mut f = Fields::new();
            f.set("device", "ctx-device");
            f.set("rate", 44_100u32);
            f
        });
        let mut call = Fields::new();
        call.set("device", "call-device");
        call.set("take", 1u32);
        let merged = core.merge(&call);
        // Call-site overrides context for `device`...
        assert_eq!(
            merged.get("device"),
            Some(&Value::Str("call-device".into()))
        );
        // ...and the call-only field comes through...
        assert_eq!(merged.get("take"), Some(&Value::U64(1)));
        // ...and the context-only field survives.
        assert_eq!(merged.get("rate"), Some(&Value::U64(44_100)));
    }

    #[test]
    fn value_from_impls_cover_common_numerics() {
        let mut f = Fields::new();
        f.set("a", "hello");
        f.set("b", 42i32);
        f.set("c", 42u32);
        f.set("d", 1.5f64);
        f.set("e", true);
        assert_eq!(f.get("a"), Some(&Value::Str("hello".into())));
        assert_eq!(f.get("b"), Some(&Value::I64(42)));
        assert_eq!(f.get("c"), Some(&Value::U64(42)));
        assert_eq!(f.get("d"), Some(&Value::F64(1.5)));
        assert_eq!(f.get("e"), Some(&Value::Bool(true)));
    }

    #[test]
    fn fields_macro_builds_a_bag() {
        let f = fields! { name = "Mic", rate = 48_000u32 };
        assert_eq!(f.len(), 2);
        assert_eq!(f.get("name"), Some(&Value::Str("Mic".into())));
        assert_eq!(f.get("rate"), Some(&Value::U64(48_000)));
    }

    #[test]
    fn tel_macros_call_log_with_right_level() {
        let tel = Arc::new(TestTelemetry::new());
        tel_trace!(tel, "t");
        tel_debug!(tel, "d");
        tel_info!(tel, "i", k = 1u32);
        tel_warn!(tel, "w");
        tel_error!(tel, "e");
        let cap = tel.captured();
        assert_eq!(cap.len(), 5);
        assert_eq!(cap[0].level, Level::Trace);
        assert_eq!(cap[1].level, Level::Debug);
        assert_eq!(cap[2].level, Level::Info);
        assert_eq!(cap[3].level, Level::Warn);
        assert_eq!(cap[4].level, Level::Error);
        // Field came through on the info call:
        assert_eq!(cap[2].fields.get("k"), Some(&Value::U64(1)));
    }

    #[test]
    fn test_telemetry_concrete_captures_merged_fields() {
        let tel = TestTelemetry::with_context(fields! {
            device = "MacBook Mic",
            rate = 48_000u32,
        });
        tel_info!(&tel, "starting capture", take = 7u32);
        let cap = tel.captured();
        assert_eq!(cap.len(), 1);
        let c = &cap[0];
        assert_eq!(c.level, Level::Info);
        assert_eq!(c.msg, "starting capture");
        assert_eq!(
            c.fields.get("device"),
            Some(&Value::Str("MacBook Mic".into()))
        );
        assert_eq!(c.fields.get("rate"), Some(&Value::U64(48_000)));
        assert_eq!(c.fields.get("take"), Some(&Value::U64(7)));
    }

    #[test]
    fn dyn_telemetry_dispatch() {
        let tel: Arc<dyn Telemetry> = Arc::new(TestTelemetry::new());
        tel_info!(tel, "hello", k = "v");
        // Down-cast not required — we don't need to read captured here,
        // just prove the vtable dispatch compiles and runs.
        let _ = tel;
    }
}
