//! Parsing / formatting of user-typed values.
//! QUALITY GATE: nothing in here panics or unwraps on user input; every parser returns
//! `Result<_, String>` and the UI shows the message as a non-blocking toast.

use crate::model::{DepKind, TaskId};
use chrono::NaiveDate;

const MAX_DAYS: f64 = 36_500.0;

/// "5", "5d", "2.5d", "12h", "2w" -> working minutes. Bare numbers are days.
pub fn parse_duration(s: &str, mpd: i64) -> Result<i64, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("Duration is empty (try 5d, 12h or 2w)".into());
    }
    let (num, unit) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&s[..s.len() - 1], c),
        _ => (&s[..], 'd'),
    };
    let v: f64 = num.trim().parse().map_err(|_| format!("'{}' is not a valid duration (try 5d, 12h or 2w)", s))?;
    if !v.is_finite() || v < 0.0 {
        return Err("Duration must be a non-negative number".into());
    }
    let days = match unit {
        'd' => v,
        'h' => v * 60.0 / mpd as f64,
        'w' => v * 5.0,
        _ => return Err(format!("Unknown duration unit '{}': use d, h or w", unit)),
    };
    if days > MAX_DAYS {
        return Err("Duration is too large".into());
    }
    Ok((days * mpd as f64).round() as i64)
}

pub fn format_duration(min: i64, mpd: i64) -> String {
    if mpd <= 0 {
        return format!("{}m", min);
    }
    if min == 0 {
        "0d".into()
    } else if min % mpd == 0 {
        format!("{}d", min / mpd)
    } else if min < mpd && min % 60 == 0 {
        format!("{}h", min / 60)
    } else {
        let d = min as f64 / mpd as f64;
        let s = format!("{:.2}", d);
        format!("{}d", s.trim_end_matches('0').trim_end_matches('.'))
    }
}

/// Signed lag such as "+3d" / "-2d" / "3d" / "" (=0).
pub fn parse_lag(s: &str, mpd: i64) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    let (sign, rest) = match s.chars().next() {
        Some('-') => (-1, &s[1..]),
        Some('+') => (1, &s[1..]),
        _ => (1, s),
    };
    Ok(sign * parse_duration(rest, mpd)?)
}

pub fn format_lag(lag_min: i64, mpd: i64) -> String {
    if lag_min == 0 {
        String::new()
    } else if lag_min > 0 {
        format!("+{}", format_duration(lag_min, mpd))
    } else {
        format!("-{}", format_duration(-lag_min, mpd))
    }
}

pub fn parse_date(s: &str) -> Result<NaiveDate, String> {
    let s = s.trim();
    for fmt in ["%Y-%m-%d", "%d/%m/%Y", "%m/%d/%Y", "%d %b %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d);
        }
    }
    Err(format!("'{}' is not a valid date (use YYYY-MM-DD)", s))
}

pub fn format_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// "3FS+2d, 5, 7SS-1d"  ->  [(3, FS, +2d), (5, FS, 0), (7, SS, -1d)]
pub fn parse_predecessors(s: &str, mpd: i64) -> Result<Vec<(TaskId, DepKind, i64)>, String> {
    let mut out = vec![];
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return Err(format!("'{}': a predecessor must start with a task ID", part));
        }
        let id: TaskId = digits.parse().map_err(|_| format!("'{}': task ID is too large", digits))?;
        let rest = part[digits.len()..].trim();
        let (kind, lag_str) = if rest.len() >= 2 && rest.is_char_boundary(2) && rest[..2].chars().all(|c| c.is_ascii_alphabetic()) {
            let k = DepKind::parse(&rest[..2]).ok_or_else(|| format!("'{}': unknown link type '{}' (FS, SS, FF, SF)", part, &rest[..2]))?;
            (k, &rest[2..])
        } else {
            (DepKind::FS, rest)
        };
        let lag = parse_lag(lag_str, mpd).map_err(|e| format!("'{}': {}", part, e))?;
        out.push((id, kind, lag));
    }
    Ok(out)
}

