//! The buffered trace-file writer.
//!
//! [`TraceWriter`] owns one gzip encoder over `traces/<stamp>-ux.jsonl.gz` and
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
//! `gzcat`/`gunzip` (and `jq`) read with no fuss. There is also a backstop:
//! `GzEncoder`'s own `Drop` calls `try_finish()`, so any path that unwinds the
//! `World` normally — including a **panic** — still flushes the trailer even if
//! `finish()` was never called. Don't remove either path thinking it redundant:
//! `finish()` covers the `AppExit`-before-`Last` ordering case, `Drop` covers
//! the rest. Only an abrupt kill that skips destructors entirely (`kill -9`,
//! `abort`, `process::exit`) leaves a trailerless stream. Such a trace is still
//! recoverable — every sync-flushed block decodes — but stock `gzcat` on macOS
//! prints nothing for a trailerless stream, so the reader
//! ([`super::replay::load`]) and the docs both provide a tolerant unpack path
//! for that case.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bevy::prelude::Resource;
use flate2::write::GzEncoder;
use flate2::Compression;

use super::paths;
use super::record::{Body, Record};

/// Buffered gzip writer for one run's `<stamp>-ux.jsonl.gz`. A Bevy resource
/// so the recording systems can `ResMut` it; flushed in `Last`.
#[derive(Resource)]
pub struct TraceWriter {
    /// `None` only after [`Self::finish`] has consumed the encoder to write
    /// the gzip trailer. Every other method treats `None` as "already
    /// finalized" and becomes a no-op.
    out: Option<GzEncoder<BufWriter<fs::File>>>,
    path: PathBuf,
}

impl TraceWriter {
    /// Create `<root>/<stamp>-ux.jsonl.gz`, creating the root directory.
    ///
    /// `stamp` is the launch-time filename stamp (lexicographically sortable →
    /// "latest" is the greatest name); the caller stamps it so this stays
    /// testable with a temp dir.
    ///
    /// Uses `create_new` so a trace is never truncated or overwritten. On a
    /// millisecond collision a numeric tie-breaker is appended via
    /// [`paths::create_new_file`].
    pub fn create(root: &Path, stamp: &str) -> std::io::Result<Self> {
        fs::create_dir_all(root)?;
        let (file, path) = paths::create_new_file(root, stamp)?;
        Ok(Self {
            out: Some(GzEncoder::new(BufWriter::new(file), Compression::fast())),
            path,
        })
    }

    /// The file path this run is writing into.
    pub fn path(&self) -> &Path {
        &self.path
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
