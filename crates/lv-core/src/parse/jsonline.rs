//! JSON 行解析（FR-P4）：每行一个 JSON 对象，字段名可映射配置。
//!
//! 默认字段名与统一模型一致：ts/host/app/pid/level/tag/msg。
//! `ts` 接受 RFC3339 字符串或 epoch 数字（按数量级自动识别 秒/毫秒/微秒）。
//! `level` 接受 0–7 数字或 syslog 级别名。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{level_from_name, ParsedLine};
use crate::parse::ts::parse_rfc3339;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct FieldMap {
    pub ts: String,
    pub host: String,
    pub app: String,
    pub pid: String,
    pub level: String,
    pub tag: String,
    pub msg: String,
}

impl Default for FieldMap {
    fn default() -> Self {
        Self {
            ts: "ts".into(),
            host: "host".into(),
            app: "app".into(),
            pid: "pid".into(),
            level: "level".into(),
            tag: "tag".into(),
            msg: "msg".into(),
        }
    }
}

fn ts_from_value(v: &Value) -> Option<(i64, i16)> {
    match v {
        Value::String(s) => parse_rfc3339(s),
        Value::Number(n) => {
            let f = n.as_f64()?;
            if !f.is_finite() || f <= 0.0 {
                return None;
            }
            // 数量级识别：>1e15 微秒；>1e12 毫秒；否则秒（可带小数）
            let us = if f > 1e15 {
                f as i64
            } else if f > 1e12 {
                (f * 1e3) as i64
            } else {
                (f * 1e6) as i64
            };
            Some((us, 0))
        }
        _ => None,
    }
}

pub fn parse<'a>(raw: &'a str, map: &FieldMap) -> Option<ParsedLine<'a>> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let obj: serde_json::Map<String, Value> = serde_json::from_str(trimmed).ok()?;

    let (ts, tz) = obj
        .get(&map.ts)
        .and_then(ts_from_value)
        .map(|(us, off)| (Some(us), off))
        .unwrap_or((None, 0));

    let str_field = |key: &str| -> String {
        match obj.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };
    let host = str_field(&map.host);
    let app = str_field(&map.app);
    let tag = str_field(&map.tag);

    let pid = match obj.get(&map.pid) {
        Some(Value::Number(n)) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    };

    let level = match obj.get(&map.level) {
        Some(Value::Number(n)) => n.as_u64().map(|v| (v.min(7)) as u8),
        Some(Value::String(s)) => level_from_name(s),
        _ => None,
    }
    .unwrap_or(6);

    let msg = match obj.get(&map.msg) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    };

    // 借用字段须指向 raw —— JSON 解码值是新串，统一走 owned 路径。
    // host/app/tag 在 store 侧会被驻留，这里通过泄漏到调用栈外不可行，
    // 故 ParsedLine 的借用字段使用 raw 中不存在的内容时需由调用方处理：
    // 我们把它们放进 msg_owned 同级的 owned 容器。
    Some(ParsedLine {
        ts,
        tz_offset_min: tz,
        host: leak_into(raw, host)?,
        app: leak_into(raw, app)?,
        tag: leak_into(raw, tag)?,
        pid,
        level,
        facility: 16,
        msg_range: (0, 0),
        msg_owned: Some(msg),
        parsed: true,
    })
}

/// 在 raw 中找到与解码值相同的子串则借用之；否则退化为在 raw 中找不到的
/// 情况（含转义），此时借用空串并把值并入 msg_owned 前缀是不可取的——
/// 直接在 raw 里搜索子串：JSON 字段值无转义时必然存在。
fn leak_into<'a>(raw: &'a str, owned: String) -> Option<&'a str> {
    if owned.is_empty() {
        return Some("");
    }
    match raw.find(owned.as_str()) {
        Some(pos) => Some(&raw[pos..pos + owned.len()]),
        None => Some(""), // 含转义的罕见情况：字段值降级为空（msg 不受影响）
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> FieldMap {
        FieldMap::default()
    }

    #[test]
    fn basic_object() {
        let raw = r#"{"ts":"2026-06-12T10:16:41.834Z","host":"dev1","app":"sensor","pid":117,"level":"err","tag":"i2c","msg":"bus timeout"}"#;
        let p = parse(raw, &map()).unwrap();
        assert_eq!(p.host, "dev1");
        assert_eq!(p.app, "sensor");
        assert_eq!(p.pid, Some(117));
        assert_eq!(p.level, 3);
        assert_eq!(p.tag, "i2c");
        assert_eq!(p.msg_owned.as_deref(), Some("bus timeout"));
        assert!(p.ts.is_some());
    }

    #[test]
    fn epoch_seconds_millis_micros() {
        let s = r#"{"ts":1781259401.834,"msg":"a"}"#;
        let (us, _) = ts_from_value(&serde_json::json!(1781259401.834f64)).unwrap();
        assert_eq!(us / 1_000_000, 1781259401);
        let (us_ms, _) = ts_from_value(&serde_json::json!(1781259401834u64)).unwrap();
        assert_eq!(us_ms / 1_000_000, 1781259401);
        let (us_us, _) = ts_from_value(&serde_json::json!(1781259401834000u64)).unwrap();
        assert_eq!(us_us / 1_000_000, 1781259401);
        assert!(parse(s, &map()).unwrap().ts.is_some());
    }

    #[test]
    fn level_as_number() {
        let raw = r#"{"level":3,"msg":"x"}"#;
        assert_eq!(parse(raw, &map()).unwrap().level, 3);
    }

    #[test]
    fn custom_field_mapping() {
        let m = FieldMap {
            ts: "time".into(),
            host: "device".into(),
            msg: "message".into(),
            level: "severity".into(),
            ..FieldMap::default()
        };
        let raw = r#"{"time":"2026-06-12T10:16:41Z","device":"d9","severity":"warning","message":"low battery"}"#;
        let p = parse(raw, &m).unwrap();
        assert_eq!(p.host, "d9");
        assert_eq!(p.level, 4);
        assert_eq!(p.msg_owned.as_deref(), Some("low battery"));
    }

    #[test]
    fn escaped_msg_decoded() {
        let raw = r#"{"msg":"line1\nline2 \"quoted\""}"#;
        let p = parse(raw, &map()).unwrap();
        assert_eq!(p.msg_owned.as_deref(), Some("line1\nline2 \"quoted\""));
    }

    #[test]
    fn missing_fields_tolerated() {
        let raw = r#"{"msg":"only msg"}"#;
        let p = parse(raw, &map()).unwrap();
        assert_eq!(p.ts, None);
        assert_eq!(p.host, "");
        assert_eq!(p.level, 6);
    }

    #[test]
    fn non_json_rejected() {
        assert!(parse("not json", &map()).is_none());
        assert!(parse("{broken json", &map()).is_none());
    }

    #[test]
    fn non_string_msg_serialized() {
        let raw = r#"{"msg":{"k":1}}"#;
        let p = parse(raw, &map()).unwrap();
        assert_eq!(p.msg_owned.as_deref(), Some(r#"{"k":1}"#));
    }
}
