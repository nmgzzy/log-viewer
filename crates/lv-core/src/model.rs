//! 统一日志数据模型（与需求 §3 对齐）。
//!
//! 内存布局以紧凑为先：每条记录固定大小的 `RecordMeta`（≈56 字节），
//! 原始行存于 Arena，`msg` 是 raw 内的字节区间（解析结果总是 raw 的子串），
//! host/app/tag 驻留为符号 id。

/// Arena 中一段字节的引用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanRef {
    pub offset: u64,
    pub len: u32,
}

pub const PID_NONE: u32 = u32::MAX;

/// syslog 8 级 severity 名（索引即数值）。
pub const LEVEL_NAMES: [&str; 8] = [
    "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
];

/// 常见 facility 名。
pub const FACILITY_NAMES: [&str; 24] = [
    "kern", "user", "mail", "daemon", "auth", "syslog", "lpr", "news", "uucp", "cron", "authpriv",
    "ftp", "ntp", "audit", "alert", "clockd", "local0", "local1", "local2", "local3", "local4",
    "local5", "local6", "local7",
];

pub fn level_name(level: u8) -> &'static str {
    LEVEL_NAMES.get(level as usize).copied().unwrap_or("?")
}

pub fn facility_name(fac: u8) -> &'static str {
    FACILITY_NAMES.get(fac as usize).copied().unwrap_or("?")
}

/// 按名字解析 level（容忍常见别名）。
pub fn level_from_name(s: &str) -> Option<u8> {
    let v = match s {
        "emerg" | "panic" | "EMERG" => 0,
        "alert" | "ALERT" => 1,
        "crit" | "critical" | "CRIT" | "CRITICAL" | "fatal" | "FATAL" => 2,
        "err" | "error" | "ERR" | "ERROR" => 3,
        "warning" | "warn" | "WARNING" | "WARN" => 4,
        "notice" | "NOTICE" => 5,
        "info" | "INFO" => 6,
        "debug" | "DEBUG" | "trace" | "TRACE" => 7,
        _ => return None,
    };
    Some(v)
}

pub mod flags {
    /// 该行被某个解析器成功解析。
    pub const PARSED: u8 = 1 << 0;
    /// 时间戳并非来自日志内容（接收时间 / 邻行推断），仅用于排序。
    pub const TS_SYNTHETIC: u8 = 1 << 1;
}

/// 存储中的一条记录（定长部分）。
#[derive(Clone, Copy, Debug)]
pub struct RecordMeta {
    /// epoch 微秒（UTC）。
    pub ts: i64,
    /// 原始时区偏移（分钟），用于"原始时区"显示模式。
    pub tz_offset_min: i16,
    /// 来源 id（store 内的 sources 下标）。
    pub source: u16,
    pub host: u32,
    pub app: u32,
    pub tag: u32,
    pub pid: u32,
    pub level: u8,
    pub facility: u8,
    pub flags: u8,
    /// 原始行。
    pub raw: SpanRef,
    /// msg 正文。通常指向 raw 内的子区间；JSON 行解码后的 msg 是独立追加的串。
    pub msg: SpanRef,
}

impl RecordMeta {
    pub fn is_parsed(&self) -> bool {
        self.flags & flags::PARSED != 0
    }
    pub fn ts_is_synthetic(&self) -> bool {
        self.flags & flags::TS_SYNTHETIC != 0
    }
}

/// 解析器输出的中间结构：所有字符串借用自原始行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine<'a> {
    /// epoch 微秒（UTC）；None 表示该行没有可用时间戳。
    pub ts: Option<i64>,
    pub tz_offset_min: i16,
    pub host: &'a str,
    pub app: &'a str,
    pub tag: &'a str,
    pub pid: Option<u32>,
    pub level: u8,
    pub facility: u8,
    /// msg 在原始行内的字节区间 [start, end)。
    pub msg_range: (usize, usize),
    /// 当 msg 不是 raw 的子串时（JSON 转义解码），存放解码后的正文；
    /// 存储时优先于 msg_range。
    pub msg_owned: Option<String>,
    /// 是否解析成功；false 时整行按 raw 展示并标记"未解析"。
    pub parsed: bool,
}

impl<'a> ParsedLine<'a> {
    /// 解析失败的回退：整行即 msg。
    pub fn unparsed(raw: &'a str) -> Self {
        ParsedLine {
            ts: None,
            tz_offset_min: 0,
            host: "",
            app: "",
            tag: "",
            pid: None,
            level: 6,
            facility: 1,
            msg_range: (0, raw.len()),
            msg_owned: None,
            parsed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_meta_stays_compact() {
        // 容量基线依赖紧凑布局：1M 行的定长部分应在 ~56MB 量级
        assert!(
            std::mem::size_of::<RecordMeta>() <= 64,
            "RecordMeta 过大: {}",
            std::mem::size_of::<RecordMeta>()
        );
    }

    #[test]
    fn level_names_roundtrip() {
        for (i, name) in LEVEL_NAMES.iter().enumerate() {
            assert_eq!(level_from_name(name), Some(i as u8));
        }
        assert_eq!(level_from_name("error"), Some(3));
        assert_eq!(level_from_name("warn"), Some(4));
        assert_eq!(level_from_name("bogus"), None);
    }
}
