//! Trace reader: parse a recorded `ux.jsonl.gz` back into typed records the
//! replay driver can serve frame by frame.
//!
//! The write side ([`super::super::record`]) is `Serialize`-only — its `Body`
//! carries `&'static str` fields (`app_version`) that cannot round-trip through
//! `Deserialize`. So this module defines its *own* read-side shapes (owning
//! `String` where the writer had `&'static str`) and parses each line in two
//! steps: pull `f`/`k` from a `serde_json::Value`, then deserialize the rest
//! into the matching payload. Two-step rather than one `#[serde(flatten)]` +
//! internally-tagged enum, because that serde combination is finicky on the
//! read path and the per-kind match gives precise errors — exactly what a
//! human or agent reading a malformed trace wants.
//!
//! The port payloads (`FeatureSnapshot`, `CoachEvent`, `Command`, `Scale`)
//! reuse their own `Deserialize` impls (the `serde` feature on `domain-ports`),
//! so this module never restates a port type's shape.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use serde::Deserialize;
use serde_json::Value;

use domain_ports::app_coach::{CoachEvent, FeatureSnapshot};

use crate::trace::record::SCHEMA_VERSION;

/// A fully-loaded trace: the one-time header plus per-frame record buckets in
/// ascending frame order.
pub struct LoadedTrace {
    pub header: Header,
    /// One bucket per frame that produced at least one record, ascending by
    /// frame index. The driver walks these in order.
    pub frames: Vec<FrameRecords>,
}

impl LoadedTrace {
    /// The greatest frame index present, or 0 for an empty trace. The driver
    /// runs until it has served this frame.
    pub fn last_frame(&self) -> u32 {
        self.frames.last().map(|fr| fr.frame).unwrap_or(0)
    }
}

/// The `run` header, read side. Mirrors [`crate::trace::record::Body::Run`]
/// with owned strings.
#[derive(Debug, Clone)]
pub struct Header {
    pub schema: u32,
    pub window_logical: [f32; 2],
    pub scale_factor: f32,
}

/// Everything recorded at one frame index, grouped so the driver can serve a
/// frame's coach reads + inputs + delta in one lookup.
#[derive(Default)]
pub struct FrameRecords {
    pub frame: u32,
    /// `frame` record's delta, if present (every live frame writes one).
    pub delta_s: Option<f32>,
    /// The `coach` read recorded this frame (at most one record per frame).
    pub coach: Option<CoachRead>,
    /// Input messages recorded this frame, in recorded order.
    pub inputs: Vec<InputRecord>,
}

/// Read side of the `coach` record — what `drain_events` saw this frame, as
/// the port types (schema 2). No `Debug`: it embeds `CoachEvent`, which the
/// port deliberately keeps `Debug`-free.
#[derive(Default, Deserialize)]
pub struct CoachRead {
    #[serde(default)]
    pub events: Vec<CoachEvent>,
    #[serde(default)]
    pub latest: Option<FeatureSnapshot>,
    #[serde(default)]
    pub drained: Vec<FeatureSnapshot>,
}

/// Read side of one `input` record. Internally tagged by `input`, matching the
/// writer ([`crate::trace::record::InputRecord`]) — the schema-3 `WindowEvent`
/// variant set. The driver turns each back into a Bevy [`WindowEvent`] (and
/// fans out to the typed channels, as winit does).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum InputRecord {
    Key {
        key: String,
        state: String,
        #[serde(default)]
        repeat: bool,
    },
    KeyboardFocusLost,
    MouseButton {
        button: String,
        state: String,
    },
    Cursor {
        pos: [f32; 2],
    },
    CursorEntered,
    CursorLeft,
    Wheel {
        unit: String,
        x: f32,
        y: f32,
    },
    Touch {
        phase: String,
        pos: [f32; 2],
        id: u64,
    },
    Resize {
        size: [f32; 2],
    },
    ScaleFactor {
        scale_factor: f64,
    },
}

