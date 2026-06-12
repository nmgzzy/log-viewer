//! 时间戳解析辅助：RFC3339 / ISO8601（含小数秒与时区偏移）。

use chrono::{DateTime, FixedOffset};

/// 解析 RFC3339/ISO8601 时间戳，返回 (epoch 微秒 UTC, 时区偏移分钟)。
/// 支持 `2026-06-12T10:16:41.834Z`、`2026-06-12T10:16:41.834+00:00`、
/// 无小数秒等变体。
pub fn parse_rfc3339(s: &str) -> Option<(i64, i16)> {
    let dt: DateTime<FixedOffset> = DateTime::parse_from_rfc3339(s).ok()?;
    let offset_min = (dt.offset().local_minus_utc() / 60) as i16;
    Some((dt.timestamp_micros(), offset_min))
}

/// 取出字符串开头形如时间戳的 token 长度（首个空格前），并验证可解析。
pub fn leading_rfc3339(s: &str) -> Option<(usize, i64, i16)> {
    let end = s.find(' ').unwrap_or(s.len());
    let (us, off) = parse_rfc3339(&s[..end])?;
    Some((end, us, off))
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn month_from_abbrev(s: &str) -> Option<u32> {
    MONTHS.iter().position(|m| m.eq_ignore_ascii_case(s)).map(|i| i as u32 + 1)
}

/// 由本地日期时间分量构造 epoch 微秒（按给定偏移解释）。
pub fn micros_from_parts(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    tz_offset_min: i16,
) -> Option<i64> {
    use chrono::{NaiveDate, NaiveDateTime};
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let ndt: NaiveDateTime = date.and_hms_opt(hour, min, sec)?;
    let local_us = ndt.and_utc().timestamp_micros();
    Some(local_us - tz_offset_min as i64 * 60 * 1_000_000)
}

/// 将 epoch 微秒格式化为 RFC3339（毫秒精度），按给定偏移显示。
pub fn format_rfc3339_ms(ts_us: i64, tz_offset_min: i16) -> String {
    use chrono::TimeZone;
    let offset = FixedOffset::east_opt(tz_offset_min as i32 * 60)
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    match offset.timestamp_micros(ts_us) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        _ => String::from("-"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_with_millis_utc() {
        let (us, off) = parse_rfc3339("2026-06-12T10:16:41.834Z").unwrap();
        assert_eq!(off, 0);
        assert_eq!(us % 1_000_000, 834_000);
        let back = format_rfc3339_ms(us, 0);
        assert_eq!(back, "2026-06-12T10:16:41.834+00:00");
    }

    #[test]
    fn rfc3339_with_offset() {
        let (us_a, off_a) = parse_rfc3339("2026-06-12T18:16:41.834+08:00").unwrap();
        let (us_b, off_b) = parse_rfc3339("2026-06-12T10:16:41.834Z").unwrap();
        assert_eq!(us_a, us_b); // 同一时刻
        assert_eq!(off_a, 480);
        assert_eq!(off_b, 0);
    }

    #[test]
    fn rfc3339_no_fraction() {
        let (us, _) = parse_rfc3339("2026-06-12T10:16:41+00:00").unwrap();
        assert_eq!(us % 1_000_000, 0);
    }

    #[test]
    fn rfc3339_microseconds() {
        let (us, _) = parse_rfc3339("2026-06-12T10:16:41.123456Z").unwrap();
        assert_eq!(us % 1_000_000, 123_456);
    }

    #[test]
    fn invalid_rejected() {
        assert!(parse_rfc3339("not-a-date").is_none());
        assert!(parse_rfc3339("2026-13-40T99:99:99Z").is_none());
    }

    #[test]
    fn parts_respect_offset() {
        let a = micros_from_parts(2026, 6, 12, 18, 0, 0, 480).unwrap();
        let b = micros_from_parts(2026, 6, 12, 10, 0, 0, 0).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn month_abbrev() {
        assert_eq!(month_from_abbrev("Jun"), Some(6));
        assert_eq!(month_from_abbrev("dec"), Some(12));
        assert_eq!(month_from_abbrev("Foo"), None);
    }
}
