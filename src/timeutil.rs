//! Minimal timestamp handling.
//!
//! ponytail: no `chrono`/`time` dependency. We only need (a) RFC 3339 parsing for
//! provider timestamps and (b) epoch-millis rendering for filesystem stamps, so a
//! civil-calendar conversion is enough. Ceiling: no leap-second table, no
//! ambiguous local-time resolution (local times without offset are rejected and
//! reported as `unknown` confidence rather than guessed). Upgrade path: swap in
//! `time` if we ever need calendrical arithmetic.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A UTC instant stored as nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Utc(pub i64);

impl Utc {
    pub fn to_rfc3339(self) -> String {
        let (secs, nanos) = (
            self.0.div_euclid(1_000_000_000),
            self.0.rem_euclid(1_000_000_000),
        );
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
        if nanos == 0 {
            format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
        } else {
            format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{nanos:09}Z")
        }
    }
}

impl fmt::Display for Utc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// Parsed timestamp plus how much we trust it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub utc: Option<Utc>,
    /// The literal text found in the source, kept verbatim.
    pub original: Option<String>,
    pub confidence: TimestampConfidence,
}

impl Stamp {
    pub fn unknown() -> Self {
        Stamp {
            utc: None,
            original: None,
            confidence: TimestampConfidence::Unknown,
        }
    }
    pub fn from_utc(utc: Utc, confidence: TimestampConfidence, original: Option<String>) -> Self {
        Stamp {
            utc: Some(utc),
            original,
            confidence,
        }
    }
    pub fn rfc3339(&self) -> Option<String> {
        self.utc.map(|u| u.to_rfc3339())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampConfidence {
    /// Absolute instant with an explicit UTC offset, from the source itself.
    Exact,
    /// Absolute instant derived from a provider-encoded epoch value.
    ProviderDerived,
    /// Absolute instant taken from a database column (epoch seconds/millis or text).
    DatabaseDerived,
    /// Absolute instant taken from the filesystem, not from the record.
    FilesystemDerived,
    /// Only ordering within the source is known.
    SequenceOnly,
    /// Nothing usable was found.
    Unknown,
}

impl TimestampConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            TimestampConfidence::Exact => "exact",
            TimestampConfidence::ProviderDerived => "provider_derived",
            TimestampConfidence::DatabaseDerived => "database_derived",
            TimestampConfidence::FilesystemDerived => "filesystem_derived",
            TimestampConfidence::SequenceOnly => "sequence_only",
            TimestampConfidence::Unknown => "unknown",
        }
    }
}

/// Days from civil date (Howard Hinnant's algorithm), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an RFC 3339 / ISO 8601 timestamp that carries an explicit offset.
///
/// Accepts `Z`, `+HH:MM`, `-HH:MM`, fractional seconds and a space instead of
/// `T`. A timestamp without an offset (naive local time) is intentionally
/// rejected: guessing a timezone would fabricate history.
pub fn parse_rfc3339(input: &str) -> Option<Utc> {
    let s = input.trim();
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    if !(b[4] == b'-' && b[7] == b'-') {
        return None;
    }
    if !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    // `2026-02-30` must not roll over into March: that would invent a date.
    if day > month_days {
        return None;
    }
    let mut idx = 19;
    let mut nanos: i64 = 0;
    if b.get(idx) == Some(&b'.') || b.get(idx) == Some(&b',') {
        idx += 1;
        let start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        let frac = s.get(start..idx)?;
        let digits: String = frac.chars().take(9).collect();
        let scale = 10i64.pow(9 - digits.len() as u32);
        nanos = digits.parse::<i64>().ok()? * scale;
    }
    let rest = s.get(idx..)?.trim();
    let offset_secs = match rest {
        // A missing offset is NOT "UTC": it falls through to `_`, which
        // rejects it, so a naive local time is never guessed into existence.
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.chars().next()? {
                '+' => 1i64,
                '-' => -1i64,
                _ => return None,
            };
            let body = rest.get(1..)?;
            let (h, m) = if let Some((h, m)) = body.split_once(':') {
                (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
            } else if body.len() == 4 {
                (
                    body.get(0..2)?.parse::<i64>().ok()?,
                    body.get(2..4)?.parse::<i64>().ok()?,
                )
            } else if body.len() == 2 {
                (body.parse::<i64>().ok()?, 0)
            } else {
                return None;
            };
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    // `Utc` is i64 nanoseconds (years ~1677..2262); anything outside is
    // rejected rather than wrapped into a different instant.
    Some(Utc(secs.checked_mul(1_000_000_000)?.checked_add(nanos)?))
}

/// Interpret a value that may be epoch seconds, epoch milliseconds, epoch
/// microseconds, or a string timestamp. Used for database columns.
pub fn parse_epoch_like(value: i64) -> Option<(Utc, TimestampConfidence)> {
    let abs = value.unsigned_abs();
    let unit = if abs < 10_000_000_000 {
        1_000_000_000
    } else if abs < 10_000_000_000_000 {
        1_000_000
    } else if abs < 10_000_000_000_000_000 {
        1_000
    } else {
        1
    };
    Some((
        Utc(value.checked_mul(unit)?),
        TimestampConfidence::DatabaseDerived,
    ))
}

/// Parse a JSON value that is either a number (epoch-ish) or a string
/// (RFC 3339 or numeric text). Returns `None` when nothing usable is found.
pub fn parse_json_timestamp(value: &serde_json::Value) -> Option<(Utc, TimestampConfidence)> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                parse_epoch_like(i)
            } else {
                let f = n.as_f64()?;
                if f.abs() < 1e11 {
                    Some((Utc((f * 1e9) as i64), TimestampConfidence::ProviderDerived))
                } else {
                    Some((Utc((f * 1e6) as i64), TimestampConfidence::ProviderDerived))
                }
            }
        }
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            if let Some(u) = parse_rfc3339(t) {
                return Some((u, TimestampConfidence::Exact));
            }
            if let Ok(i) = t.parse::<i64>() {
                return parse_epoch_like(i);
            }
            None
        }
        _ => None,
    }
}