/// Decode a `ux.jsonl.gz` file to a `String`, tolerating a missing gzip
/// trailer. A run killed mid-flight leaves a stream whose flushed deflate
/// blocks all decode but has no final CRC/ISIZE trailer; `MultiGzDecoder`
/// writes all flushed lines into the buffer and then returns
/// `UnexpectedEof`. We keep what was decoded and treat that as the end of
/// the stream — the same loss boundary as today's plain-`BufWriter` flush.
fn decode_gz(path: &Path) -> io::Result<String> {
    let file = fs::File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let mut decoder = MultiGzDecoder::new(file);
    let mut text = String::new();
    match decoder.read_to_string(&mut text) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // Truncated stream (killed run) — the flushed lines are already in
            // `text`; only the final partial line (if any) after the last
            // Z_SYNC_FLUSH boundary is missing. The `.lines()` parse below
            // handles that cleanly.
        }
        Err(e) => {
            return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
        }
    }
    Ok(text)
}

/// Load and parse `<dir>/ux.jsonl.gz`. Refuses any schema other than the
/// current [`SCHEMA_VERSION`] — replay serves reads verbatim, so a reader
/// that can't guarantee the channel's shape must not pretend to.
pub fn load(dir: &Path) -> io::Result<LoadedTrace> {
    let path = dir.join("ux.jsonl.gz");
    let text = decode_gz(&path)?;

    let mut lines = text.lines().filter(|l| !l.trim().is_empty());

    // First line must be the `run` header.
    let first = lines
        .next()
        .ok_or_else(|| err(format!("{}: empty trace", path.display())))?;
    let header = parse_header(first).map_err(err)?;
    if header.schema != SCHEMA_VERSION {
        return Err(err(format!(
            "{}: schema {} (replay needs {SCHEMA_VERSION}) — re-record the trace",
            path.display(),
            header.schema
        )));
    }

    // Bucket the remaining records by frame, preserving in-frame order. A
    // BTreeMap keeps frames ascending without a separate sort.
    let mut buckets: BTreeMap<u32, FrameRecords> = BTreeMap::new();
    for line in lines {
        let v: Value =
            serde_json::from_str(line).map_err(|e| err(format!("bad json line: {e}")))?;
        let f = v
            .get("f")
            .and_then(Value::as_u64)
            .ok_or_else(|| err(format!("record missing `f`: {line}")))? as u32;
        let k = v
            .get("k")
            .and_then(Value::as_str)
            .ok_or_else(|| err(format!("record missing `k`: {line}")))?;

        let bucket = buckets.entry(f).or_insert_with(|| FrameRecords {
            frame: f,
            ..Default::default()
        });
        match k {
            "frame" => {
                bucket.delta_s = v.get("delta_s").and_then(Value::as_f64).map(|d| d as f32);
            }
            "coach" => {
                bucket.coach = Some(
                    CoachRead::deserialize(&v)
                        .map_err(|e| err(format!("bad coach record: {e}")))?,
                );
            }
            "input" => {
                bucket.inputs.push(
                    InputRecord::deserialize(&v)
                        .map_err(|e| err(format!("bad input record: {e}")))?,
                );
            }
            // `state`, `cmd`, `geom`, `mark` carry no input the driver replays
            // (cmd/geom/mark are outputs; state is re-derived by the app). They
            // stay in the file for diffing but the loader ignores them.
            _ => {}
        }
    }

    Ok(LoadedTrace {
        header,
        frames: buckets.into_values().collect(),
    })
}

