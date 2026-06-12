//! 搜索（FR-10）：与过滤解耦——在"当前可见集"（视图 seq 列表）上找命中，
//! 不改变可见集；支持子串/正则、计数与上一处/下一处跳转。

use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

use crate::store::LogStore;

#[derive(Clone, Debug, Default)]
pub struct SearchSpec {
    pub query: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
}

pub struct SearchResult {
    /// 命中的"视图内下标"（不是 seq），升序。
    pub hits: Vec<u32>,
    pub error: Option<String>,
}

pub fn compile_search(spec: &SearchSpec) -> Result<Regex, String> {
    let pattern = if spec.is_regex {
        spec.query.clone()
    } else {
        regex::escape(&spec.query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!spec.case_sensitive)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| format!("{e}"))
}

/// 在视图上执行搜索（对原始行匹配，覆盖所有列内容）。
pub fn run_search(store: &LogStore, view: &[u64], spec: &SearchSpec) -> SearchResult {
    if spec.query.is_empty() {
        return SearchResult {
            hits: Vec::new(),
            error: None,
        };
    }
    let re = match compile_search(spec) {
        Ok(r) => r,
        Err(e) => {
            return SearchResult {
                hits: Vec::new(),
                error: Some(e),
            }
        }
    };
    let flags: Vec<bool> = view
        .par_iter()
        .with_min_len(4096)
        .map(|seq| {
            store
                .meta_by_seq(*seq)
                .map(|m| re.is_match(store.raw_text(m)))
                .unwrap_or(false)
        })
        .collect();
    let hits = flags
        .iter()
        .enumerate()
        .filter_map(|(i, h)| h.then_some(i as u32))
        .collect();
    SearchResult { hits, error: None }
}

/// 当前行之后（含当前）的下一处命中；wrap 环回。
pub fn next_hit(hits: &[u32], from_row: u32) -> Option<u32> {
    if hits.is_empty() {
        return None;
    }
    match hits.binary_search(&from_row) {
        Ok(i) => Some(hits[i]),
        Err(i) if i < hits.len() => Some(hits[i]),
        _ => Some(hits[0]), // wrap
    }
}

/// 当前行之前的上一处命中；wrap 环回。
pub fn prev_hit(hits: &[u32], from_row: u32) -> Option<u32> {
    if hits.is_empty() {
        return None;
    }
    match hits.binary_search(&from_row) {
        Ok(0) | Err(0) => Some(*hits.last().unwrap()), // wrap
        Ok(i) => Some(hits[i - 1]),
        Err(i) => Some(hits[i - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_auto, ParserCtx};

    fn store_with(lines: &[&str]) -> (LogStore, Vec<u64>) {
        let mut s = LogStore::new();
        let src = s.add_source("test");
        let ctx = ParserCtx::default();
        for (i, l) in lines.iter().enumerate() {
            let p = parse_auto(l, &ctx);
            s.append(l, &p, src, i as i64);
        }
        let view: Vec<u64> = (0..s.len() as u64).collect();
        (s, view)
    }

    #[test]
    fn finds_hits_in_view_order() {
        let (s, view) = store_with(&[
            "alpha one",
            "beta two",
            "ALPHA three",
            "gamma four",
        ]);
        let r = run_search(
            &s,
            &view,
            &SearchSpec {
                query: "alpha".into(),
                ..Default::default()
            },
        );
        assert_eq!(r.hits, vec![0, 2]);
    }

    #[test]
    fn case_sensitive_mode() {
        let (s, view) = store_with(&["alpha", "ALPHA"]);
        let r = run_search(
            &s,
            &view,
            &SearchSpec {
                query: "ALPHA".into(),
                case_sensitive: true,
                ..Default::default()
            },
        );
        assert_eq!(r.hits, vec![1]);
    }

    #[test]
    fn regex_and_error() {
        let (s, view) = store_with(&["abc123", "xyz"]);
        let r = run_search(
            &s,
            &view,
            &SearchSpec {
                query: r"\d+".into(),
                is_regex: true,
                ..Default::default()
            },
        );
        assert_eq!(r.hits, vec![0]);
        let r = run_search(
            &s,
            &view,
            &SearchSpec {
                query: "([bad".into(),
                is_regex: true,
                ..Default::default()
            },
        );
        assert!(r.error.is_some());
    }

    #[test]
    fn next_prev_with_wrap() {
        let hits = vec![2u32, 5, 9];
        assert_eq!(next_hit(&hits, 0), Some(2));
        assert_eq!(next_hit(&hits, 3), Some(5));
        assert_eq!(next_hit(&hits, 9), Some(9));
        assert_eq!(next_hit(&hits, 10), Some(2)); // wrap
        assert_eq!(prev_hit(&hits, 9), Some(5));
        assert_eq!(prev_hit(&hits, 2), Some(9)); // wrap
        assert_eq!(prev_hit(&hits, 7), Some(5));
        assert_eq!(next_hit(&[], 0), None);
    }

    #[test]
    fn search_only_on_view_subset() {
        let (s, _) = store_with(&["match a", "match b", "match c"]);
        let view = vec![0u64, 2]; // 模拟过滤后的视图
        let r = run_search(
            &s,
            &view,
            &SearchSpec {
                query: "match".into(),
                ..Default::default()
            },
        );
        assert_eq!(r.hits, vec![0, 1]); // 视图内下标
    }
}
