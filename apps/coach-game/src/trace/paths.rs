//! Canonical trace-path helpers: the one place that knows the filename
//! shape, the root constant, and how to resolve "newest" trace.
//!
//! Every site that needs a trace file path or wants to find the latest run
//! goes through this module — no more scattered `"ux.jsonl.gz"` literals.

use std::fs;
use std::path::{Path, PathBuf};

/// Gitignored directory all traces live under, relative to the working dir.
pub const ROOT: &str = "traces";

/// Resolve the trace root for the current platform.
///
/// On iOS the working directory is not writable (sandbox), so we write into
/// the app's Documents container instead — readable via
/// `xcrun simctl get_app_container booted <bundle-id> data` after the run.
///
/// On every other platform (Mac, Linux, …) this returns `PathBuf::from(ROOT)`,
/// preserving the existing relative-path behaviour exactly.
#[cfg(target_os = "ios")]
pub fn trace_root() -> PathBuf {
    // NSHomeDirectory() returns the app's sandbox root
    // (e.g. `.../Application/<uuid>`).  Documents/ is the conventional
    // container-backed user location; `traces/` keeps a clean subdir.
    use objc2_foundation::NSHomeDirectory;
    let home = NSHomeDirectory();
    PathBuf::from(home.to_string()).join("Documents").join(ROOT)
}

#[cfg(not(target_os = "ios"))]
pub fn trace_root() -> PathBuf {
    PathBuf::from(ROOT)
}

/// Fixed suffix appended to every trace file: `<stamp>-ux.jsonl.gz`.
const SUFFIX: &str = "-ux.jsonl.gz";

/// The file name for a run stamped `stamp`: `<stamp>-ux.jsonl.gz`.
pub fn file_name(stamp: &str) -> String {
    format!("{stamp}{SUFFIX}")
}

/// Full path for a run: `<root>/<stamp>-ux.jsonl.gz`.
pub fn file_path(root: &Path, stamp: &str) -> PathBuf {
    root.join(file_name(stamp))
}

/// Open a trace file for exclusive creation, returning the path used.
///
/// Uses `create_new(true)` so a trace is never truncated or overwritten.
/// On `AlreadyExists` (two runs that stamp the same millisecond, or a
/// backwards clock step), a numeric tie-breaker is inserted before the
/// suffix: `<stamp>.001-ux.jsonl.gz`, `.002`, … up to 999. The separator
/// is `.` (not `-`) deliberately: `.` (0x2E) sorts *after* the primary's
/// `-ux…` suffix but *before* the next millisecond's stamp, so
/// [`newest`] still returns the most recent file of a colliding pair —
/// the "latest = lexicographically greatest" invariant holds. (A `-`
/// separator would sort the tie-breaker *before* the primary and break it.)
pub fn create_new_file(root: &Path, stamp: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    let primary = file_path(root, stamp);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&primary)
    {
        Ok(f) => return Ok((f, primary)),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    for n in 1u32..=999 {
        let name = format!("{stamp}.{n:03}{SUFFIX}");
        let path = root.join(&name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => continue,
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("could not find a free trace filename for stamp {stamp:?} after 999 tries"),
    ))
}

/// The newest trace file under `root` — the lexicographically greatest name
/// whose filename ends in `SUFFIX` — or `None` if there are none.
///
/// Replaces `newest_dir`. Matches flat *files* ending in `-ux.jsonl.gz`;
/// old per-run subdirectories are correctly ignored.
pub fn newest(root: &Path) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for entry in fs::read_dir(root).ok()?.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let p = entry.path();
        let is_trace = p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(SUFFIX))
            .unwrap_or(false);
        if is_trace && best.as_ref().map(|b| p > *b).unwrap_or(true) {
            best = Some(p);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("gurukul-paths-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The collision tie-breaker must sort *after* the primary so `newest`
    /// returns the most recent file of a same-millisecond pair. A `-`
    /// separator would invert this (`-001-` < `-ux`); the fix is `.`.
    #[test]
    fn tiebreaker_sorts_after_primary() {
        let dir = temp_dir("tie");
        let stamp = "2026-06-12-004623-767";

        // First create takes the primary, second collides onto `.001`.
        let (_f1, primary) = create_new_file(&dir, stamp).unwrap();
        let (_f2, second) = create_new_file(&dir, stamp).unwrap();
        assert_eq!(primary, file_path(&dir, stamp), "first wins the bare name");
        assert_eq!(
            second.file_name().unwrap().to_str().unwrap(),
            "2026-06-12-004623-767.001-ux.jsonl.gz",
            "collision uses the `.NNN` tie-breaker"
        );

        // newest must pick the *second* (newer) file, not the primary.
        assert_eq!(
            newest(&dir).unwrap(),
            second,
            "newest must return the collision file, not the older primary"
        );

        // And a genuinely later stamp must still outrank a tie-broken earlier one.
        let (_f3, later) = create_new_file(&dir, "2026-06-12-004623-768").unwrap();
        assert_eq!(
            newest(&dir).unwrap(),
            later,
            "next-ms stamp outranks `.001`"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `newest` matches flat trace files only, ignoring leftover old-layout
    /// subdirectories.
    #[test]
    fn newest_ignores_subdirs() {
        let dir = temp_dir("dirs");
        fs::create_dir_all(dir.join("2026-06-12-000000-000")).unwrap(); // old layout
        assert_eq!(newest(&dir), None, "a bare subdir is not a trace");
        let (_f, file) = create_new_file(&dir, "2026-06-12-010000-000").unwrap();
        assert_eq!(newest(&dir).unwrap(), file);
        let _ = fs::remove_dir_all(&dir);
    }
}