/// Parse the `run` header line. Only the fields replay needs are pulled
/// (schema gate + window override); the rest stay in the file for readers.
fn parse_header(line: &str) -> Result<Header, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("bad header json: {e}"))?;
    if v.get("k").and_then(Value::as_str) != Some("run") {
        return Err(format!("first line is not a `run` header: {line}"));
    }
    let schema = v
        .get("schema")
        .and_then(Value::as_u64)
        .ok_or("header missing `schema`")? as u32;
    let wl = v
        .get("window_logical")
        .and_then(Value::as_array)
        .ok_or("header missing `window_logical`")?;
    let window_logical = [
        wl.first().and_then(Value::as_f64).unwrap_or(0.0) as f32,
        wl.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32,
    ];
    let scale_factor = v.get("scale_factor").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    Ok(Header {
        schema,
        window_logical,
        scale_factor,
    })
}

/// The lexicographically greatest subdirectory of `root` — the "newest" trace,
/// since run directories are stamped `YYYY-MM-DD-HHMMSS` (sortable by
/// construction; see [`crate::trace::launch_stamp`]). `None` if `root` has no
/// subdirectories.
pub fn newest_dir(root: &Path) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for entry in fs::read_dir(root).ok()?.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let p = entry.path();
            if best.as_ref().map(|b| p > *b).unwrap_or(true) {
                best = Some(p);
            }
        }
    }
    best
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Write a gzip-compressed trace file under a fresh temp dir and return its
    /// run directory.
    fn write_trace(tag: &str, lines: &[&str]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gurukul-load-test-{tag}-{n}"));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("run");
        fs::create_dir_all(&dir).unwrap();
        let file = fs::File::create(dir.join("ux.jsonl.gz")).unwrap();
        let mut gz = GzEncoder::new(file, Compression::fast());
        gz.write_all(lines.join("\n").as_bytes()).unwrap();
        gz.finish().unwrap();
        dir
    }

    const HEADER_V3: &str = r#"{"f":0,"k":"run","schema":3,"app_version":"0.1.0","window_logical":[800.0,600.0],"scale_factor":2.0,"wall_start":"2026-06-10 00:00:00 UTC"}"#;

    #[test]
    fn parses_each_record_kind() {
        let dir = write_trace(
            "kinds",
            &[
                HEADER_V3,
                r#"{"f":1,"k":"frame","delta_s":0.016}"#,
                r#"{"f":1,"k":"coach","drained":[{"hop_index":1,"f0_hz":222.0,"confidence":0.7,"onset":0.0,"breath":0.0,"vibrato_rate":0.0,"vibrato_depth":0.0,"t_ms":1010}]}"#,
                r#"{"f":1,"k":"input","input":"cursor","pos":[640.0,360.0]}"#,
                r#"{"f":1,"k":"input","input":"mouse_button","button":"Left","state":"pressed"}"#,
                r#"{"f":1,"k":"input","input":"key","key":"F8","state":"pressed","repeat":false}"#,
                r#"{"f":1,"k":"state","from":"—","to":"MainMenu"}"#,
                r#"{"f":2,"k":"frame","delta_s":0.017}"#,
            ],
        );
        let trace = load(&dir).expect("loads");
        assert_eq!(trace.header.schema, 3);
        assert_eq!(trace.header.window_logical, [800.0, 600.0]);
        assert_eq!(trace.header.scale_factor, 2.0);
        assert_eq!(trace.frames.len(), 2, "two frames produced records");

        let f1 = &trace.frames[0];
        assert_eq!(f1.frame, 1);
        assert_eq!(f1.delta_s, Some(0.016));
        let coach = f1.coach.as_ref().expect("coach record on frame 1");
        assert_eq!(coach.drained.len(), 1);
        assert!((coach.drained[0].f0_hz - 222.0).abs() < 1e-3);
        // Three inputs this frame, preserved in recorded (arrival) order:
        // cursor, then button, then key — the cross-channel ordering schema 3
        // exists to keep (a click needs the cursor move before it).
        assert_eq!(f1.inputs.len(), 3);
        match &f1.inputs[0] {
            InputRecord::Cursor { pos } => assert_eq!(*pos, [640.0, 360.0]),
            other => panic!("expected a cursor input first, got {other:?}"),
        }
        match &f1.inputs[1] {
            InputRecord::MouseButton { button, state } => {
                assert_eq!(button, "Left");
                assert_eq!(state, "pressed");
            }
            other => panic!("expected a mouse-button input second, got {other:?}"),
        }
        match &f1.inputs[2] {
            InputRecord::Key { key, state, .. } => {
                assert_eq!(key, "F8");
                assert_eq!(state, "pressed");
            }
            other => panic!("expected a key input third, got {other:?}"),
        }
        assert_eq!(trace.last_frame(), 2);
    }

    #[test]
    fn refuses_wrong_schema() {
        let bad_header = r#"{"f":0,"k":"run","schema":1,"app_version":"0.1.0","window_logical":[800.0,600.0],"scale_factor":2.0,"wall_start":"x"}"#;
        let dir = write_trace("schema", &[bad_header]);
        match load(&dir) {
            Ok(_) => panic!("schema 1 must be refused"),
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData);
                assert!(e.to_string().contains("schema 1"), "got {e}");
            }
        }
    }

    #[test]
    fn newest_dir_picks_greatest_name() {
        let root = std::env::temp_dir().join(format!("gurukul-newest-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for name in [
            "2026-06-10-090000",
            "2026-06-10-143212",
            "2026-06-09-235959",
        ] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        let newest = newest_dir(&root).expect("a subdirectory");
        assert_eq!(newest.file_name().unwrap(), "2026-06-10-143212");
        let _ = fs::remove_dir_all(&root);
    }

    /// Crash-recovery contract: sync-flush a few records into a gzip stream,
    /// then drop the gzip trailer (simulating a killed run). The loader must
    /// still return every flushed record — the whole point of per-frame flush.
    ///
    /// How the truncation is produced: write through a `GzEncoder` with
    /// `Z_SYNC_FLUSH` after each record (i.e. `flush()` on the encoder), capture
    /// the raw bytes, then truncate at the end of the last flushed block — before
    /// the gzip CRC/ISIZE trailer that `finish()` would append. That is exactly
    /// what the kernel sees for a killed run.
    #[test]
    fn truncated_stream_recovers_flushed_lines() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gurukul-trunc-test-{n}"));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("run");
        fs::create_dir_all(&dir).unwrap();

        // Build the raw gz bytes with sync-flushes but no trailer.
        let mut raw: Vec<u8> = Vec::new();
        {
            // `IntoInnerError` is infallible here since we flush before finish.
            let mut gz = GzEncoder::new(&mut raw, Compression::fast());
            for line in &[
                HEADER_V3,
                r#"{"f":1,"k":"frame","delta_s":0.016}"#,
                r#"{"f":2,"k":"frame","delta_s":0.017}"#,
            ] {
                writeln!(gz, "{line}").unwrap();
                // Z_SYNC_FLUSH: flushes compressed bytes without closing stream.
                gz.flush().unwrap();
            }
            // Deliberately do NOT call gz.finish() — we want a trailerless stream.
            // Drop the encoder; the raw bytes end after the last sync-flush block.
        }
        // `raw` now contains a valid gzip stream body (flushed blocks) with no
        // CRC/ISIZE trailer — the same bytes a killed process leaves on disk.
        fs::write(dir.join("ux.jsonl.gz"), &raw).unwrap();

        let trace = load(&dir).expect("truncated stream must still load");
        assert_eq!(
            trace.frames.len(),
            2,
            "both flushed frame records must be recovered from the truncated stream"
        );
        assert_eq!(trace.frames[0].frame, 1);
        assert_eq!(trace.frames[1].frame, 2);
        assert_eq!(trace.frames[0].delta_s, Some(0.016));
        assert_eq!(trace.frames[1].delta_s, Some(0.017));

        let _ = fs::remove_dir_all(&root);
    }
}
