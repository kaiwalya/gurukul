//! Telemetry port: structured logging.
//!
//! The port provides a *cross-platform structured-log interface* that
//! adapters bridge to whatever the host's observability stack is
//! (stderr, `os_log`, a remote endpoint, a buffered file, etc.).
//!
//! # Surface
//!
//! - [`Telemetry`] — the app-facing trait. Apps log through this, build
//!   scoped child loggers via [`Telemetry::child`], and emit typed
//!   events via [`Telemetry::event`].
//! - [`TelemetryCore`] — adapter-side helper holding the shared logic
//!   (context bag, field-merge rule, child construction, event
//!   stamping). Takes a [`crate::clock::Clock`] at construction so it
//!   can stamp `t_ms` onto every event. See `domain-ports/AGENTS.md`
//!   for the `<Domain>Core` pattern.
//! - [`Event`] — closed-set, schema-locked catalog of events the app
//!   emits. Distinct from free-form logs.
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

use crate::clock::Clock;
use core::fmt;
use std::collections::BTreeMap;
use std::sync::Arc;

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
// Typed events
// ---------------------------------------------------------------------

/// Closed-set catalog of typed events the app may emit.
///
/// Events are **schema-locked**: each variant lists exactly the fields
/// it carries, no more, no less. They are intended for warehouse /
/// analytics destinations where schema drift is expensive — adding a
/// field, renaming a field, or changing a field's type is a port-level
/// change that goes through one review.
///
/// Contrast with [`Telemetry::log`], which carries a free-form
/// [`Fields`] bag.
///
/// # Common fields
///
/// Every adapter stamps two fields onto every event before emission
/// via [`TelemetryCore::stamp`]:
///
/// - `event` — the variant name as a string (`"boot"`, `"shutdown"`).
/// - `t_ms` — milliseconds since the telemetry instance's clock epoch.
///
/// These are not part of the variant fields because they are
/// adapter-supplied, not caller-supplied. Variants list only the
/// event-specific schema.
///
/// # Context is *not* merged into events
///
/// Logger context (set via [`TelemetryCore::with_context`] or
/// [`Telemetry::child`]) does not flow into events. The variant's
/// fields are exactly the columns the warehouse receives. This is
/// deliberate: events represent a stable schema, and context bags are
/// per-scope decoration.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// App has finished initial wiring and is about to start its main
    /// loop. Emitted once per process.
    Boot {
        /// Package version string of the host app.
        app_version: &'static str,
    },
    /// App is about to exit its main loop. Emitted once per process,
    /// best-effort (a panicking process may skip it).
    Shutdown {
        /// Milliseconds from `Boot` to this point.
        uptime_ms: u64,
    },
}

impl Event {
    /// Stable string name. Used as the destination key (table name,
    /// metric name, GA event name). Locked to the variant — changing
    /// these strings breaks downstream consumers.
    pub fn name(&self) -> &'static str {
        match self {
            Event::Boot { .. } => "boot",
            Event::Shutdown { .. } => "shutdown",
        }
    }

    /// The variant-specific fields, *without* the common `event` and
    /// `t_ms` stamps. Adapters should not call this directly — they
    /// call [`TelemetryCore::stamp`] which adds the common fields.
    pub fn fields(&self) -> Fields {
        let mut f = Fields::new();
        match self {
            Event::Boot { app_version } => {
                f.set("app_version", *app_version);
            }
            Event::Shutdown { uptime_ms } => {
                f.set("uptime_ms", *uptime_ms);
            }
        }
        f
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

    /// Return a new logger that inherits this one's context with
    /// `fields` layered on top. Child-supplied keys win over inherited
    /// ones. Children share the same sink as the parent — they are a
    /// *context scope*, not a separate destination.
    ///
    /// Adapters implement this by building the merged context via
    /// [`TelemetryCore::child`] and constructing a new instance of
    /// themselves around it (cloning whatever sink/state is shared).
    /// Any I/O handle the adapter holds must be cheap to share across
    /// children — typically by wrapping it in an `Arc`.
    fn child(&self, fields: Fields) -> Arc<dyn Telemetry>;

    /// Emit a typed [`Event`].
    ///
    /// Adapters stamp the common `event` + `t_ms` fields via
    /// [`TelemetryCore::stamp`] and route the result to whatever
    /// destination the adapter targets (stderr, warehouse, GA, etc.).
    ///
    /// Logger context is **not** merged. See [`Event`].
    fn event(&self, e: &Event);
}

