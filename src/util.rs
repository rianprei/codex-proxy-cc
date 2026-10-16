//! Zero-dependency utilities: unique IDs and RFC 3339 timestamps.
//!
//! Replaces the `uuid` and `chrono` crates with ~60 lines of std-only code.
//! The IDs only need to be unique within a process lifetime (message ids,
//! request ids, session ids); the timestamps only need to round-trip as
//! RFC 3339 for the Codex auth file's `last_refresh` field.

use std::sync::atomic::{AtomicU64, Ordering};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique id without the uuid crate: nanosecond timestamp + process-unique
/// counter, hex-encoded. Collision-free within a process; across restarts
/// the timestamp prefix guarantees ordering.
pub fn unique_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let ctr = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ts:x}{ctr:06x}")
}

/// Days from the Unix epoch to a civil date (Howard Hinnant's algorithm,
/// public domain). Inverted for the reverse mapping.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil date from days since the Unix epoch.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current UTC time as `(unix_seconds, nanos)`.
fn now_parts() -> (i64, u32) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

/// RFC 3339 UTC timestamp, second precision: `2026-10-15T14:00:00Z`.
pub fn rfc3339_now() -> String {
    let (secs, _) = now_parts();
    rfc3339_from_unix(secs)
}

/// RFC 3339 UTC timestamp, microsecond precision: `2026-10-15T14:00:00.123456Z`.
pub fn rfc3339_now_micros() -> String {
    let (secs, nanos) = now_parts();
    let (y, mo, d) = civil_from_days(secs.div_euclid(86400));
    let sod = secs.rem_euclid(86400);
    let (h, mi, s) = (sod / 3600, (sod / 60) % 60, sod % 60);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:06}Z",
        nanos / 1000
    )
}

/// Format a unix timestamp as RFC 3339 (second precision).
pub fn rfc3339_from_unix(secs: i64) -> String {
    let (y, mo, d) = civil_from_days(secs.div_euclid(86400));
    let sod = secs.rem_euclid(86400);
    let (h, mi, s) = (sod / 3600, (sod / 60) % 60, sod % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Parse an RFC 3339 timestamp to unix seconds. Handles `Z` suffix,
/// optional fractional seconds. Returns None on malformed input.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    // Minimum: 2026-10-15T14:00:00Z
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_ids_are_unique() {
        let a = unique_id();
        let b = unique_id();
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn rfc3339_round_trip() {
        let s = rfc3339_now();
        let unix = parse_rfc3339(&s).expect("parse own output");
        let back = rfc3339_from_unix(unix);
        assert_eq!(s, back);
    }

    #[test]
    fn rfc3339_known_date() {
        // 2026-10-15T00:00:00Z is unix 1792022400 (verified with `date +%s`).
        assert_eq!(parse_rfc3339("2026-10-15T00:00:00Z"), Some(1792022400));
        assert_eq!(rfc3339_from_unix(1792022400), "2026-10-15T00:00:00Z");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("2026-10-15"), None);
    }

    #[test]
    fn micros_format() {
        let s = rfc3339_now_micros();
        assert!(s.ends_with('Z'));
        assert!(s.contains('.'));
        assert_eq!(s.len(), 27);
    }
}
