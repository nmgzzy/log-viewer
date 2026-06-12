//! 混合格式自动探测（FR-P5）：按行首特征分派到候选解析器，
//! 全部失败则回退 `ParsedLine::unparsed`，绝不丢行。

use crate::model::ParsedLine;
use crate::parse::{jsonline, rfc3164, rfc5424, uf_file, ParserCtx};

/// 解析一行（自动探测格式）。总是返回结果。
pub fn parse_auto<'a>(raw: &'a str, ctx: &ParserCtx) -> ParsedLine<'a> {
    let b = raw.as_bytes();
    match b.first() {
        Some(b'<') => {
            // RFC5424（<PRI>1 ...）优先，失败转 RFC3164（<PRI>MMM dd ...）
            if let Some(p) = rfc5424::parse(raw) {
                return p;
            }
            if let Some(p) = rfc3164::parse(raw, ctx) {
                return p;
            }
        }
        Some(b'{') => {
            if let Some(p) = jsonline::parse(raw, &ctx.json_map) {
                return p;
            }
        }
        Some(c) if c.is_ascii_digit() => {
            // ISO 日期开头 → uf_log 文件模板
            if let Some(p) = uf_file::parse(raw) {
                return p;
            }
        }
        Some(c) if c.is_ascii_alphabetic() => {
            // 月份缩写开头 → 无 PRI 的 RFC3164
            if let Some(p) = rfc3164::parse(raw, ctx) {
                return p;
            }
        }
        _ => {}
    }
    // 行首特征误导时的兜底：把代价低的候选再各试一次
    if let Some(p) = uf_file::parse(raw) {
        return p;
    }
    if let Some(p) = jsonline::parse(raw, &ctx.json_map) {
        return p;
    }
    ParsedLine::unparsed(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ParserCtx {
        ParserCtx::default()
    }

    #[test]
    fn detects_each_format() {
        let c = ctx();
        let p = parse_auto("<134>1 2026-06-12T10:16:41.834Z h a 117 i2c - m", &c);
        assert!(p.parsed);
        assert_eq!(p.tag, "i2c");

        let p = parse_auto("2026-06-12T10:16:41.834+00:00 h a[117] info i2c: m", &c);
        assert!(p.parsed);
        assert_eq!(p.pid, Some(117));

        let p = parse_auto("<13>Jun 12 10:16:41 h sshd[1]: m", &c);
        assert!(p.parsed);
        assert_eq!(p.app, "sshd");

        let p = parse_auto(r#"{"msg":"hello","level":"err"}"#, &c);
        assert!(p.parsed);
        assert_eq!(p.level, 3);

        let p = parse_auto("Jun 12 10:16:41 host cron: tick", &c);
        assert!(p.parsed);
        assert_eq!(p.app, "cron");
    }

    #[test]
    fn mixed_garbage_never_dropped() {
        let c = ctx();
        for raw in [
            "",
            "   ",
            "random words without structure",
            "<<<>>>",
            "<999>1 bad pri",
            "{not json}",
            "\u{1f600} emoji line",
        ] {
            let p = parse_auto(raw, &c);
            assert!(!p.parsed, "应回退未解析: {raw:?}");
            assert_eq!(p.msg_range, (0, raw.len()));
        }
    }

    #[test]
    fn mixed_stream_each_line_independent() {
        let c = ctx();
        let lines = [
            "<134>1 2026-06-12T10:16:41.1Z h a 1 t - rfc5424",
            "2026-06-12T10:16:41.2+00:00 h a[1] info t: uf_file",
            "garbage in the middle",
            r#"{"ts":"2026-06-12T10:16:41.3Z","msg":"json"}"#,
        ];
        let parsed: Vec<bool> = lines.iter().map(|l| parse_auto(l, &c).parsed).collect();
        assert_eq!(parsed, vec![true, true, false, true]);
    }
}