// ---------------------------------------------------------------------
// Adapter-side helper
// ---------------------------------------------------------------------

/// Shared logic adapters can compose into their `impl Telemetry`.
///
/// Holds:
///
/// - the immutable context bag (merged into every log line),
/// - the [`Clock`] used to stamp `t_ms` on events.
///
/// Adapters compose `TelemetryCore` as a field on their concrete type
/// and call its helpers (`merge`, `child`, `stamp`) from their
/// `impl Telemetry`. The clock is supplied by the app at adapter
/// construction — this is the cross-port dependency that ties
/// telemetry to a chosen time source.
///
/// See `domain-ports/AGENTS.md` for the `<Domain>Core` pattern this
/// participates in.
#[derive(Clone)]
pub struct TelemetryCore {
    context: Fields,
    clock: Arc<dyn Clock>,
}

impl TelemetryCore {
    /// New Core with empty context, using `clock` for event
    /// timestamps.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            context: Fields::new(),
            clock,
        }
    }

    /// New Core with a starting context. Used by adapter constructors
    /// when they accept caller-supplied initial context.
    pub fn with_context(clock: Arc<dyn Clock>, context: Fields) -> Self {
        Self { context, clock }
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

    /// Produce a new Core whose context is the parent's context with
    /// `fields` layered on top. Child-supplied keys win over inherited
    /// ones — same precedence rule as [`merge`](Self::merge).
    ///
    /// The child shares the parent's [`Clock`].
    ///
    /// Adapters call this from their `impl Telemetry::child` so the
    /// merge rule lives in one place. The adapter then constructs a
    /// new instance of itself around the returned Core, sharing any
    /// I/O state (sink, mutex, etc.) with the parent.
    pub fn child(&self, fields: Fields) -> TelemetryCore {
        TelemetryCore {
            context: self.context.merged_with(&fields),
            clock: Arc::clone(&self.clock),
        }
    }

    /// Render an [`Event`] to its on-the-wire fields, stamping the
    /// adapter-supplied common fields (`event` name and `t_ms`)
    /// alongside the variant-specific fields.
    ///
    /// Logger context is intentionally not merged — see [`Event`].
    pub fn stamp(&self, e: &Event) -> Fields {
        let mut f = e.fields();
        f.set("event", e.name());
        f.set("t_ms", self.clock.now_ms());
        f
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
pub use fakes::{CapturedEvent, CapturedLog, TestTelemetry};

#[cfg(any(test, feature = "test-util"))]
mod fakes {
    use super::{Clock, Event, Fields, Level, Telemetry, TelemetryCore};
    use std::sync::{Arc, Mutex};

    /// A single captured log call. Used by [`TestTelemetry`] for
    /// consumer-side assertions on [`Telemetry::log`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct CapturedLog {
        pub level: Level,
        pub msg: String,
        pub fields: Fields,
    }

    /// A single captured event emission. Captures the rendered fields
    /// (post-stamp), which is what an adapter would actually send
    /// downstream.
    #[derive(Debug, Clone, PartialEq)]
    pub struct CapturedEvent {
        pub name: &'static str,
        pub fields: Fields,
    }

    struct Capture {
        logs: Vec<CapturedLog>,
        events: Vec<CapturedEvent>,
    }

    /// In-memory `Telemetry` that records every log + event call.
    /// Composes `TelemetryCore` exactly the way a real adapter would
    /// — so consumer tests exercise the same merge / stamp code path
    /// as production adapters.
    ///
    /// Children created via [`Telemetry::child`] share the same
    /// capture buffer with their parent: a single snapshot reads
    /// every entry produced by the parent or any descendant.
    pub struct TestTelemetry {
        core: TelemetryCore,
        capture: Arc<Mutex<Capture>>,
    }

    impl TestTelemetry {
        /// New test telemetry using the given clock for event stamps.
        pub fn new(clock: Arc<dyn Clock>) -> Self {
            Self {
                core: TelemetryCore::new(clock),
                capture: Arc::new(Mutex::new(Capture {
                    logs: Vec::new(),
                    events: Vec::new(),
                })),
            }
        }

        pub fn with_context(clock: Arc<dyn Clock>, context: Fields) -> Self {
            Self {
                core: TelemetryCore::with_context(clock, context),
                capture: Arc::new(Mutex::new(Capture {
                    logs: Vec::new(),
                    events: Vec::new(),
                })),
            }
        }

        /// Snapshot of every log call so far, through this logger or
        /// any of its descendants.
        pub fn logs(&self) -> Vec<CapturedLog> {
            self.capture.lock().unwrap().logs.clone()
        }

        /// Snapshot of every event call so far, through this logger
        /// or any of its descendants.
        pub fn events(&self) -> Vec<CapturedEvent> {
            self.capture.lock().unwrap().events.clone()
        }
    }

    impl Telemetry for TestTelemetry {
        fn log(&self, level: Level, msg: &str, fields: &Fields) {
            let merged = self.core.merge(fields);
            self.capture.lock().unwrap().logs.push(CapturedLog {
                level,
                msg: msg.to_owned(),
                fields: merged,
            });
        }

        fn child(&self, fields: Fields) -> Arc<dyn Telemetry> {
            Arc::new(TestTelemetry {
                core: self.core.child(fields),
                capture: Arc::clone(&self.capture),
            })
        }

        fn event(&self, e: &Event) {
            let fields = self.core.stamp(e);
            self.capture.lock().unwrap().events.push(CapturedEvent {
                name: e.name(),
                fields,
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
    use crate::clock::TestClock;

    fn fake_clock() -> Arc<dyn Clock> {
        Arc::new(TestClock::new(0))
    }

    fn fake_clock_at(ns: u64) -> Arc<dyn Clock> {
        Arc::new(TestClock::new(ns))
    }

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
        let core = TelemetryCore::with_context(
            fake_clock(),
            fields! { device = "ctx-device", rate = 44_100u32 },
        );
        let merged = core.merge(&fields! { device = "call-device", take = 1u32 });
        assert_eq!(
            merged.get("device"),
            Some(&Value::Str("call-device".into()))
        );
        assert_eq!(merged.get("take"), Some(&Value::U64(1)));
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
        let tel = Arc::new(TestTelemetry::new(fake_clock()));
        tel_trace!(tel, "t");
        tel_debug!(tel, "d");
        tel_info!(tel, "i", k = 1u32);
        tel_warn!(tel, "w");
        tel_error!(tel, "e");
        let cap = tel.logs();
        assert_eq!(cap.len(), 5);
        assert_eq!(cap[0].level, Level::Trace);
        assert_eq!(cap[1].level, Level::Debug);
        assert_eq!(cap[2].level, Level::Info);
        assert_eq!(cap[3].level, Level::Warn);
        assert_eq!(cap[4].level, Level::Error);
        assert_eq!(cap[2].fields.get("k"), Some(&Value::U64(1)));
    }

    #[test]
    fn test_telemetry_concrete_captures_merged_fields() {
        let tel = TestTelemetry::with_context(
            fake_clock(),
            fields! { device = "MacBook Mic", rate = 48_000u32 },
        );
        tel_info!(&tel, "starting capture", take = 7u32);
        let cap = tel.logs();
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
        let tel: Arc<dyn Telemetry> = Arc::new(TestTelemetry::new(fake_clock()));
        tel_info!(tel, "hello", k = "v");
        let _ = tel;
    }

    #[test]
    fn core_child_merges_context() {
        let parent = TelemetryCore::with_context(
            fake_clock(),
            fields! { phase = "boot", device = "parent-mic" },
        );
        let child = parent.child(fields! { device = "child-mic", take = 1u32 });
        assert_eq!(
            child.context().get("phase"),
            Some(&Value::Str("boot".into()))
        );
        assert_eq!(
            child.context().get("device"),
            Some(&Value::Str("child-mic".into()))
        );
        assert_eq!(child.context().get("take"), Some(&Value::U64(1)));
        assert_eq!(
            parent.context().get("device"),
            Some(&Value::Str("parent-mic".into()))
        );
        assert_eq!(parent.context().get("take"), None);
    }

    #[test]
    fn telemetry_child_inherits_and_merges_on_log() {
        let parent = TestTelemetry::with_context(fake_clock(), fields! { phase = "boot" });
        let child = parent.child(fields! { device = "MacBook Mic" });
        tel_info!(child, "opened", rate = 48_000u32);
        let cap = parent.logs();
        assert_eq!(cap.len(), 1);
        let c = &cap[0];
        assert_eq!(c.fields.get("phase"), Some(&Value::Str("boot".into())));
        assert_eq!(
            c.fields.get("device"),
            Some(&Value::Str("MacBook Mic".into()))
        );
        assert_eq!(c.fields.get("rate"), Some(&Value::U64(48_000)));
    }

    #[test]
    fn child_call_site_overrides_inherited_context() {
        let parent = TestTelemetry::with_context(fake_clock(), fields! { device = "parent-mic" });
        let child = parent.child(fields! { device = "child-mic" });
        tel_info!(child, "x", device = "call-mic");
        let cap = parent.logs();
        assert_eq!(
            cap[0].fields.get("device"),
            Some(&Value::Str("call-mic".into()))
        );
    }

    #[test]
    fn nested_children_share_capture_buffer() {
        let parent = TestTelemetry::new(fake_clock());
        let a = parent.child(fields! { scope = "a" });
        let b = a.child(fields! { scope2 = "b" });
        tel_info!(parent, "from-parent");
        tel_info!(a, "from-a");
        tel_info!(b, "from-b");
        let cap = parent.logs();
        assert_eq!(cap.len(), 3);
        assert_eq!(cap[0].msg, "from-parent");
        assert_eq!(cap[1].msg, "from-a");
        assert_eq!(cap[2].msg, "from-b");
        assert_eq!(cap[2].fields.get("scope"), Some(&Value::Str("a".into())));
        assert_eq!(cap[2].fields.get("scope2"), Some(&Value::Str("b".into())));
    }

    // ----------------------- Event tests -----------------------

    #[test]
    fn event_name_is_locked_to_variant() {
        assert_eq!(
            Event::Boot {
                app_version: "0.1.0"
            }
            .name(),
            "boot"
        );
        assert_eq!(Event::Shutdown { uptime_ms: 0 }.name(), "shutdown");
    }

    #[test]
    fn event_fields_carry_only_variant_specific_data() {
        let f = Event::Boot {
            app_version: "0.1.0",
        }
        .fields();
        assert_eq!(f.get("app_version"), Some(&Value::Str("0.1.0".into())));
        // Common fields are *not* on Event::fields() — they come from stamp.
        assert_eq!(f.get("event"), None);
        assert_eq!(f.get("t_ms"), None);
    }

    #[test]
    fn stamp_adds_event_name_and_t_ms() {
        // 42 ms in ns.
        let core = TelemetryCore::new(fake_clock_at(42_000_000));
        let f = core.stamp(&Event::Boot {
            app_version: "0.1.0",
        });
        assert_eq!(f.get("event"), Some(&Value::Str("boot".into())));
        assert_eq!(f.get("t_ms"), Some(&Value::U64(42)));
        assert_eq!(f.get("app_version"), Some(&Value::Str("0.1.0".into())));
    }

    #[test]
    fn event_does_not_pick_up_logger_context() {
        let core = TelemetryCore::with_context(
            fake_clock_at(1_000_000), // 1 ms
            fields! { phase = "ignored", device = "ignored-mic" },
        );
        let f = core.stamp(&Event::Shutdown { uptime_ms: 1000 });
        // Schema-locked: only event fields + stamps.
        assert_eq!(f.get("event"), Some(&Value::Str("shutdown".into())));
        assert_eq!(f.get("t_ms"), Some(&Value::U64(1)));
        assert_eq!(f.get("uptime_ms"), Some(&Value::U64(1000)));
        assert_eq!(f.get("phase"), None);
        assert_eq!(f.get("device"), None);
    }

    #[test]
    fn test_telemetry_captures_events_separately_from_logs() {
        let tel = TestTelemetry::new(fake_clock_at(100_000_000)); // 100 ms
        tel_info!(&tel, "a log line");
        tel.event(&Event::Boot {
            app_version: "0.1.0",
        });
        tel.event(&Event::Shutdown { uptime_ms: 99 });

        let logs = tel.logs();
        let events = tel.events();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].msg, "a log line");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "boot");
        assert_eq!(
            events[0].fields.get("app_version"),
            Some(&Value::Str("0.1.0".into()))
        );
        assert_eq!(events[0].fields.get("t_ms"), Some(&Value::U64(100)));
        assert_eq!(events[1].name, "shutdown");
        assert_eq!(events[1].fields.get("uptime_ms"), Some(&Value::U64(99)));
    }
}
