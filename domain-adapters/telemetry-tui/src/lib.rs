//! adapter-telemetry-tui: a Telemetry adapter that buffers formatted
//! log lines into a bounded ring the TUI head reads on every frame.
//!
//! When ratatui owns the terminal, anything that writes directly to
//! stdout/stderr will corrupt the frame. Telemetry calls are routed
//! here instead — the adapter formats with the same shape as
//! `adapter-telemetry-std` and pushes the line into a shared
//! [`LogBuffer`]. The head pulls lines out and renders them in the
//! console pane.
//!
//! Capacity is bounded; on overflow the oldest line is dropped (a
//! ring, not a queue). One sustained burst won't grow memory; the
//! head can scroll back as far as the ring goes.

use domain_ports::clock::Clock;
use domain_ports::telemetry::{Event, Fields, Level, Telemetry, TelemetryCore};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Shared, bounded log ring. Cheap to `Arc::clone` and pass to the
/// TUI head.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Push one formatted line. Drops the oldest line on overflow.
    pub fn push(&self, line: String) {
        let mut q = self.inner.lock().unwrap();
        if q.len() == self.capacity {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// Snapshot the current contents as a `Vec<String>` for rendering.
    /// Allocates one Vec per call — fine at frame rate, the buffer is
    /// at most a few thousand short strings.
    pub fn snapshot(&self) -> Vec<String> {
        let q = self.inner.lock().unwrap();
        q.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Build a TUI-backed Telemetry. Returns the `impl Telemetry` and the
/// shared [`LogBuffer`] the head reads from. The clock supplies `t_ms`
/// for events.
pub fn new(clock: Arc<dyn Clock>, capacity: usize) -> (impl Telemetry, LogBuffer) {
    let buffer = LogBuffer::new(capacity);
    let adapter = BufferTelemetry {
        core: TelemetryCore::new(clock),
        buffer: buffer.clone(),
    };
    (adapter, buffer)
}

struct BufferTelemetry {
    core: TelemetryCore,
    buffer: LogBuffer,
}

impl Telemetry for BufferTelemetry {
    fn log(&self, level: Level, msg: &str, fields: &Fields) {
        let merged = self.core.merge(fields);
        let line = if merged.is_empty() {
            format!("[{level}] {msg}")
        } else {
            format!("[{level}] {msg} {merged}")
        };
        self.buffer.push(line);
    }

    fn child(&self, fields: Fields) -> Arc<dyn Telemetry> {
        Arc::new(BufferTelemetry {
            core: self.core.child(fields),
            buffer: self.buffer.clone(),
        })
    }

    fn event(&self, e: &Event) {
        let stamped = self.core.stamp(e);
        self.buffer.push(format!("[EVENT] {stamped}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_ports::clock::TestClock;
    use domain_ports::fields;

    fn make(cap: usize) -> (impl Telemetry, LogBuffer) {
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new(0));
        new(clock, cap)
    }

    #[test]
    fn lines_land_in_buffer_in_order() {
        let (tel, buf) = make(16);
        tel.log(Level::Info, "one", &Fields::new());
        tel.log(Level::Warn, "two", &fields! { k = 1u32 });
        let lines = buf.snapshot();
        assert_eq!(lines, vec!["[INFO] one", "[WARN] two {k=1}"]);
    }

    #[test]
    fn ring_drops_oldest_on_overflow() {
        let (tel, buf) = make(3);
        for i in 0..5 {
            tel.log(Level::Info, &format!("msg{i}"), &Fields::new());
        }
        let lines = buf.snapshot();
        assert_eq!(
            lines,
            vec!["[INFO] msg2", "[INFO] msg3", "[INFO] msg4"],
            "ring should retain the most recent {} lines",
            3
        );
    }

    #[test]
    fn child_shares_the_same_buffer() {
        let (parent, buf) = make(16);
        let child = parent.child(fields! { scope = "boot" });
        parent.log(Level::Info, "p", &Fields::new());
        child.log(Level::Info, "c", &Fields::new());
        let lines = buf.snapshot();
        assert_eq!(lines, vec!["[INFO] p", "[INFO] c {scope=\"boot\"}"]);
    }
}
