//! The buffered trace-file writer.
//!
//! [`TraceWriter`] owns one gzip encoder over `traces/<launch>/ux.jsonl.gz` and
//! appends one JSON line per [`Record`]. Append-only and flushed once per
//! frame (in `Last`), so a panic mid-run leaves every line up to the crash
//! intact — the crash-safety the plan relies on.
//!
//! The writer stacks as `GzEncoder<BufWriter<File>>`. Each per-frame
//! `flush()` calls `Z_SYNC_FLUSH` on the encoder, which emits a complete,
//! decodable deflate block without ending the gzip stream — the compression
//! dictionary is preserved across flushes, so the ratio hit is small.
//!
//! On a graceful exit (window close / Cmd-Q → `AppExit`), [`Self::finish`]
//! writes the gzip *trailer*, leaving a fully-valid `.gz` that ordinary
//! `gzcat`/`gunzip` (and `jq`) read with no fuss. Only a hard crash or
//! `kill -9` skips the trailer; such a trace is still recoverable — every
//! sync-flushed block decodes — but stock `gzcat` on macOS prints nothing
//! for a trailerless stream, so the reader ([`super::replay::load`]) and the
//! docs both provide a tolerant unpack path for that case.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bevy::prelude::Resource;
use flate2::write::GzEncoder;
use flate2::Compression;

use super::record::{Body, Record};

/// Buffered gzip writer for one run's `ux.jsonl.gz`. A Bevy resource so the
/// recording systems can `ResMut` it; flushed in `Last`.
#[derive(Resource)]
pub struct TraceWriter {
    /// `None` only after [`Self::finish`] has consumed the encoder to write
    /// the gzip trailer. Every other method treats `None` as "already
    /// finalized" and becomes a no-op.
    out: Option<GzEncoder<BufWriter<File>>>,
    dir: PathBuf,
}

impl TraceWriter {
    /// Create `<root>/<run_dir>/ux.jsonl.gz`, creating parents. `run_dir` is
    /// the launch-time directory name (lexicographically sortable → "latest" is
    /// the greatest name); the caller stamps it so this stays testable with a
    /// temp dir.
    pub fn create(root: &Path, run_dir: &str) -> std::io::Result<Self> {
        let dir = root.join(run_dir);
        fs::create_dir_all(&dir)?;
        let file = File::create(dir.join("ux.jsonl.gz"))?;
        Ok(Self {
            out: Some(GzEncoder::new(BufWriter::new(file), Compression::fast())),
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
        let Some(out) = self.out.as_mut() else { return };
        let record = Record { f, body };
        match serde_json::to_string(&record) {
            Ok(line) => {
                // One record per line; ignore IO errors past logging — a full
                // disk should not take down the app under observation.
                if let Err(e) = writeln!(out, "{line}") {
                    bevy::log::warn!("trace write failed: {e}");
                }
            }
            Err(e) => bevy::log::warn!("trace serialize failed: {e}"),
        }
    }

    /// Flush the buffer to disk. Called once per frame in `Last`.
    ///
    /// On a `GzEncoder` this performs a `Z_SYNC_FLUSH`: it pushes all
    /// compressed bytes through the `BufWriter` and into the OS without closing
    /// the gzip stream. Every line written before this call is recoverable even
    /// if the process is killed before the next flush.
    pub fn flush(&mut self) {
        let Some(out) = self.out.as_mut() else { return };
        if let Err(e) = out.flush() {
            bevy::log::warn!("trace flush failed: {e}");
        }
    }

    /// Finish the gzip stream, writing its trailer (CRC + length), then flush
    /// the underlying file. Called once on graceful shutdown (`AppExit`); after
    /// this the writer is inert. A finalized trace is a fully-valid `.gz` that
    /// stock `gzcat`/`gunzip` read without complaint — the common case. A run
    /// that never reaches here (hard crash / `kill -9`) leaves a trailerless
    /// but still sync-flushed stream the tolerant reader recovers.
    pub fn finish(&mut self) {
        let Some(out) = self.out.take() else { return };
        match out.finish() {
            Ok(mut buf) => {
                if let Err(e) = buf.flush() {
                    bevy::log::warn!("trace finish flush failed: {e}");
                }
            }
            Err(e) => bevy::log::warn!("trace finish failed: {e}"),
        }
    }
}
