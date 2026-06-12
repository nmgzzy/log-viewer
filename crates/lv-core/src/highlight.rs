//! 规则高亮（FR-6）：按关键字/正则/字段条件设定颜色样式；
//! 规则有序（先命中先生效）、可启停、可导入导出（规则包）。
//! level 默认配色在 UI 层。

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::model::RecordMeta;
use crate::store::LogStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MatchField {
    #[default]
    Msg,
    Raw,
    Tag,
    App,
    Host,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightRule {
    pub name: String,
    pub enabled: bool,
    pub query: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub field: MatchField,
    /// RGB 前景/背景色；None 表示不改。
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
}

impl Default for HighlightRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            query: String::new(),
            is_regex: false,
            case_sensitive: false,
            field: MatchField::Msg,
            fg: None,
            bg: None,
            bold: false,
        }
    }
}

/// 编译后的规则集。规则顺序即优先级。
pub struct HighlightSet {
    pub rules: Vec<HighlightRule>,
    compiled: Vec<Option<Regex>>,
    pub errors: Vec<String>,
}

impl HighlightSet {
    pub fn compile(rules: Vec<HighlightRule>) -> Self {
        let mut compiled = Vec::with_capacity(rules.len());
        let mut errors = Vec::new();
        for r in &rules {
            if r.query.is_empty() {
                compiled.push(None);
                continue;
            }
            let pattern = if r.is_regex {
                r.query.clone()
            } else {
                regex::escape(&r.query)
            };
            match RegexBuilder::new(&pattern)
                .case_insensitive(!r.case_sensitive)
                .size_limit(1 << 22)
                .build()
            {
                Ok(re) => compiled.push(Some(re)),
                Err(e) => {
                    errors.push(format!("规则 `{}` 正则无效: {e}", r.name));
                    compiled.push(None);
                }
            }
        }
        Self {
            rules,
            compiled,
            errors,
        }
    }

    /// 返回第一条命中的启用规则。
    pub fn match_record<'a>(&'a self, store: &LogStore, m: &RecordMeta) -> Option<&'a HighlightRule> {
        for (rule, re) in self.rules.iter().zip(&self.compiled) {
            if !rule.enabled {
                continue;
            }
            let Some(re) = re else { continue };
            let text: &str = match rule.field {
                MatchField::Msg => store.msg_text(m),
                MatchField::Raw => store.raw_text(m),
                MatchField::Tag => store.syms.get(m.tag),
                MatchField::App => store.syms.get(m.app),
                MatchField::Host => store.syms.get(m.host),
            };
            if re.is_match(text) {
                return Some(rule);
            }
        }
        None
    }
}

/// 规则包导出/导入（JSON）。
pub fn rules_to_json(rules: &[HighlightRule]) -> String {
    serde_json::to_string_pretty(rules).unwrap_or_else(|_| "[]".into())
}

pub fn rules_from_json(s: &str) -> anyhow::Result<Vec<HighlightRule>> {
    Ok(serde_json::from_str(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_auto, ParserCtx};

    fn store_with(lines: &[&str]) -> LogStore {
        let mut s = LogStore::new();
        let src = s.add_source("test");
        let ctx = ParserCtx::default();
        for (i, l) in lines.iter().enumerate() {
            let p = parse_auto(l, &ctx);
            s.append(l, &p, src, i as i64);
        }
        s
    }

    fn rule(q: &str, field: MatchField) -> HighlightRule {
        HighlightRule {
            name: q.into(),
            query: q.into(),
            field,
            fg: Some([255, 0, 0]),
            ..Default::default()
        }
    }

    #[test]
    fn first_match_wins_by_order() {
        let s = store_with(&["<134>1 2026-06-12T10:00:01Z h a 1 i2c - timeout on bus"]);
        let m = *s.meta_at(0).unwrap();
        let set = HighlightSet::compile(vec![rule("bus", MatchField::Msg), rule("timeout", MatchField::Msg)]);
        assert_eq!(set.match_record(&s, &m).unwrap().name, "bus");
    }

    #[test]
    fn disabled_rule_skipped() {
        let s = store_with(&["<134>1 2026-06-12T10:00:01Z h a 1 i2c - timeout"]);
        let m = *s.meta_at(0).unwrap();
        let mut r1 = rule("timeout", MatchField::Msg);
        r1.enabled = false;
        let set = HighlightSet::compile(vec![r1]);
        assert!(set.match_record(&s, &m).is_none());
    }

    #[test]
    fn field_targets() {
        let s = store_with(&["<134>1 2026-06-12T10:00:01Z dev9 sensors 1 i2c - all good"]);
        let m = *s.meta_at(0).unwrap();
        assert!(HighlightSet::compile(vec![rule("i2c", MatchField::Tag)])
            .match_record(&s, &m)
            .is_some());
        assert!(HighlightSet::compile(vec![rule("dev9", MatchField::Host)])
            .match_record(&s, &m)
            .is_some());
        assert!(HighlightSet::compile(vec![rule("sensors", MatchField::App)])
            .match_record(&s, &m)
            .is_some());
        // msg 里没有 dev9
        assert!(HighlightSet::compile(vec![rule("dev9", MatchField::Msg)])
            .match_record(&s, &m)
            .is_none());
    }

    #[test]
    fn invalid_regex_collected_not_fatal() {
        let mut r = rule("([bad", MatchField::Msg);
        r.is_regex = true;
        let set = HighlightSet::compile(vec![r]);
        assert_eq!(set.errors.len(), 1);
        let s = store_with(&["anything"]);
        let m = *s.meta_at(0).unwrap();
        assert!(set.match_record(&s, &m).is_none());
    }

    #[test]
    fn json_roundtrip() {
        let rules = vec![
            rule("err", MatchField::Msg),
            HighlightRule {
                name: "内核".into(),
                query: "kernel".into(),
                field: MatchField::App,
                bg: Some([40, 40, 0]),
                bold: true,
                ..Default::default()
            },
        ];
        let json = rules_to_json(&rules);
        let back = rules_from_json(&json).unwrap();
        assert_eq!(back, rules);
    }
}
