//! Trace reader: parse a recorded `<stamp>-ux.jsonl.gz` back into typed
//! records the replay driver can serve frame by frame.
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
use std::path::Path;

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

/// Decode a `<stamp>-ux.jsonl.gz` file to a `String`, tolerating a missing
/// gzip trailer. A run killed mid-flight leaves a stream whose flushed deflate
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
            // Truncated stream (killed run): the flushed lines are already in
            // `text`, but the tail may be a *partial* final line — bytes the
            // app wrote after the last `\n` but before the kill. `str::lines`
            // would hand that partial line to the parser as if it were a whole
            // record, so on truncation we drop everything after the last
            // newline. A complete final record always ends in `\n` (the writer
            // uses `writeln!`), so this only ever discards a genuinely
            // incomplete line. The loss boundary matches the plain-`BufWriter`
            // writer: the last unterminated line is gone, every flushed line
            // before it survives.
            if let Some(last_nl) = text.rfind('\n') {
                text.truncate(last_nl + 1);
            } else {
                // Not even one complete line decoded — nothing usable.
                text.clear();
            }
        }
        Err(e) => {
            return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
        }
    }
    Ok(text)
}

/// Load and parse the trace file at `path` (`traces/<stamp>-ux.jsonl.gz`).
///
/// Refuses any schema other than the current [`SCHEMA_VERSION`] — replay
/// serves reads verbatim, so a reader that can't guarantee the channel's shape
/// must not pretend to.
pub fn load(path: &Path) -> io::Result<LoadedTrace> {
    let text = decode_gz(path)?;

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
            // Output-only channels are not replayed as inputs. Listed explicitly
            // so a `grep "poly"` finds this site.
            "poly" | "state" | "cmd" | "geom" | "mark" => {}
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

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::paths;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::path::PathBuf;

    /// Write a gzip-compressed trace file at `<root>/<stamp>-ux.jsonl.gz` and
    /// return the **file path**.
    fn write_trace(tag: &str, lines: &[&str]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gurukul-load-test-{tag}-{n}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = paths::file_path(&root, "run");
        let file = fs::File::create(&path).unwrap();
        let mut gz = GzEncoder::new(file, Compression::fast());
        gz.write_all(lines.join("\n").as_bytes()).unwrap();
        gz.finish().unwrap();
        path
    }

    const HEADER_V4: &str = r#"{"f":0,"k":"run","schema":4,"app_version":"0.1.0","window_logical":[800.0,600.0],"scale_factor":2.0,"wall_start":"2026-06-10 00:00:00 UTC"}"#;

    #[test]
    fn parses_each_record_kind() {
        let path = write_trace(
            "kinds",
            &[
                HEADER_V4,
                r#"{"f":1,"k":"frame","delta_s":0.016}"#,
                r#"{"f":1,"k":"coach","drained":[{"hop_index":1,"f0_hz":222.0,"confidence":0.7,"onset":0.0,"breath":0.0,"vibrato_rate":0.0,"vibrato_depth":0.0,"t_ms":1010}]}"#,
                r#"{"f":1,"k":"input","input":"cursor","pos":[640.0,360.0]}"#,
                r#"{"f":1,"k":"input","input":"mouse_button","button":"Left","state":"pressed"}"#,
                r#"{"f":1,"k":"input","input":"key","key":"F8","state":"pressed","repeat":false}"#,
                r#"{"f":1,"k":"state","from":"—","to":"MainMenu"}"#,
                r#"{"f":2,"k":"frame","delta_s":0.017}"#,
            ],
        );
        let trace = load(&path).expect("loads");
        assert_eq!(trace.header.schema, 4);
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
        let path = write_trace("schema", &[bad_header]);
        match load(&path) {
            Ok(_) => panic!("schema 1 must be refused"),
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData);
                assert!(e.to_string().contains("schema 1"), "got {e}");
            }
        }
    }

    #[test]
    fn newest_picks_greatest_file() {
        let root = std::env::temp_dir().join(format!("gurukul-newest-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for stamp in [
            "2026-06-10-090000-000",
            "2026-06-10-143212-000",
            "2026-06-09-235959-000",
        ] {
            let path = paths::file_path(&root, stamp);
            fs::write(&path, b"").unwrap();
        }
        // Also add a subdirectory with the old layout — it must be ignored.
        fs::create_dir_all(root.join("2026-06-11-000000-000")).unwrap();

        let newest = paths::newest(&root).expect("a trace file");
        assert_eq!(
            newest.file_name().unwrap(),
            paths::file_name("2026-06-10-143212-000").as_str()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Build raw gzip bytes from `lines`, sync-flushed per line, then truncated
    /// to omit the trailer — exactly the bytes a killed process leaves on disk.
    ///
    /// The truncation must be done deliberately: `GzEncoder`'s `Drop` impl calls
    /// `try_finish()`, so merely letting the encoder fall out of scope writes the
    /// trailer and yields a *complete* file (the trap both code reviews caught).
    /// Instead we record the byte length right after the last `Z_SYNC_FLUSH`,
    /// then `finish()` and truncate back to that length — dropping the trailer
    /// the same way a `kill -9` does. `tail` is extra bytes appended after the
    /// final newline-terminated record (an in-progress partial line), or empty.
    fn trailerless_gz(lines: &[&str], tail: &[u8]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        for line in lines {
            writeln!(gz, "{line}").unwrap();
            gz.flush().unwrap(); // Z_SYNC_FLUSH — flushed, stream still open
        }
        gz.write_all(tail).unwrap();
        gz.flush().unwrap();
        // Byte boundary of everything flushed so far, *before* any trailer.
        let cut = gz.get_ref().len();
        let mut raw = gz.finish().unwrap(); // appends the trailer
        raw.truncate(cut); // …which we now chop off
        raw
    }

    fn write_raw(tag: &str, raw: &[u8]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gurukul-trunc-test-{tag}-{n}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = paths::file_path(&root, "run");
        fs::write(&path, raw).unwrap();
        path
    }

    /// Crash-recovery contract: a sync-flushed, trailerless stream (killed run)
    /// must still yield every flushed record — the whole point of per-frame
    /// flush.
    #[test]
    fn truncated_stream_recovers_flushed_lines() {
        let raw = trailerless_gz(
            &[
                HEADER_V4,
                r#"{"f":1,"k":"frame","delta_s":0.016}"#,
                r#"{"f":2,"k":"frame","delta_s":0.017}"#,
            ],
            b"",
        );

        // Guard against the Drop-writes-the-trailer trap regressing: the fixture
        // must genuinely fail strict (trailer-requiring) decoding.
        assert!(
            flate2::read::GzDecoder::new(&raw[..])
                .read_to_string(&mut String::new())
                .is_err(),
            "fixture must be trailerless, else the recovery path is untested"
        );

        let path = write_raw("plain", &raw);
        let trace = load(&path).expect("truncated stream must still load");
        assert_eq!(
            trace.frames.len(),
            2,
            "both flushed frame records must be recovered from the truncated stream"
        );
        assert_eq!(trace.frames[0].frame, 1);
        assert_eq!(trace.frames[1].frame, 2);
        assert_eq!(trace.frames[0].delta_s, Some(0.016));
        assert_eq!(trace.frames[1].delta_s, Some(0.017));
    }

    /// A run killed *mid-write* leaves a partial final line — bytes after the
    /// last `\n`. The loader must drop that incomplete line, not feed it to the
    /// parser as if it were a whole record (which would error out the whole
    /// trace, or worse, parse a half-record as real data).
    #[test]
    fn truncated_stream_drops_partial_final_line() {
        let raw = trailerless_gz(
            &[HEADER_V4, r#"{"f":1,"k":"frame","delta_s":0.016}"#],
            // A half-written next record: no closing brace, no newline.
            br#"{"f":2,"k":"fra"#,
        );
        let path = write_raw("partial", &raw);
        let trace = load(&path).expect("partial final line must not fail the load");
        assert_eq!(
            trace.frames.len(),
            1,
            "only the complete flushed frame survives; the partial line is dropped"
        );
        assert_eq!(trace.frames[0].frame, 1);
    }
}
