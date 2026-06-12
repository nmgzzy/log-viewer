//! Tab 视图维护：过滤后的 seq 列表 + 显示顺序。
//!
//! 存储永远只追加（seq 即身份，淘汰只动头部）；网络/合并流的乱序
//! 在这里解决——视图尾部窗口内按 ts 稳定排序（FR-8 时间窗重排）。

use crate::filter::CompiledFilter;
use crate::store::{LogStore, REORDER_WINDOW};

#[derive(Default)]
pub struct TabView {
    /// 过滤命中的 seq，按显示顺序排列。
    pub seqs: Vec<u64>,
    /// 已增量处理到的 store 末尾 seq。
    last_seen: u64,
    /// true：尾部按 ts 重排（UDP / 合并 Tab）。
    pub sort_by_ts: bool,
}

impl TabView {
    pub fn new(sort_by_ts: bool) -> Self {
        Self {
            sort_by_ts,
            ..Default::default()
        }
    }

    pub fn len(&self) -> usize {
        self.seqs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seqs.is_empty()
    }

    /// 全量重建（过滤条件变化时）。
    pub fn rebuild(&mut self, store: &LogStore, filter: &CompiledFilter) {
        self.seqs = filter.eval_full(store);
        if self.sort_by_ts {
            let mut keyed: Vec<(i64, u64)> = self
                .seqs
                .iter()
                .map(|&q| (store.meta_by_seq(q).map(|m| m.ts).unwrap_or(i64::MIN), q))
                .collect();
            keyed.sort_by_key(|(ts, _)| *ts); // 稳定排序保持到达序
            self.seqs = keyed.into_iter().map(|(_, q)| q).collect();
        }
        self.last_seen = store.next_seq();
    }

    /// 增量更新：清理已淘汰的头部 + 判定新行 + 尾部窗口重排。
    /// 返回是否有变化。
    pub fn update_incremental(&mut self, store: &LogStore, filter: &CompiledFilter) -> bool {
        let mut changed = false;
        // 1) 头部淘汰清理（淘汰只发生在最旧端，视图头部为最旧）
        let first = store.first_seq();
        let stale = self
            .seqs
            .iter()
            .take_while(|&&q| q < first)
            .count();
        if stale > 0 {
            self.seqs.drain(..stale);
            changed = true;
        }
        // 深处偶发的过期项（乱序窗口残留）：display 层按 None 安全降级，
        // 这里周期性整体清理避免积累
        if self.seqs.first().is_some_and(|&q| q < first) {
            self.seqs.retain(|&q| q >= first);
            changed = true;
        }
        // 2) 新行判定
        let next = store.next_seq();
        if self.last_seen < next {
            let from = self.last_seen.max(first);
            let added_at = self.seqs.len();
            for q in from..next {
                if let Some(m) = store.meta_by_seq(q) {
                    if filter.eval(store, m) {
                        self.seqs.push(q);
                    }
                }
            }
            self.last_seen = next;
            if self.seqs.len() > added_at {
                changed = true;
                // 3) 尾部按 ts 稳定重排（仅排序 tab）
                if self.sort_by_ts {
                    let win = (self.seqs.len() - added_at + REORDER_WINDOW)
                        .min(self.seqs.len());
                    let start = self.seqs.len() - win;
                    let tail = &mut self.seqs[start..];
                    let mut keyed: Vec<(i64, u64)> = tail
                        .iter()
                        .map(|&q| {
                            (store.meta_by_seq(q).map(|m| m.ts).unwrap_or(i64::MIN), q)
                        })
                        .collect();
                    keyed.sort_by_key(|(ts, _)| *ts);
                    for (slot, (_, q)) in tail.iter_mut().zip(keyed) {
                        *slot = q;
                    }
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{CompiledFilter, FilterSpec, TextCond};
    use crate::parse::{parse_auto, ParserCtx};
    use crate::store::RetainLimits;

    fn push(store: &mut LogStore, src: u16, line: &str, fallback: i64) {
        let ctx = ParserCtx::default();
        let p = parse_auto(line, &ctx);
        store.append(line, &p, src, fallback);
    }

    fn passthrough(store: &LogStore) -> CompiledFilter {
        CompiledFilter::compile(FilterSpec::default(), store)
    }

    #[test]
    fn incremental_appends_matching_only() {
        let mut s = LogStore::new();
        let src = s.add_source("t");
        let spec = FilterSpec {
            texts: vec![TextCond::contains("keep")],
            ..Default::default()
        };
        let mut v = TabView::new(false);
        let f = CompiledFilter::compile(spec, &s);
        v.rebuild(&s, &f);
        assert_eq!(v.len(), 0);
        push(&mut s, src, "keep one", 1);
        push(&mut s, src, "drop two", 2);
        push(&mut s, src, "keep three", 3);
        assert!(v.update_incremental(&s, &f));
        assert_eq!(v.seqs, vec![0, 2]);
    }

    #[test]
    fn sorted_view_reorders_tail_by_ts() {
        let mut s = LogStore::new();
        let src = s.add_source("udp");
        // 乱序到达（ts 在内容里）
        push(&mut s, src, "<134>1 2026-06-12T10:00:02Z d a 1 t - second", 0);
        push(&mut s, src, "<134>1 2026-06-12T10:00:01Z d a 1 t - first", 0);
        push(&mut s, src, "<134>1 2026-06-12T10:00:03Z d a 1 t - third", 0);
        let f = passthrough(&s);
        let mut v = TabView::new(true);
        v.update_incremental(&s, &f);
        let msgs: Vec<String> = v
            .seqs
            .iter()
            .map(|&q| s.msg_text(s.meta_by_seq(q).unwrap()).to_owned())
            .collect();
        assert_eq!(msgs, vec!["first", "second", "third"]);
        // 后续到达的乱序行也插入正确显示位置
        push(&mut s, src, "<134>1 2026-06-12T10:00:02.5Z d a 1 t - middle", 0);
        v.update_incremental(&s, &f);
        let msgs: Vec<String> = v
            .seqs
            .iter()
            .map(|&q| s.msg_text(s.meta_by_seq(q).unwrap()).to_owned())
            .collect();
        assert_eq!(msgs, vec!["first", "second", "middle", "third"]);
    }

    #[test]
    fn eviction_purges_view_head() {
        let mut s = LogStore::new();
        s.limits = RetainLimits {
            max_rows: 10,
            max_bytes: u64::MAX,
        };
        let src = s.add_source("t");
        let f = passthrough(&s);
        let mut v = TabView::new(false);
        for i in 0..30 {
            push(&mut s, src, &format!("line {i}"), i);
        }
        s.enforce_limits();
        v.update_incremental(&s, &f);
        assert_eq!(v.len(), 10);
        assert_eq!(*v.seqs.first().unwrap(), 20);
        // 显示访问全部有效
        assert!(v.seqs.iter().all(|&q| s.meta_by_seq(q).is_some()));
    }

    #[test]
    fn rebuild_after_filter_change() {
        let mut s = LogStore::new();
        let src = s.add_source("t");
        for i in 0..10 {
            push(
                &mut s,
                src,
                &format!("<13{}>1 2026-06-12T10:00:0{}Z h a 1 t - m{i}", i % 2 + 4, i),
                0,
            );
        }
        // <134> info / <135> debug 交替
        let mut spec = FilterSpec::default();
        spec.levels = [false; 8];
        spec.levels[6] = true; // 仅 info
        let f = CompiledFilter::compile(spec, &s);
        let mut v = TabView::new(false);
        v.rebuild(&s, &f);
        assert_eq!(v.len(), 5);
    }
}
