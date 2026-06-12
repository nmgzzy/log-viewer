//! 单元格格式化：时间显示模式（绝对/相对、原始/本地时区）等（FR-5）。

use lv_core::model::{RecordMeta, PID_NONE};
use lv_core::parse::ts::format_rfc3339_ms;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum TimeMode {
    AbsOriginal,
    AbsLocal,
    RelFirst,
    RelPrev,
}

impl Default for TimeMode {
    fn default() -> Self {
        TimeMode::AbsOriginal
    }
}

/// 本机时区偏移（分钟）。
pub fn local_tz_offset_min() -> i16 {
    use chrono::Offset;
    (chrono::Local::now().offset().fix().local_minus_utc() / 60) as i16
}

/// 相对时间：+12.345s / +1m02.500s / +2h03m04s。
fn fmt_delta_us(delta: i64) -> String {
    let sign = if delta < 0 { "-" } else { "+" };
    let d = delta.unsigned_abs();
    let ms = (d / 1000) % 1000;
    let s = d / 1_000_000;
    if s >= 3600 {
        format!("{sign}{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
    } else if s >= 60 {
        format!("{sign}{}m{:02}.{:03}s", s / 60, s % 60, ms)
    } else {
        format!("{sign}{}.{:03}s", s, ms)
    }
}

pub fn format_time(
    m: &RecordMeta,
    mode: TimeMode,
    first_ts: i64,
    prev_ts: Option<i64>,
    local_off_min: i16,
) -> String {
    let mark = if m.ts_is_synthetic() { "~" } else { "" };
    match mode {
        TimeMode::AbsOriginal => format!("{mark}{}", format_rfc3339_ms(m.ts, m.tz_offset_min)),
        TimeMode::AbsLocal => format!("{mark}{}", format_rfc3339_ms(m.ts, local_off_min)),
        TimeMode::RelFirst => format!("{mark}{}", fmt_delta_us(m.ts - first_ts)),
        TimeMode::RelPrev => match prev_ts {
            Some(p) => format!("{mark}{}", fmt_delta_us(m.ts - p)),
            None => format!("{mark}+0.000s"),
        },
    }
}

pub fn format_pid(m: &RecordMeta) -> String {
    if m.pid == PID_NONE {
        String::new()
    } else {
        m.pid.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_formats() {
        assert_eq!(fmt_delta_us(12_345_000), "+12.345s");
        assert_eq!(fmt_delta_us(-500_000), "-0.500s");
        assert_eq!(fmt_delta_us(62_500_000), "+1m02.500s");
        assert_eq!(fmt_delta_us(7_384_000_000), "+2h03m04s");
    }
}
