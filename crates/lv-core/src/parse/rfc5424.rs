//! RFC5424 解析（FR-P1）。
//!
//! `<PRI>VERSION SP TIMESTAMP SP HOSTNAME SP APP-NAME SP PROCID SP MSGID SP
//!  STRUCTURED-DATA [SP MSG]`，NILVALUE 为 `-`。
//! 例：`<134>1 2026-06-12T10:16:41.834Z host app 117 i2c - msg`

use crate::model::ParsedLine;
use crate::parse::ts::parse_rfc3339;

/// 解析失败返回 None（调用方走下一个候选格式）。
pub fn parse(raw: &str) -> Option<ParsedLine<'_>> {
    let b = raw.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }
    let gt = raw[..raw.len().min(6)].find('>')?;
    let pri: u16 = raw[1..gt].parse().ok()?;
    if pri > 191 {
        return None;
    }
    let mut i = gt + 1;
    // VERSION：RFC5424 必须为非零数字（当前即 "1"）
    let ver_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == ver_start || i >= b.len() || b[i] != b' ' {
        return None; // 没有版本号 → 不是 RFC5424（可能是 RFC3164）
    }
    i += 1;

    let next_token = |from: usize| -> Option<(usize, usize)> {
        if from >= b.len() {
            return None;
        }
        let end = raw[from..].find(' ').map(|p| from + p).unwrap_or(raw.len());
        Some((from, end))
    };

    // TIMESTAMP
    let (ts_s, ts_e) = next_token(i)?;
    let ts_str = &raw[ts_s..ts_e];
    let (ts, tz) = if ts_str == "-" {
        (None, 0i16)
    } else {
        let (us, off) = parse_rfc3339(ts_str)?;
        (Some(us), off)
    };
    i = (ts_e + 1).min(raw.len());

    // HOSTNAME / APP-NAME / PROCID / MSGID
    let (h_s, h_e) = next_token(i)?;
    i = (h_e + 1).min(raw.len());
    let (a_s, a_e) = next_token(i)?;
    i = (a_e + 1).min(raw.len());
    let (p_s, p_e) = next_token(i)?;
    i = (p_e + 1).min(raw.len());
    let (m_s, m_e) = next_token(i)?;
    i = (m_e + 1).min(raw.len());

    let field = |s: usize, e: usize| -> &str {
        let v = &raw[s..e];
        if v == "-" {
            ""
        } else {
            v
        }
    };
    let host = field(h_s, h_e);
    let app = field(a_s, a_e);
    let procid = &raw[p_s..p_e];
    let pid: Option<u32> = if procid == "-" {
        None
    } else {
        procid.parse().ok() // 非数字 PROCID 容忍为无 pid
    };
    let tag = field(m_s, m_e);

    // STRUCTURED-DATA："-" 或一个以上 [..]（值内 ']' 以 '\' 转义）
    if i > raw.len() {
        return None;
    }
    let sd_start = i;
    if sd_start >= raw.len() {
        // 没有 SD 字段（行在 MSGID 后结束）：按空消息处理
        return Some(ParsedLine {
            ts,
            tz_offset_min: tz,
            host,
            app,
            tag,
            pid,
            level: (pri & 7) as u8,
            facility: (pri >> 3) as u8,
            msg_range: (raw.len(), raw.len()),
            msg_owned: None,
            parsed: true,
        });
    }
    let mut j = sd_start;
    if b[j] == b'-' {
        j += 1;
    } else if b[j] == b'[' {
        let mut escaped = false;
        let mut depth_open = false;
        while j < b.len() {
            let c = b[j];
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'[' {
                depth_open = true;
            } else if c == b']' {
                depth_open = false;
                // 多个 SD 元素相连：`][` 继续
                if j + 1 >= b.len() || b[j + 1] != b'[' {
                    j += 1;
                    break;
                }
            }
            j += 1;
        }
        if depth_open {
            return None; // SD 未闭合
        }
    } else {
        return None;
    }

    // MSG：SD 后跟单个空格；可能带 UTF-8 BOM
    let msg_start = if j < raw.len() && b[j] == b' ' {
        let mut s = j + 1;
        if raw[s..].starts_with('\u{feff}') {
            s += '\u{feff}'.len_utf8();
        }
        s
    } else {
        raw.len()
    };

    Some(ParsedLine {
        ts,
        tz_offset_min: tz,
        host,
        app,
        tag,
        pid,
        level: (pri & 7) as u8,
        facility: (pri >> 3) as u8,
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
        let raw = "<134>1 2026-06-12T10:16:41.834Z host app 117 i2c - msg body here";
        let p = parse(raw).unwrap();
        assert!(p.parsed);
        assert_eq!(p.level, 6); // 134 & 7
        assert_eq!(p.facility, 16); // local0
        assert_eq!(p.host, "host");
        assert_eq!(p.app, "app");
        assert_eq!(p.pid, Some(117));
        assert_eq!(p.tag, "i2c");
        assert!(p.ts.is_some());
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "msg body here");
    }

    #[test]
    fn nil_fields() {
        let raw = "<13>1 - - - - - - hello";
        let p = parse(raw).unwrap();
        assert_eq!(p.ts, None);
        assert_eq!(p.host, "");
        assert_eq!(p.app, "");
        assert_eq!(p.pid, None);
        assert_eq!(p.tag, "");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "hello");
    }

    #[test]
    fn structured_data_with_escaped_bracket() {
        let raw = r#"<165>1 2026-06-12T10:16:41Z h a 1 t [ex@123 k="v\]x"][e2 a="b"] real msg"#;
        let p = parse(raw).unwrap();
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "real msg");
    }

    #[test]
    fn no_msg_part() {
        let raw = "<134>1 2026-06-12T10:16:41.834Z host app 117 i2c -";
        let p = parse(raw).unwrap();
        assert_eq!(p.msg_range.0, p.msg_range.1);
    }

    #[test]
    fn kernel_empty_pid_nil() {
        let raw = "<6>1 2026-06-12T10:16:41.834Z dev kernel - boot - usb 1-1: new device";
        let p = parse(raw).unwrap();
        assert_eq!(p.app, "kernel");
        assert_eq!(p.pid, None);
        assert_eq!(p.facility, 0);
        assert_eq!(p.level, 6);
        assert_eq!(p.tag, "boot");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "usb 1-1: new device");
    }

    #[test]
    fn bom_stripped_from_msg() {
        let raw = "<134>1 2026-06-12T10:16:41Z h a 1 t - \u{feff}bom msg";
        let p = parse(raw).unwrap();
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "bom msg");
    }

    #[test]
    fn rejects_rfc3164_style() {
        // RFC3164：PRI 后不是版本号数字+空格
        assert!(parse("<13>Jun 12 10:16:41 host app: msg").is_none());
    }

    #[test]
    fn rejects_bad_pri() {
        assert!(parse("<999>1 2026-06-12T10:16:41Z h a - - - m").is_none());
        assert!(parse("no pri at all").is_none());
    }

    #[test]
    fn msg_with_chinese() {
        let raw = "<134>1 2026-06-12T10:16:41.834Z 设备一 应用 117 总线 - 温度过高，已降频";
        let p = parse(raw).unwrap();
        assert_eq!(p.host, "设备一");
        assert_eq!(&raw[p.msg_range.0..p.msg_range.1], "温度过高，已降频");
    }
}
