//! 过滤引擎（FR-4）。
//!
//! 多维条件：level 多选、tag/host/app 包含与排除、pid、时间范围、
//! 文本（子串/正则，include/exclude，AND/OR）。全量扫描用 rayon 并行，
//! 新进流逐行增量判定。所有文本匹配统一编译为 `regex`（字面量转义），
//! 线性时间执行，天然防 ReDoS。

use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::model::RecordMeta;
use crate::store::LogStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Combine {
    #[default]
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextCond {
    pub query: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    /// true：命中则排除该行。
    pub exclude: bool,
}

impl TextCond {
    pub fn contains(q: impl Into<String>) -> Self {
        Self {
            query: q.into(),
            is_regex: false,
            case_sensitive: false,
            exclude: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterSpec {
    /// level 多选（下标即 severity 0–7）。
    pub levels: [bool; 8],
    pub include_tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    pub include_hosts: Vec<String>,
    pub exclude_hosts: Vec<String>,
    pub include_apps: Vec<String>,
    pub exclude_apps: Vec<String>,
    pub pid: Option<u32>,
    pub time_from_us: Option<i64>,
    pub time_to_us: Option<i64>,
    /// 对 msg 的文本条件（include 条件之间按 combine 组合，exclude 恒为 AND-NOT）。
    pub texts: Vec<TextCond>,
    pub combine: Combine,
    /// 未解析行是否显示（它们没有可靠的 level/tag 维度）。
    pub show_unparsed: bool,
}

impl Default for FilterSpec {
    fn default() -> Self {
        Self {
            levels: [true; 8],
            include_tags: Vec::new(),
            exclude_tags: Vec::new(),
            include_hosts: Vec::new(),
            exclude_hosts: Vec::new(),
            include_apps: Vec::new(),
            exclude_apps: Vec::new(),
            pid: None,
            time_from_us: None,
            time_to_us: None,
            texts: Vec::new(),
            combine: Combine::And,
            show_unparsed: true,
        }
    }
}

impl FilterSpec {
    /// 是否未施加任何约束（全通过）。
    pub fn is_passthrough(&self) -> bool {
        self == &FilterSpec::default()
    }

    /// "≥ 某严重级别"快捷设置：保留 0..=threshold。
    pub fn set_min_severity(&mut self, threshold: u8) {
        for (i, v) in self.levels.iter_mut().enumerate() {
            *v = i as u8 <= threshold;
        }
    }
}

/// 编译后的过滤器：正则已编译、符号集合已就绪。
pub struct CompiledFilter {
    pub spec: FilterSpec,
    include_res: Vec<Regex>,
    exclude_res: Vec<Regex>,
    /// 正则编译错误（UI 提示用；出错的条件按"不匹配任何行"处理）。
    pub errors: Vec<String>,
    // 符号 id 缓存（基于某一时刻的符号表；新符号在 eval 时兜底查询）
    inc_tags: Option<HashSet<u32>>,
    exc_tags: HashSet<u32>,
    inc_hosts: Option<HashSet<u32>>,
    exc_hosts: HashSet<u32>,
    inc_apps: Option<HashSet<u32>>,
    exc_apps: HashSet<u32>,
    syms_stamp: usize,
}

fn build_regex(c: &TextCond, errors: &mut Vec<String>) -> Option<Regex> {
    let pattern = if c.is_regex {
        c.query.clone()
    } else {
        regex::escape(&c.query)
    };
    match RegexBuilder::new(&pattern)
        .case_insensitive(!c.case_sensitive)
        .size_limit(1 << 22) // 编译期上限，防恶意模式占内存
        .build()
    {
        Ok(r) => Some(r),
        Err(e) => {
            errors.push(format!("正则无效 `{}`: {e}", c.query));
            None
        }
    }
}

impl CompiledFilter {
    pub fn compile(spec: FilterSpec, store: &LogStore) -> Self {
        let mut errors = Vec::new();
        let mut include_res = Vec::new();
        let mut exclude_res = Vec::new();
        for c in spec.texts.iter().filter(|c| !c.query.is_empty()) {
            if let Some(r) = build_regex(c, &mut errors) {
                if c.exclude {
                    exclude_res.push(r);
                } else {
                    include_res.push(r);
                }
            } else if !c.exclude {
                // 无效的 include 正则：让结果为空集比静默全量更可见
                include_res.push(Regex::new("$^never-matches^$").unwrap());
            }
        }
        let to_set = |names: &[String], store: &LogStore| -> HashSet<u32> {
            names.iter().filter_map(|n| store.syms.lookup(n)).collect()
        };
        let inc_or_none = |names: &[String], store: &LogStore| -> Option<HashSet<u32>> {
            if names.is_empty() {
                None
            } else {
                Some(to_set(names, store))
            }
        };
        Self {
            inc_tags: inc_or_none(&spec.include_tags, store),
            exc_tags: to_set(&spec.exclude_tags, store),
            inc_hosts: inc_or_none(&spec.include_hosts, store),
            exc_hosts: to_set(&spec.exclude_hosts, store),
            inc_apps: inc_or_none(&spec.include_apps, store),
            exc_apps: to_set(&spec.exclude_apps, store),
            syms_stamp: store.syms.len(),
            include_res,
            exclude_res,
            errors,
            spec,
        }
    }

    /// 符号表增长后刷新 id 缓存（增量流入新 tag/host 时调用）。
    pub fn refresh_syms(&mut self, store: &LogStore) {
        if store.syms.len() == self.syms_stamp {
            return;
        }
        *self = Self::compile(self.spec.clone(), store);
    }

    /// 判定单条记录。
    pub fn eval(&self, store: &LogStore, m: &RecordMeta) -> bool {
        if !m.is_parsed() {
            if !self.spec.show_unparsed {
                return false;
            }
            // 未解析行只受时间与文本条件约束
        } else {
            if !self.spec.levels[m.level as usize & 7] {
                return false;
            }
            if let Some(inc) = &self.inc_tags {
                if !inc.contains(&m.tag) {
                    return false;
                }
            }
            if self.exc_tags.contains(&m.tag) {
                return false;
            }
            if let Some(inc) = &self.inc_hosts {
                if !inc.contains(&m.host) {
                    return false;
                }
            }
            if self.exc_hosts.contains(&m.host) {
                return false;
            }
            if let Some(inc) = &self.inc_apps {
                if !inc.contains(&m.app) {
                    return false;
                }
            }
            if self.exc_apps.contains(&m.app) {
                return false;
            }
            if let Some(pid) = self.spec.pid {
                if m.pid != pid {
                    return false;
                }
            }
        }
        if let Some(from) = self.spec.time_from_us {
            if m.ts < from {
                return false;
            }
        }
        if let Some(to) = self.spec.time_to_us {
            if m.ts > to {
                return false;
            }
        }
        if !self.include_res.is_empty() || !self.exclude_res.is_empty() {
            let msg = store.msg_text(m);
            for r in &self.exclude_res {
                if r.is_match(msg) {
                    return false;
                }
            }
            if !self.include_res.is_empty() {
                let hit = match self.spec.combine {
                    Combine::And => self.include_res.iter().all(|r| r.is_match(msg)),
                    Combine::Or => self.include_res.iter().any(|r| r.is_match(msg)),
                };
                if !hit {
                    return false;
                }
            }
        }
        true
    }

    /// 全量扫描（rayon 并行），返回命中记录的 seq（保持顺序）。
    pub fn eval_full(&self, store: &LogStore) -> Vec<u64> {
        let len = store.len();
        let first = store.first_seq();
        if self.spec.is_passthrough() {
            return (first..first + len as u64).collect();
        }
        let hits: Vec<bool> = (0..len)
            .into_par_iter()
            .with_min_len(4096)
            .map(|i| {
                store
                    .meta_at(i)
                    .map(|m| self.eval(store, m))
                    .unwrap_or(false)
            })
            .collect();
        let mut out = Vec::with_capacity(hits.iter().filter(|h| **h).count());
        for (i, h) in hits.iter().enumerate() {
            if *h {
                out.push(first + i as u64);
            }
        }
        out
    }
}

/// 命名保存的过滤器（FR-4：命名、复用、导入导出）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedFilter {
    pub name: String,
    pub spec: FilterSpec,
}

pub fn filters_to_json(filters: &[SavedFilter]) -> String {
    serde_json::to_string_pretty(filters).unwrap_or_else(|_| "[]".into())
}

pub fn filters_from_json(s: &str) -> anyhow::Result<Vec<SavedFilter>> {
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

    fn demo_store() -> LogStore {
        store_with(&[
            "<131>1 2026-06-12T10:00:01Z dev1 app1 1 i2c - bus timeout on read",   // err
            "<132>1 2026-06-12T10:00:02Z dev1 app1 1 uart - rx overflow",          // warning
            "<134>1 2026-06-12T10:00:03Z dev2 app2 2 i2c - probe ok",              // info
            "<135>1 2026-06-12T10:00:04Z dev2 app2 2 spi - debug detail xfer",     // debug
            "raw garbage line unparsed",
        ])
    }

    fn run(spec: FilterSpec, store: &LogStore) -> Vec<u64> {
        CompiledFilter::compile(spec, store).eval_full(store)
    }

    #[test]
    fn passthrough_returns_all() {
        let s = demo_store();
        assert_eq!(run(FilterSpec::default(), &s).len(), 5);
    }

    #[test]
    fn min_severity() {
        let s = demo_store();
        let mut spec = FilterSpec::default();
        spec.set_min_severity(4); // err+warning（含 0..=4)
        spec.show_unparsed = false;
        let hits = run(spec, &s);
        assert_eq!(hits, vec![0, 1]);
    }

    #[test]
    fn tag_include_exclude() {
        let s = demo_store();
        let mut spec = FilterSpec::default();
        spec.include_tags = vec!["i2c".into()];
        spec.show_unparsed = false;
        assert_eq!(run(spec, &s), vec![0, 2]);

        let mut spec = FilterSpec::default();
        spec.exclude_tags = vec!["i2c".into(), "spi".into()];
        spec.show_unparsed = false;
        assert_eq!(run(spec, &s), vec![1]);
    }

    #[test]
    fn host_and_pid() {
        let s = demo_store();
        let mut spec = FilterSpec::default();
        spec.include_hosts = vec!["dev2".into()];
        spec.show_unparsed = false;
        assert_eq!(run(spec, &s), vec![2, 3]);

        let mut spec = FilterSpec::default();
        spec.pid = Some(1);
        spec.show_unparsed = false;
        assert_eq!(run(spec, &s), vec![0, 1]);
    }

    #[test]
    fn time_range() {
        let s = demo_store();
        let (from, _) = crate::parse::ts::parse_rfc3339("2026-06-12T10:00:02Z").unwrap();
        let (to, _) = crate::parse::ts::parse_rfc3339("2026-06-12T10:00:03Z").unwrap();
        let spec = FilterSpec {
            time_from_us: Some(from),
            time_to_us: Some(to),
            show_unparsed: false,
            ..Default::default()
        };
        assert_eq!(run(spec, &s), vec![1, 2]);
    }

    #[test]
    fn text_substring_case_insensitive() {
        let s = demo_store();
        let spec = FilterSpec {
            texts: vec![TextCond::contains("TIMEOUT")],
            ..Default::default()
        };
        assert_eq!(run(spec, &s), vec![0]);
    }

    #[test]
    fn text_and_or_exclude() {
        let s = store_with(&[
            "<134>1 2026-06-12T10:00:01Z h a 1 t - alpha beta",
            "<134>1 2026-06-12T10:00:02Z h a 1 t - alpha gamma",
            "<134>1 2026-06-12T10:00:03Z h a 1 t - beta gamma",
        ]);
        // AND
        let spec = FilterSpec {
            texts: vec![TextCond::contains("alpha"), TextCond::contains("beta")],
            combine: Combine::And,
            ..Default::default()
        };
        assert_eq!(run(spec, &s), vec![0]);
        // OR
        let spec = FilterSpec {
            texts: vec![TextCond::contains("alpha"), TextCond::contains("beta")],
            combine: Combine::Or,
            ..Default::default()
        };
        assert_eq!(run(spec, &s), vec![0, 1, 2]);
        // exclude
        let spec = FilterSpec {
            texts: vec![TextCond::contains("alpha"), TextCond {
                exclude: true,
                ..TextCond::contains("gamma")
            }],
            ..Default::default()
        };
        assert_eq!(run(spec, &s), vec![0]);
    }

    #[test]
    fn regex_match_and_invalid_regex() {
        let s = demo_store();
        let spec = FilterSpec {
            texts: vec![TextCond {
                query: r"bus\s+time".into(),
                is_regex: true,
                case_sensitive: false,
                exclude: false,
            }],
            ..Default::default()
        };
        assert_eq!(run(spec, &s), vec![0]);

        let bad = FilterSpec {
            texts: vec![TextCond {
                query: "([unclosed".into(),
                is_regex: true,
                case_sensitive: false,
                exclude: false,
            }],
            ..Default::default()
        };
        let cf = CompiledFilter::compile(bad, &s);
        assert!(!cf.errors.is_empty());
        assert!(cf.eval_full(&s).is_empty()); // 显式空集而非静默放行
    }

    #[test]
    fn unparsed_visibility_toggle() {
        let s = demo_store();
        let mut spec = FilterSpec::default();
        spec.set_min_severity(3);
        // 未解析行默认仍显示
        assert_eq!(run(spec.clone(), &s), vec![0, 4]);
        spec.show_unparsed = false;
        assert_eq!(run(spec, &s), vec![0]);
    }

    #[test]
    fn saved_filters_roundtrip() {
        let mut spec = FilterSpec::default();
        spec.include_tags = vec!["i2c".into()];
        spec.texts = vec![TextCond::contains("x")];
        let saved = vec![SavedFilter {
            name: "我的过滤".into(),
            spec,
        }];
        let json = filters_to_json(&saved);
        let back = filters_from_json(&json).unwrap();
        assert_eq!(back, saved);
    }
}
