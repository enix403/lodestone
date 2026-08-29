//! Minimal calendar arithmetic.
//!
//! lodestone needs to stamp snapshots and name trash runs, and to compute the age of a
//! trash run for pruning. That is the whole requirement, so it does not justify a date
//! library — just Howard Hinnant's `civil_from_days` / `days_from_civil` pair, which is
//! the standard branch-free conversion in both directions.

/// Seconds since the Unix epoch, UTC.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-08-29T19:15:00Z` — for display and for snapshot metadata.
pub fn format_rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `20260829T191500Z` — ISO 8601 basic format.
///
/// Used for trash run directory names: filename-safe on every platform (no colons), and
/// lexicographically sortable, so listing directories in name order is chronological.
pub fn format_compact(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs as i64);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Inverse of [`format_compact`]. Returns `None` for anything not in that exact shape,
/// so a stray directory in the trash root is ignored rather than misread as a run.
pub fn parse_compact(s: &str) -> Option<u64> {
    if s.len() != 16 || s.as_bytes()[8] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(4, 6)?, num(6, 8)?);
    let (h, mi, sec) = (num(9, 11)?, num(11, 13)?, num(13, 15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    let total = days * 86_400 + h * 3600 + mi * 60 + sec;
    u64::try_from(total).ok()
}

pub fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Days since the Unix epoch for a civil date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn compact_round_trips() {
        for secs in [0u64, 1, 1_000_000_000, 1_709_164_800, 1_788_000_000] {
            let s = format_compact(secs);
            assert_eq!(s.len(), 16, "{s}");
            assert_eq!(parse_compact(&s), Some(secs), "{s}");
        }
    }

    #[test]
    fn compact_is_lexicographically_chronological() {
        // Listing trash runs in name order must be listing them in time order.
        let mut names: Vec<String> = [1_788_000_000u64, 1_000_000_000, 1_709_164_800]
            .iter()
            .map(|s| format_compact(*s))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                format_compact(1_000_000_000),
                format_compact(1_709_164_800),
                format_compact(1_788_000_000),
            ]
        );
    }

    #[test]
    fn parse_rejects_anything_that_is_not_a_run_name() {
        // A stray directory in the trash root must be ignored, not misread.
        for bad in [
            "",
            "notatimestamp",
            "20260829191500Z",  // missing T
            "20260829T191500",  // missing Z
            "20261329T191500Z", // month 13
            "20260832T191500Z", // day 32
            "20260829T251500Z", // hour 25
            "20260829T196100Z", // minute 61
            "20260829T19150XZ", // non-numeric
        ] {
            assert_eq!(parse_compact(bad), None, "should have rejected {bad:?}");
        }
    }

    #[test]
    fn rfc3339_formatting() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
