//! Numbers, units and times as a person writes them.

use chrono::{DateTime, Datelike, Utc};

/// A number with its unit, trimmed of the noise a float carries: `16` not
/// `16.0`, `10.6` not `10.600000381`, `1,540` with a separator.
pub fn num(v: f64, decimals: usize) -> String {
    let rounded = format!("{v:.decimals$}");
    // Drop trailing zeros after the point, and the point itself.
    let rounded = if rounded.contains('.') {
        rounded.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        rounded
    };
    let (int, frac) = match rounded.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (rounded, None),
    };
    let negative = int.starts_with('-');
    let digits = int.trim_start_matches('-');
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    if let Some(f) = frac {
        out.push('.');
        out.push_str(&f);
    }
    out
}

pub fn unit(v: f64, decimals: usize, unit: &str) -> String {
    format!("{} {unit}", num(v, decimals))
}

pub fn degrees(v: f64) -> String {
    format!("{}°", num(v, 0))
}

pub fn celsius(v: f64) -> String {
    format!("{} °C", num(v, 1))
}

/// A compass point for a bearing.
pub fn compass(deg: f64) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW", "NW",
        "NNW",
    ];
    let i = ((deg.rem_euclid(360.0) + 11.25) / 22.5) as usize % 16;
    POINTS[i]
}

/// `240° (WSW)`.
pub fn bearing(deg: f64) -> String {
    format!("{}° ({})", num(deg, 0), compass(deg))
}

/// Metres per second, with knots beside it because half the layers that
/// report wind report it in knots.
pub fn wind_ms(ms: f64) -> String {
    format!("{} m/s ({} kt)", num(ms, 1), num(ms * 1.943_844, 0))
}

pub fn knots(kt: f64) -> String {
    format!("{} kt ({} km/h)", num(kt, 0), num(kt * 1.852, 0))
}

pub fn metres(m: f64) -> String {
    if m.abs() >= 10_000.0 {
        format!("{} km", num(m / 1000.0, 1))
    } else {
        format!("{} m", num(m, 0))
    }
}

pub fn feet(ft: f64) -> String {
    format!("{} ft ({} m)", num(ft, 0), num(ft * 0.3048, 0))
}

/// A time the way a person reads one: `16 Sep 12:00 UTC`. Accepts RFC 3339
/// and the bare `Z` forms the feeds use, and Unix seconds as text.
pub fn when(s: &str) -> Option<String> {
    parse_time(s).map(|t| {
        // A ground station registered in December 2019 must not read as
        // "21 Dec", which is last December to anyone reading it.
        if t.year() == Utc::now().year() {
            t.format("%-d %b %H:%M UTC").to_string()
        } else {
            t.format("%-d %b %Y %H:%M UTC").to_string()
        }
    })
}

pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = s.parse::<DateTime<Utc>>() {
        return Some(t);
    }
    if let Ok(secs) = s.parse::<i64>() {
        return DateTime::from_timestamp(secs, 0);
    }
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(t.and_utc());
    }
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%MZ") {
        return Some(t.and_utc());
    }
    None
}

/// `16 Sep 12:00 – 16:00 UTC`, or one end if that is all there is.
pub fn span(from: Option<&str>, to: Option<&str>) -> Option<String> {
    match (from.and_then(parse_time), to.and_then(parse_time)) {
        (Some(a), Some(b)) => Some(if a.date_naive() == b.date_naive() {
            format!("{} – {} UTC", a.format("%-d %b %H:%M"), b.format("%H:%M"))
        } else {
            format!("{} – {} UTC", a.format("%-d %b %H:%M"), b.format("%-d %b %H:%M"))
        }),
        (Some(a), None) => Some(format!("from {} UTC", a.format("%-d %b %H:%M"))),
        (None, Some(b)) => Some(format!("until {} UTC", b.format("%-d %b %H:%M"))),
        (None, None) => None,
    }
}

/// `2 h 15 min`, `40 s`, `3 days`.
pub fn duration(secs: i64) -> String {
    let s = secs.abs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
    } else {
        let d = s / 86_400;
        let h = (s % 86_400) / 3600;
        if h == 0 { format!("{d} days") } else { format!("{d} days {h} h") }
    }
}

/// `snake_case_key` → `Snake case key`.
pub fn words(key: &str) -> String {
    let mut s = key.replace('_', " ");
    if let Some(first) = s.get(..1) {
        let upper = first.to_uppercase();
        s.replace_range(..1, &upper);
    }
    s
}

/// A code with a decoded meaning: `Severe (SEV)` when the code adds
/// something, else just the words.
pub fn decoded(words: &str, code: &str) -> String {
    if words.eq_ignore_ascii_case(code) {
        words.to_string()
    } else {
        format!("{words} ({code})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_like_a_person_wrote_them() {
        assert_eq!(num(16.0, 1), "16");
        assert_eq!(num(10.600000381, 1), "10.6");
        assert_eq!(num(1540.0, 0), "1,540");
        assert_eq!(num(-1234567.891, 2), "-1,234,567.89");
        assert_eq!(num(0.005, 3), "0.005");
    }

    #[test]
    fn bearings_get_a_compass_point() {
        assert_eq!(bearing(240.0), "240° (WSW)");
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(359.0), "N");
        assert_eq!(compass(180.0), "S");
    }

    #[test]
    fn every_time_form_the_feeds_use_parses() {
        assert_eq!(when("2026-09-16T12:00:00Z").as_deref(), Some("16 Sep 12:00 UTC"));
        assert_eq!(when("2026-09-16T12:00:00+00:00").as_deref(), Some("16 Sep 12:00 UTC"));
        assert_eq!(when("1789560000").as_deref(), Some("16 Sep 12:00 UTC"));
        assert_eq!(when("2026-09-16T12:00Z").as_deref(), Some("16 Sep 12:00 UTC"));
        assert_eq!(when("nonsense"), None);
    }

    #[test]
    fn spans_and_durations() {
        assert_eq!(
            span(Some("2026-09-16T12:00:00Z"), Some("2026-09-16T16:00:00Z")).as_deref(),
            Some("16 Sep 12:00 – 16:00 UTC")
        );
        assert_eq!(duration(40), "40 s");
        assert_eq!(duration(8100), "2 h 15 min");
        assert_eq!(duration(3 * 86_400), "3 days");
    }
}
