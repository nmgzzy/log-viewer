//! 导出（FR-11）：把当前/过滤后的视图导出为 文本 / JSON 行 / CSV。

use std::io::Write;

use crate::model::{level_name, PID_NONE};
use crate::parse::ts::format_rfc3339_ms;
use crate::store::LogStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Text,
    Json,
    Csv,
}

impl ExportFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Text => "log",
            ExportFormat::Json => "jsonl",
            ExportFormat::Csv => "csv",
        }
    }
}

/// 导出视图（seq 列表）到 writer，返回导出的行数。
pub fn export(
    store: &LogStore,
    view: &[u64],
    fmt: ExportFormat,
    w: &mut impl Write,
) -> anyhow::Result<u64> {
    let mut n = 0u64;
    if fmt == ExportFormat::Csv {
        writeln!(w, "ts,host,app,pid,level,tag,source,msg")?;
    }
    for seq in view {
        let Some(m) = store.meta_by_seq(*seq) else { continue };
        match fmt {
            ExportFormat::Text => {
                writeln!(w, "{}", store.raw_text(m))?;
            }
            ExportFormat::Json => {
                let obj = serde_json::json!({
                    "ts": if m.ts_is_synthetic() { serde_json::Value::Null } else {
                        serde_json::Value::String(format_rfc3339_ms(m.ts, m.tz_offset_min))
                    },
                    "host": store.syms.get(m.host),
                    "app": store.syms.get(m.app),
                    "pid": if m.pid == PID_NONE { serde_json::Value::Null } else { m.pid.into() },
                    "level": if m.is_parsed() { serde_json::Value::String(level_name(m.level).into()) } else { serde_json::Value::Null },
                    "tag": store.syms.get(m.tag),
                    "source": store.source_name(m.source),
                    "msg": store.msg_text(m),
                    "raw": store.raw_text(m),
                });
                writeln!(w, "{obj}")?;
            }
            ExportFormat::Csv => {
                let ts = if m.ts_is_synthetic() {
                    String::new()
                } else {
                    format_rfc3339_ms(m.ts, m.tz_offset_min)
                };
                let pid = if m.pid == PID_NONE {
                    String::new()
                } else {
                    m.pid.to_string()
                };
                let level = if m.is_parsed() { level_name(m.level) } else { "" };
                writeln!(
                    w,
                    "{},{},{},{},{},{},{},{}",
                    csv_field(&ts),
                    csv_field(store.syms.get(m.host)),
                    csv_field(store.syms.get(m.app)),
                    pid,
                    level,
                    csv_field(store.syms.get(m.tag)),
                    csv_field(store.source_name(m.source)),
                    csv_field(store.msg_text(m)),
                )?;
            }
        }
        n += 1;
    }
    w.flush()?;
    Ok(n)
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_auto, ParserCtx};

    fn demo() -> (LogStore, Vec<u64>) {
        let mut s = LogStore::new();
        let src = s.add_source("test");
        let ctx = ParserCtx::default();
        let lines = [
            "<131>1 2026-06-12T10:00:01.500Z dev1 app1 7 i2c - bus, \"quoted\" fail",
            "raw unparsed line",
        ];
        for (i, l) in lines.iter().enumerate() {
            let p = parse_auto(l, &ctx);
            s.append(l, &p, src, i as i64);
        }
        let view = (0..s.len() as u64).collect();
        (s, view)
    }

    #[test]
    fn text_is_raw_passthrough() {
        let (s, view) = demo();
        let mut buf = Vec::new();
        let n = export(&s, &view, ExportFormat::Text, &mut buf).unwrap();
        assert_eq!(n, 2);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("<131>1"));
        assert!(out.ends_with("raw unparsed line\n"));
    }

    #[test]
    fn json_lines_valid_and_complete() {
        let (s, view) = demo();
        let mut buf = Vec::new();
        export(&s, &view, ExportFormat::Json, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let objs: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0]["host"], "dev1");
        assert_eq!(objs[0]["level"], "err");
        assert_eq!(objs[0]["pid"], 7);
        assert!(objs[0]["ts"].as_str().unwrap().contains("10:00:01.500"));
        // 未解析行：ts/level 为 null，raw 保留
        assert!(objs[1]["ts"].is_null());
        assert!(objs[1]["level"].is_null());
        assert_eq!(objs[1]["raw"], "raw unparsed line");
    }

    #[test]
    fn csv_escaping() {
        let (s, view) = demo();
        let mut buf = Vec::new();
        export(&s, &view, ExportFormat::Csv, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let mut lines = out.lines();
        assert_eq!(lines.next().unwrap(), "ts,host,app,pid,level,tag,source,msg");
        let row = lines.next().unwrap();
        // 含逗号与引号的 msg 被正确引用
        assert!(row.ends_with("\"bus, \"\"quoted\"\" fail\""));
    }

    #[test]
    fn export_subset_only() {
        let (s, _) = demo();
        let mut buf = Vec::new();
        let n = export(&s, &[1], ExportFormat::Text, &mut buf).unwrap();
        assert_eq!(n, 1);
        assert_eq!(String::from_utf8(buf).unwrap(), "raw unparsed line\n");
    }
}
