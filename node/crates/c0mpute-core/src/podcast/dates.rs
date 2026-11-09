//! The two date formats podcast feeds use, without a date crate: RFC 2822
//! (`pubDate`, written and read) and RFC 3339 / plain `YYYY-MM-DD` (what a
//! publisher types into `episodes.toml`).

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `Wed, 04 Jun 2026 12:00:00 +0000` for a Unix time.
pub fn fmt_rfc2822(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        DAYS[days.rem_euclid(7) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// `YYYY-MM-DD` (UTC) for a Unix time.
pub fn fmt_ymd(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn zone_offset_secs(z: &str) -> Option<i64> {
    let z = z.trim();
    match z.to_ascii_uppercase().as_str() {
        "" | "Z" | "GMT" | "UT" | "UTC" => return Some(0),
        "EST" => return Some(-5 * 3600),
        "EDT" => return Some(-4 * 3600),
        "CST" => return Some(-6 * 3600),
        "CDT" => return Some(-5 * 3600),
        "MST" => return Some(-7 * 3600),
        "MDT" => return Some(-6 * 3600),
        "PST" => return Some(-8 * 3600),
        "PDT" => return Some(-7 * 3600),
        _ => {}
    }
    let (sign, rest) = match z.as_bytes().first()? {
        b'+' => (1, &z[1..]),
        b'-' => (-1, &z[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 4 {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

fn hms(s: &str) -> Option<i64> {
    let mut parts = s.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec: i64 = match parts.next() {
        Some(x) => x.split('.').next()?.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some(h * 3600 + m * 60 + sec)
}

/// Parse an RFC 2822 date as feeds write it, leniently: the weekday is
/// optional, seconds are optional, two-digit years and common US zone names
/// are accepted.
pub fn parse_rfc2822(s: &str) -> Option<i64> {
    let s = s.trim();
    let s = match s.find(',') {
        Some(i) => &s[i + 1..],
        None => s,
    };
    let mut it = s.split_whitespace();
    let d: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let m = MONTHS
        .iter()
        .position(|x| mon.len() >= 3 && x.eq_ignore_ascii_case(&mon[..3]))? as u32
        + 1;
    let mut y: i64 = it.next()?.parse().ok()?;
    if y < 100 {
        y += if y < 50 { 2000 } else { 1900 };
    }
    let t = hms(it.next()?)?;
    let off = zone_offset_secs(it.next().unwrap_or(""))?;
    if !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + t - off)
}

/// Parse `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS][Z|+HH:MM]` or an RFC 2822 date.
pub fn parse_date_input(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        let y: i64 = s[..4].parse().ok()?;
        let m: u32 = s[5..7].parse().ok()?;
        let d: u32 = s[8..10].parse().ok()?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let day = days_from_civil(y, m, d) * 86_400;
        let rest = &s[10..];
        if rest.is_empty() {
            return Some(day);
        }
        let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
        let zone_at = rest
            .find(|c: char| c == 'Z' || c == 'z' || c == '+' || c == '-')
            .unwrap_or(rest.len());
        let t = hms(&rest[..zone_at])?;
        let off = zone_offset_secs(&rest[zone_at..])?;
        return Some(day + t - off);
    }
    parse_rfc2822(s)
}

/// `HH:MM:SS` (what `itunes:duration` most often holds).
pub fn fmt_duration(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// `itunes:duration`: plain seconds, `MM:SS` or `HH:MM:SS`.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let mut total = 0u64;
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    for p in parts {
        let n: f64 = p.parse().ok()?;
        if !n.is_finite() || n < 0.0 {
            return None;
        }
        total = total * 60 + n as u64;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_rfc2822() {
        // 2026-06-04T12:00:00Z
        let t = days_from_civil(2026, 6, 4) * 86_400 + 12 * 3600;
        assert_eq!(fmt_rfc2822(t), "Thu, 04 Jun 2026 12:00:00 +0000");
        assert_eq!(parse_rfc2822("Thu, 04 Jun 2026 12:00:00 +0000"), Some(t));
        assert_eq!(parse_rfc2822("Thu, 04 Jun 2026 12:00:00 GMT"), Some(t));
        assert_eq!(parse_rfc2822("Thu, 4 Jun 2026 08:00:00 EDT"), Some(t));
        assert_eq!(parse_rfc2822("04 June 2026 07:00 -0500"), Some(t));
        assert_eq!(parse_rfc2822("Thu, 04 Jun 26 14:00:00 +0200"), Some(t));
        assert_eq!(parse_rfc2822("not a date"), None);
        assert_eq!(fmt_rfc2822(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        for x in [0i64, 951_782_400, 1_780_000_000, 4_102_444_800] {
            assert_eq!(parse_rfc2822(&fmt_rfc2822(x)), Some(x));
        }
    }

    #[test]
    fn parses_typed_dates() {
        let day = days_from_civil(2026, 6, 4) * 86_400;
        assert_eq!(parse_date_input("2026-06-04"), Some(day));
        assert_eq!(parse_date_input("2026-06-04T12:00:00Z"), Some(day + 43_200));
        assert_eq!(parse_date_input("2026-06-04T14:00+02:00"), Some(day + 43_200));
        assert_eq!(parse_date_input("2026-13-04"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3612"), Some(3612));
        assert_eq!(parse_duration("01:00:12"), Some(3612));
        assert_eq!(parse_duration("60:12"), Some(3612));
        assert_eq!(parse_duration("3612.6"), Some(3612));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("a:b"), None);
        assert_eq!(fmt_duration(3612), "01:00:12");
    }
}