/// Best-effort timestamp from a JSON object, trying common field names in order.
pub fn from_json_fields(
    obj: &serde_json::Map<String, serde_json::Value>,
    fields: &[&str],
) -> Option<(Utc, TimestampConfidence)> {
    for f in fields {
        if let Some(v) = obj.get(*f) {
            if v.is_null() {
                continue;
            }
            if let Some(hit) = parse_json_timestamp(v) {
                return Some(hit);
            }
        }
    }
    None
}

pub fn now_utc() -> Utc {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Utc(d.as_nanos() as i64)
}

pub fn from_system_time(t: std::time::SystemTime) -> Option<Utc> {
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(Utc(d.as_nanos() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impossible_dates_and_out_of_range_years_are_rejected() {
        assert!(
            parse_rfc3339("2026-02-30T00:00:00Z").is_none(),
            "no roll-over into March"
        );
        assert!(parse_rfc3339("2025-02-29T00:00:00Z").is_none());
        assert!(parse_rfc3339("2024-02-29T00:00:00Z").is_some());
        assert!(parse_rfc3339("2026-04-31T00:00:00Z").is_none());
        assert!(
            parse_rfc3339("9999-01-01T00:00:00Z").is_none(),
            "beyond i64 ns, not wrapped"
        );
        assert!(parse_rfc3339("0001-01-01T00:00:00Z").is_none());
    }

    #[test]
    fn parses_z_and_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z").unwrap().0, 0);
        assert_eq!(
            parse_rfc3339("2026-09-28T02:31:40.214Z")
                .unwrap()
                .to_rfc3339(),
            "2026-09-28T02:31:40.214000000Z"
        );
        let a = parse_rfc3339("2026-08-20T11:59:10-03:00").unwrap();
        let b = parse_rfc3339("2026-08-20T14:59:10Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(
            parse_rfc3339("2026-01-02 03:04:05+0000"),
            parse_rfc3339("2026-01-02T03:04:05Z")
        );
    }

    #[test]
    fn rejects_naive_local_time() {
        assert!(parse_rfc3339("2026-09-28T02:31:40").is_none());
        assert!(parse_rfc3339("2026-13-40T00:00:00Z").is_none());
        assert!(parse_rfc3339("").is_none());
        assert!(parse_rfc3339("not a date").is_none());
    }

    #[test]
    fn roundtrip_days() {
        for (y, m, d) in [
            (1970i64, 1u32, 1u32),
            (2026, 10, 6),
            (2099, 12, 31),
            (1969, 7, 20),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
    }

    #[test]
    fn epochs() {
        assert_eq!(
            parse_epoch_like(1790562659).unwrap().0 .0,
            1_790_562_659_000_000_000
        );
        assert_eq!(
            parse_epoch_like(1_790_562_659_000).unwrap().0 .0,
            1_790_562_659_000_000_000
        );
        assert_eq!(parse_epoch_like(i64::MAX).unwrap().0 .0, i64::MAX);
    }
}
