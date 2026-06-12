//! uf_log 本地文件模板解析（FR-P2）。
//!
//! `ISODATE HOST APP[PID] LEVEL TAG: MSG`
//! 例：`2026-06-12T10:16:41.834+00:00 host app[117] info i2c: msg`
//! 须容忍空 PID（内核日志：`kernel[]`）；TAG 可缺省（无 `xxx:` token 时
//! 其余部分整体作为 MSG）。

use crate::model::{level_from_name, ParsedLine};
use crate::parse::ts::leading_rfc3339;

pub fn parse(raw: &str) -> Option<ParsedLine<'_>> {
    let (ts_end, ts_us, tz) = leading_rfc3339(raw)?;
    let rest = raw.get(ts_end..)?;
    if !rest.starts_with(' ') {
        return None;
    }

    // HOST
    let host_start = ts_end + 1;
    let host_end = host_start + raw.get(host_start..)?.find(' ')?;
    let host = &raw[host_start..host_end];

    // APP[PID]
    let app_start = host_end + 1;
    let app_end = app_start + raw.get(app_start..)?.find(' ')?;
    let app_tok = &raw[app_start..app_end];
    let (app, pid) = match app_tok.find('[') {
        Some(lb) if app_tok.ends_with(']') => {
            let inner = &app_tok[lb + 1..app_tok.len() - 1];
            let pid = if inner.is_empty() {
                None
            } else {
                Some(inner.parse::<u32>().ok()?)
            };
            (&app_tok[..lb], pid)
        }
        _ => (app_tok, None), // 容忍无 [] 的进程名
    };

    // LEVEL
    let lvl_start = app_end + 1;
    let lvl_end = lvl_start
        + raw
            .get(lvl_start..)?
            .find(' ')
            .unwrap_or(raw.len() - lvl_start);
    let level = level_from_name(&raw[lvl_start..lvl_end])?;

    // TAG: MSG（TAG 为下一 token 且以 ':' 结尾；否则整段为 MSG）
    let after_lvl = (lvl_end + 1).min(raw.len());
    let (tag, msg_start) = match raw.get(after_lvl..) {
        None | Some("") => ("", raw.len()),
        Some(restmsg) => {
            let tok_end = restmsg.find(' ').unwrap_or(restmsg.len());
            let tok = &restmsg[..tok_end];
            if let Some(stripped) = tok.strip_suffix(':') {
                let msg_at = after_lvl + (tok_end + 1).min(restmsg.len());
                (stripped, msg_at)
            } else {
                ("", after_lvl)
            }
        }
    };

    let facility = if app == "kernel" { 0 } else { 16 };

    Some(ParsedLine {
        ts: Some(ts_us),
        tz_offset_min: tz,
        host,
        app,
        tag,
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

    #[test]
    fn doc_example() {
        let raw = "2026-06-12T10:16:41.834+00:00 host app[117] info i2c: msg here";
        let p = parse(raw).unwrap();
        assert_eq!(p.host, "host");
        assert_eq!(p.app, "app");
        assert_eq!(p.pid, Some(117));
        assert_eq!(p.level, 6);
        assert_eq!(p.tag, "i2c");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "msg here");
        assert_eq!(p.tz_offset_min, 0);
    }

    #[test]
    fn kernel_empty_pid() {
        let raw = "2026-06-12T10:16:41.834+00:00 dev1 kernel[] warning usb: device reset";
        let p = parse(raw).unwrap();
        assert_eq!(p.app, "kernel");
        assert_eq!(p.pid, None);
        assert_eq!(p.level, 4);
        assert_eq!(p.facility, 0);
        assert_eq!(p.tag, "usb");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "device reset");
    }

    #[test]
    fn no_tag_token() {
        let raw = "2026-06-12T10:16:41.834+00:00 host app[1] err something failed badly";
        let p = parse(raw).unwrap();
        assert_eq!(p.level, 3);
        assert_eq!(p.tag, "");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "something failed badly");
    }

    #[test]
    fn level_aliases() {
        let raw = "2026-06-12T10:16:41.834+00:00 h a[1] error t: m";
        assert_eq!(parse(raw).unwrap().level, 3);
        let raw = "2026-06-12T10:16:41.834+00:00 h a[1] warn t: m";
        assert_eq!(parse(raw).unwrap().level, 4);
    }

    #[test]
    fn empty_msg() {
        let raw = "2026-06-12T10:16:41.834+00:00 host app[117] info i2c:";
        let p = parse(raw).unwrap();
        assert_eq!(p.tag, "i2c");
        assert_eq!(p.msg_range.0, p.msg_range.1);
    }

    #[test]
    fn tz_offset_preserved() {
        let raw = "2026-06-12T18:16:41.834+08:00 host app[117] info i2c: m";
        let p = parse(raw).unwrap();
        assert_eq!(p.tz_offset_min, 480);
    }

    #[test]
    fn rejects_non_iso() {
        assert!(parse("Jun 12 10:16:41 host app: msg").is_none());
        assert!(parse("<134>1 2026-06-12T10:16:41Z h a 1 t - m").is_none());
        assert!(parse("totally not a log").is_none());
    }

    #[test]
    fn rejects_bad_level() {
        // level 位置不是合法级别名 → 整行交给其它解析器/回退
        assert!(parse("2026-06-12T10:16:41Z host app[1] notalevel t: m").is_none());
    }
}