pub fn format_link(pred: TaskId, kind: DepKind, lag_min: i64, mpd: i64) -> String {
    let k = if kind == DepKind::FS && lag_min == 0 { String::new() } else { kind.as_str().to_string() };
    format!("{}{}{}", pred, k, format_lag(lag_min, mpd))
}

/// "Alice[50%], Bob" -> [("Alice",0.5),("Bob",1.0)]
pub fn parse_resources(s: &str) -> Result<Vec<(String, f64)>, String> {
    let mut out = vec![];
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(open) = part.find('[') {
            let name = part[..open].trim();
            let inner = part[open + 1..].trim_end().strip_suffix(']').ok_or_else(|| format!("'{}': missing ']'", part))?;
            let pct = inner.trim().trim_end_matches('%').trim();
            let v: f64 = pct.parse().map_err(|_| format!("'{}': units must be a percentage like 50%", part))?;
            if !v.is_finite() || v <= 0.0 || v > 10_000.0 {
                return Err(format!("'{}': units must be between 1% and 10000%", part));
            }
            if name.is_empty() {
                return Err("Resource name is empty".into());
            }
            out.push((name.to_string(), v / 100.0));
        } else {
            out.push((part.to_string(), 1.0));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MPD: i64 = 480;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5", MPD), Ok(5 * 480));
        assert_eq!(parse_duration("2.5d", MPD), Ok(1200));
        assert_eq!(parse_duration("12h", MPD), Ok(720));
        assert_eq!(parse_duration("2w", MPD), Ok(10 * 480));
        assert!(parse_duration("", MPD).is_err());
        assert!(parse_duration("abc", MPD).is_err());
        assert!(parse_duration("-3d", MPD).is_err());
        assert!(parse_duration("5x", MPD).is_err());
        assert!(parse_duration("1e99d", MPD).is_err());
        assert!(parse_duration("NaN", MPD).is_err());
        assert_eq!(format_duration(960, MPD), "2d");
        assert_eq!(format_duration(1200, MPD), "2.5d");
        assert_eq!(format_duration(240, MPD), "4h");
    }

    #[test]
    fn lags() {
        assert_eq!(parse_lag("+3d", MPD), Ok(1440));
        assert_eq!(parse_lag("-1d", MPD), Ok(-480));
        assert_eq!(parse_lag("", MPD), Ok(0));
        assert_eq!(format_lag(-480, MPD), "-1d");
    }

    #[test]
    fn predecessors() {
        let v = parse_predecessors("3FS+2d, 5, 7ss-1d", MPD).unwrap();
        assert_eq!(v, vec![(3, DepKind::FS, 960), (5, DepKind::FS, 0), (7, DepKind::SS, -480)]);
        assert!(parse_predecessors("FS", MPD).is_err());
        assert!(parse_predecessors("3XX", MPD).is_err());
        assert!(parse_predecessors("3FS+zz", MPD).is_err());
        assert!(parse_predecessors("99999999999999999999", MPD).is_err());
        assert_eq!(parse_predecessors("", MPD).unwrap(), vec![]);
        // multi-byte garbage must not panic on slicing
        assert!(parse_predecessors("3é", MPD).is_err());
    }

    #[test]
    fn resources() {
        let v = parse_resources("Alice[50%], Bob").unwrap();
        assert_eq!(v, vec![("Alice".to_string(), 0.5), ("Bob".to_string(), 1.0)]);
        assert!(parse_resources("Alice[abc]").is_err());
        assert!(parse_resources("Alice[50%").is_err());
        assert!(parse_resources("[50%]").is_err());
    }

    #[test]
    fn dates() {
        assert!(parse_date("2026-10-24").is_ok());
        assert!(parse_date("nope").is_err());
        assert!(parse_date("2026-13-40").is_err());
    }
}
