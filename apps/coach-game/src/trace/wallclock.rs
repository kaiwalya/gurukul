//! Wall-clock stamping for a trace run, with no extra dependency.
//!
//! The trace stamp must be lexicographically sortable so "latest" is the
//! greatest name, and the `run` header wants a human-readable start time.
//! The `Clock` port is monotonic-only (deltas, no civil time), and the crate
//! pulls in no `chrono`/`time`, so we convert `SystemTime` → UTC civil
//! datetime here with the standard days-since-epoch algorithm.
//!
//! UTC, not local: a portable local-time conversion needs the OS tz database
//! (a dependency we are avoiding), and a trace's only timestamp requirement is
//! a unique, sortable, roughly-human label — UTC satisfies all three. The
//! header notes the zone is UTC.

use std::time::{SystemTime, UNIX_EPOCH};

/// `(stamp, wall_start)` for the current launch:
/// - `stamp` = `YYYY-MM-DD-HHMMSS-mmm` (sortable filename stamp, 21 chars;
///   millis appended so two runs in the same second produce distinct names)
/// - `wall_start` = `YYYY-MM-DD HH:MM:SS UTC` (header label)
pub fn launch_stamp() -> (String, String) {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let ms = dur.subsec_millis(); // 0–999
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    (
        format!("{y:04}-{mo:02}-{d:02}-{h:02}{mi:02}{s:02}-{ms:03}"),
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC"),
    )
}

/// Convert Unix seconds to a UTC civil `(year, month, day, hour, min, sec)`.
/// Uses Howard Hinnant's `civil_from_days` algorithm (public domain) for the
/// date part — exact, branch-light, no lookup tables.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days is days since 1970-01-01. Shift the epoch to 0000-03-01 so the
    // leap-day lands at the end of the 400-year era.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], Mar=0
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    (year, month, day, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epochs_convert() {
        // 0 → 1970-01-01 00:00:00
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 1_700_000_000 → 2023-11-14 22:13:20 UTC (a known reference).
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn run_dir_is_sortable_and_shaped() {
        let (stamp, label) = launch_stamp();
        // YYYY-MM-DD-HHMMSS-mmm = 21 chars.
        // Format: date(10) + "-" + time(6) + "-" + millis(3) = 10+1+6+1+3 = 21.
        assert_eq!(stamp.len(), 21, "got {stamp}");
        // The millis field is the last 3 chars and must be all digits.
        let millis_part = &stamp[18..];
        assert!(
            millis_part.chars().all(|c| c.is_ascii_digit()),
            "millis field must be 3 digits, got {millis_part:?} in {stamp}"
        );
        assert!(label.ends_with(" UTC"));
    }
}
