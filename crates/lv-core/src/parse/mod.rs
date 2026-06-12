//! 解析层：把一行原始文本解析为 `ParsedLine`。
//!
//! 解析失败绝不丢行——`parse_auto` 总是返回结果，失败时回退为
//! `ParsedLine::unparsed`（FR-P5）。各格式解析器独立可测。

pub mod detect;
pub mod jsonline;
pub mod rfc3164;
pub mod rfc5424;
pub mod ts;
pub mod uf_file;

pub use detect::parse_auto;
pub use jsonline::FieldMap;

/// 解析上下文：为缺失信息的格式（RFC3164 无年份/时区）提供默认值。
#[derive(Clone, Debug)]
pub struct ParserCtx {
    /// RFC3164 时间戳没有年份，用此年补全。
    pub default_year: i32,
    /// 无时区信息的时间戳按此偏移（分钟）解释，一般传本机时区。
    pub default_tz_offset_min: i16,
    /// JSON 行字段映射。
    pub json_map: FieldMap,
}

impl Default for ParserCtx {
    fn default() -> Self {
        Self {
            default_year: 2026,
            default_tz_offset_min: 0,
            json_map: FieldMap::default(),
        }
    }
}

impl ParserCtx {
    /// 以当前本机时间初始化（年份与时区）。
    pub fn from_local_now() -> Self {
        use chrono::{Datelike, Local, Offset};
        let now = Local::now();
        Self {
            default_year: now.year(),
            default_tz_offset_min: (now.offset().fix().local_minus_utc() / 60) as i16,
            json_map: FieldMap::default(),
        }
    }
}
