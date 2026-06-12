//! RFC3164（传统 syslog）解析（FR-P3）。
//!
//! `<PRI>MMM dd HH:MM:SS HOST TAG[PID]: MSG`，无年份、无毫秒、无独立 MSGID。
//! PRI 可缺省（如直接来自文件的 `Jun 12 10:16:41 host app: msg`）。
//! TAG 即进程名 → 归一到 `app` 字段，`tag`(MSGID) 留空。

use crate::model::ParsedLine;
use crate::parse::ts::{micros_from_parts, month_from_abbrev};
use crate::parse::ParserCtx;

pub fn parse<'a>(raw: &'a str, ctx: &ParserCtx) -> Option<ParsedLine<'a>> {
    let b = raw.as_bytes();
    let mut i = 0usize;
    let (level, facility) = if b.first() == Some(&b'<') {
        let gt = raw[..raw.len().min(6)].find('>')?;
        let pri: u16 = raw[1..gt].parse().ok()?;
        if pri > 191 {
            return None;
        }
        i = gt + 1;
        ((pri & 7) as u8, (pri >> 3) as u8)
    } else {
        (6u8, 1u8) // 无 PRI：user.info
    };

    // 时间戳：MMM dd HH:MM:SS（dd 可为空格补位）
    if raw.len() < i + 15 {
        return None;
    }
    let month = month_from_abbrev(raw.get(i..i + 3)?)?;
    if b.get(i + 3) != Some(&b' ') {
        return None;
    }
    let day_str = raw.get(i + 4..i + 6)?.trim_start();
    let day: u32 = day_str.parse().ok()?;
    if b.get(i + 6) != Some(&b' ') {
        return None;
    }
    let time_str = raw.get(i + 7..i + 15)?;
    let tb = time_str.as_bytes();
    if tb[2] != b':' || tb[5] != b':' {
        return None;
    }
    let hour: u32 = time_str[0..2].parse().ok()?;
    let min: u32 = time_str[3..5].parse().ok()?;
    let sec: u32 = time_str[6..8].parse().ok()?;
    let ts = micros_from_parts(
        ctx.default_year,
        month,
        day,
        hour,
        min,
        sec,
        ctx.default_tz_offset_min,
    )?;
    i += 15;
    if b.get(i) != Some(&b' ') {
        return None;
    }
    i += 1;

    // HOST
    let host_end = i + raw.get(i..)?.find(' ')?;
    let host = &raw[i..host_end];
    i = host_end + 1;

    // TAG[PID]: MSG —— content 部分：到第一个 ':' 为 tag（可带 [pid]）
    let rest = raw.get(i..)?;
    let (app, pid, msg_start) = match rest.find(": ") {
        Some(cpos) if cpos < 48 && !rest[..cpos].contains(' ') => {
            let tag_part = &rest[..cpos];
            let (name, pid) = match tag_part.find('[') {
                Some(lb) if tag_part.ends_with(']') => {
                    let inner = &tag_part[lb + 1..tag_part.len() - 1];
                    (&tag_part[..lb], inner.parse::<u32>().ok())
                }
                _ => (tag_part, None),
            };
            (name, pid, i + cpos + 2)
        }
        // 结尾恰好是 "tag:" 无消息
        _ if rest.ends_with(':') && !rest.contains(' ') => {
            let tag_part = &rest[..rest.len() - 1];
            let (name, pid) = match tag_part.find('[') {
                Some(lb) if tag_part.ends_with("]") => {
                    let inner = &tag_part[lb + 1..tag_part.len() - 1];
                    (&tag_part[..lb], inner.parse::<u32>().ok())
                }
                _ => (tag_part, None),
            };
            (name, pid, raw.len())
        }
        _ => ("", None, i), // 无 tag：整段为 msg
    };

    Some(ParsedLine {
        ts: Some(ts),
        tz_offset_min: ctx.default_tz_offset_min,
        host,
        app,
        tag: "",
        pid,
        level,
        facility,
        msg_range: (msg_start, raw.len()),
        msg_owned: None,
        parsed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ParserCtx {
        ParserCtx {
            default_year: 2026,
            default_tz_offset_min: 0,
            ..Default::default()
        }
    }

    #[test]
    fn classic_with_pri_and_pid() {
        let raw = "<13>Jun 12 10:16:41 myhost sshd[4321]: Accepted password for root";
        let p = parse(raw, &ctx()).unwrap();
        assert_eq!(p.level, 5); // 13 & 7
        assert_eq!(p.facility, 1);
        assert_eq!(p.host, "myhost");
        assert_eq!(p.app, "sshd");
        assert_eq!(p.pid, Some(4321));
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "Accepted password for root");
        assert!(p.ts.is_some());
    }

    #[test]
    fn no_pri_no_pid() {
        let raw = "Jun  2 03:04:05 host cron: job started";
        let p = parse(raw, &ctx()).unwrap();
        assert_eq!(p.level, 6);
        assert_eq!(p.app, "cron");
        assert_eq!(p.pid, None);
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "job started");
    }

    #[test]
    fn no_tag_at_all() {
        let raw = "Jun 12 10:16:41 host just some words here";
        let p = parse(raw, &ctx()).unwrap();
        assert_eq!(p.app, "");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "just some words here");
    }

    #[test]
    fn year_and_tz_from_ctx() {
        let c = ParserCtx {
            default_year: 2025,
            default_tz_offset_min: 480,
            ..Default::default()
        };
        let raw = "Jun 12 08:00:00 h a: m";
        let p = parse(raw, &c).unwrap();
        let expect = micros_from_parts(2025, 6, 12, 8, 0, 0, 480).unwrap();
        assert_eq!(p.ts, Some(expect));
        assert_eq!(p.tz_offset_min, 480);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("hello world this is not syslog", &ctx()).is_none());
        assert!(parse("<134>1 2026-06-12T10:16:41Z h a 1 t - m", &ctx()).is_none());
    }
}
