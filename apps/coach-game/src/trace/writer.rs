//! The buffered trace-file writer.
//!
//! [`TraceWriter`] owns one `BufWriter` over `traces/<launch>/ux.jsonl` and
//! appends one JSON line per [`Record`]. Append-only and flushed once per
//! frame (in `Last`), so a panic mid-run leaves every line up to the crash
//! intact — the crash-safety the plan relies on.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bevy::prelude::Resource;

use super::record::{Body, Record};

/// Buffered writer for one run's `ux.jsonl`. A Bevy resource so the recording
/// systems can `ResMut` it; flushed in `Last`.
#[derive(Resource)]
pub struct TraceWriter {
    out: BufWriter<File>,
    dir: PathBuf,
}

impl TraceWriter {
    /// Create `<root>/<run_dir>/ux.jsonl`, creating parents. `run_dir` is the
    /// launch-time directory name (lexicographically sortable → "latest" is
    /// the greatest name); the caller stamps it so this stays testable with a
    /// temp dir.
    pub fn create(root: &Path, run_dir: &str) -> std::io::Result<Self> {
        let dir = root.join(run_dir);
        fs::create_dir_all(&dir)?;
        let file = File::create(dir.join("ux.jsonl"))?;
        Ok(Self {
            out: BufWriter::new(file),
            dir,
        })
    }

    /// The directory this run is writing into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one record at frame `f`. Serialization failure is swallowed to a
    /// log line — a malformed record must never crash the app being traced.
    pub fn write(&mut self, f: u32, body: Body) {
        let record = Record { f, body };
        match serde_json::to_string(&record) {
            Ok(line) => {
                // One record per line; ignore IO errors past logging — a full
                // disk should not take down the app under observation.
                if let Err(e) = writeln!(self.out, "{line}") {
                    bevy::log::warn!("trace write failed: {e}");
                }
            }
            Err(e) => bevy::log::warn!("trace serialize failed: {e}"),
        }
    }

    /// Flush the buffer to disk. Called once per frame in `Last`.
    pub fn flush(&mut self) {
        if let Err(e) = self.out.flush() {
            bevy::log::warn!("trace flush failed: {e}");
        }
    }
}
