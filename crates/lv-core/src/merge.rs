//! 合并时间线（FR-8）：把多个源 store 的现有内容按 `ts` k 路归并进目标
//! store；每行 source 标注沿用各自来源名。live 增量由 ingest 的 taps +
//! `append_sorted`（时间窗重排）承担。

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::parse::{parse_auto, ParserCtx};
use crate::store::LogStore;

/// 合并输入：一个 store 及其展示名（通常是 Tab 标题）。
pub struct MergeInput<'a> {
    pub store: &'a LogStore,
    pub name: String,
}

/// 把各输入的当前内容合并到 target（按 ts 稳定归并）。
/// 返回合并的行数。target 中为每个输入建立 source 条目。
pub fn merge_snapshot(target: &mut LogStore, inputs: &[MergeInput], ctx: &ParserCtx) -> usize {
    let src_ids: Vec<u16> = inputs
        .iter()
        .map(|inp| target.add_source(inp.name.clone()))
        .collect();

    // 最小堆：(ts, 输入序号, 行下标) —— 输入序号参与比较保证稳定
    let mut heap: BinaryHeap<Reverse<(i64, usize, usize)>> = BinaryHeap::new();
    for (k, inp) in inputs.iter().enumerate() {
        if let Some(m) = inp.store.meta_at(0) {
            heap.push(Reverse((m.ts, k, 0)));
        }
    }
    let mut merged = 0usize;
    while let Some(Reverse((_, k, idx))) = heap.pop() {
        let src = inputs[k].store;
        let Some(m) = src.meta_at(idx) else { continue };
        let raw = src.raw_text(m);
        let p = parse_auto(raw, ctx);
        // 堆序保证目标按 ts 追加；显示层 TabView 再兜底窗口重排
        target.append(raw, &p, src_ids[k], m.ts);
        merged += 1;
        if let Some(next) = src.meta_at(idx + 1) {
            heap.push(Reverse((next.ts, k, idx + 1)));
        }
    }
    target.enforce_limits();
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_auto;

    fn store_with(name: &str, lines: &[&str]) -> LogStore {
        let mut s = LogStore::new();
        let src = s.add_source(name);
        let ctx = ParserCtx::default();
        for (i, l) in lines.iter().enumerate() {
            let p = parse_auto(l, &ctx);
            s.append(l, &p, src, i as i64);
        }
        s
    }

    #[test]
    fn merges_by_timestamp() {
        let a = store_with(
            "fileA",
            &[
                "<134>1 2026-06-12T10:00:01Z devA a 1 t - a1",
                "<134>1 2026-06-12T10:00:03Z devA a 1 t - a2",
            ],
        );
        let b = store_with(
            "fileB",
            &[
                "<134>1 2026-06-12T10:00:02Z devB a 1 t - b1",
                "<134>1 2026-06-12T10:00:04Z devB a 1 t - b2",
            ],
        );
        let mut target = LogStore::new();
        let n = merge_snapshot(
            &mut target,
            &[
                MergeInput { store: &a, name: "A".into() },
                MergeInput { store: &b, name: "B".into() },
            ],
            &ParserCtx::default(),
        );
        assert_eq!(n, 4);
        let msgs: Vec<String> = (0..target.len())
            .map(|i| target.msg_text(target.meta_at(i).unwrap()).to_owned())
            .collect();
        assert_eq!(msgs, vec!["a1", "b1", "a2", "b2"]);
        // source 标注正确
        let m1 = *target.meta_at(1).unwrap();
        assert_eq!(target.source_name(m1.source), "B");
    }

    #[test]
    fn merge_keeps_host_distinction() {
        let a = store_with("A", &["<134>1 2026-06-12T10:00:01Z devA a 1 t - x"]);
        let b = store_with("B", &["<134>1 2026-06-12T10:00:02Z devB a 1 t - y"]);
        let mut t = LogStore::new();
        merge_snapshot(
            &mut t,
            &[
                MergeInput { store: &a, name: "A".into() },
                MergeInput { store: &b, name: "B".into() },
            ],
            &ParserCtx::default(),
        );
        let hosts: Vec<&str> = (0..t.len())
            .map(|i| t.syms.get(t.meta_at(i).unwrap().host))
            .collect();
        assert_eq!(hosts, vec!["devA", "devB"]);
    }

    #[test]
    fn merge_empty_inputs() {
        let a = store_with("A", &[]);
        let mut t = LogStore::new();
        let n = merge_snapshot(
            &mut t,
            &[MergeInput { store: &a, name: "A".into() }],
            &ParserCtx::default(),
        );
        assert_eq!(n, 0);
        assert_eq!(t.len(), 0);
    }
}
