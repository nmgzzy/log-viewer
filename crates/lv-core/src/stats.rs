//! 仪表盘数据（FR-9）：时间桶 level 计数、错误率、Top 维度。
//! 只读扫描定长 meta（不触字符串），百万行毫秒级。

use crate::store::LogStore;

#[derive(Clone, Copy, Default, Debug)]
pub struct Bucket {
    pub err: u32,    // severity 0..=3
    pub warn: u32,   // 4
    pub info: u32,   // 5..=6
    pub debug: u32,  // 7
}

impl Bucket {
    pub fn total(&self) -> u32 {
        self.err + self.warn + self.info + self.debug
    }
}

#[derive(Clone, Debug, Default)]
pub struct DashStats {
    /// 桶宽（微秒）。
    pub bucket_us: i64,
    /// 第一个桶的起点。
    pub start_us: i64,
    pub buckets: Vec<Bucket>,
    /// 工作集整体错误率（err+ / 已解析行）。
    pub err_rate: f64,
    pub total_rows: u64,
    pub top_tags: Vec<(String, u64)>,
    pub top_apps: Vec<(String, u64)>,
    pub top_hosts: Vec<(String, u64)>,
}

/// 候选"好看的"桶宽：100ms…1h。
const NICE_BUCKETS_US: [i64; 12] = [
    100_000,
    500_000,
    1_000_000,
    5_000_000,
    10_000_000,
    30_000_000,
    60_000_000,
    300_000_000,
    600_000_000,
    1_800_000_000,
    3_600_000_000,
    7_200_000_000,
];

pub fn compute_dash(store: &LogStore, max_buckets: usize) -> DashStats {
    let n = store.len();
    if n == 0 {
        return DashStats::default();
    }
    let (min_ts, max_ts) = {
        // 工作集近似按 ts 递增（乱序仅在窗口内）：扫首尾窗口取极值
        let probe = n.min(1024);
        let mut min_ts = i64::MAX;
        let mut max_ts = i64::MIN;
        for i in 0..probe {
            if let Some(m) = store.meta_at(i) {
                min_ts = min_ts.min(m.ts);
            }
        }
        for i in n.saturating_sub(probe)..n {
            if let Some(m) = store.meta_at(i) {
                max_ts = max_ts.max(m.ts);
            }
        }
        (min_ts, max_ts)
    };
    let span = (max_ts - min_ts).max(1);
    let max_buckets = max_buckets.max(8) as i64;
    let bucket_us = NICE_BUCKETS_US
        .iter()
        .copied()
        .find(|b| span / b < max_buckets)
        .unwrap_or(span / max_buckets + 1);
    let start_us = min_ts - min_ts.rem_euclid(bucket_us);
    let count = ((max_ts - start_us) / bucket_us + 1).clamp(1, max_buckets * 2) as usize;
    let mut buckets = vec![Bucket::default(); count];
    let mut err_total = 0u64;
    let mut parsed_total = 0u64;
    for i in 0..n {
        let Some(m) = store.meta_at(i) else { continue };
        if !m.is_parsed() {
            continue;
        }
        parsed_total += 1;
        let idx = ((m.ts - start_us) / bucket_us).clamp(0, count as i64 - 1) as usize;
        let b = &mut buckets[idx];
        match m.level {
            0..=3 => {
                b.err += 1;
                err_total += 1;
            }
            4 => b.warn += 1,
            7 => b.debug += 1,
            _ => b.info += 1,
        }
    }
    let top = |counts: &std::collections::HashMap<u32, u64>| -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = counts
            .iter()
            .filter(|(_, c)| **c > 0)
            .map(|(id, c)| (store.syms.get(*id).to_owned(), *c))
            .filter(|(name, _)| !name.is_empty())
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(8);
        v
    };
    DashStats {
        bucket_us,
        start_us,
        buckets,
        err_rate: if parsed_total > 0 {
            err_total as f64 / parsed_total as f64
        } else {
            0.0
        },
        total_rows: n as u64,
        top_tags: top(&store.tag_counts),
        top_apps: top(&store.app_counts),
        top_hosts: top(&store.host_counts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_auto, ParserCtx};

    fn store_with(lines: &[String]) -> LogStore {
        let mut s = LogStore::new();
        let src = s.add_source("t");
        let ctx = ParserCtx::default();
        for (i, l) in lines.iter().enumerate() {
            let p = parse_auto(l, &ctx);
            s.append(l, &p, src, i as i64);
        }
        s
    }

    #[test]
    fn buckets_and_err_rate() {
        // 10 行：每秒 1 行，其中 2 行 err
        let lines: Vec<String> = (0..10)
            .map(|i| {
                let pri = if i < 2 { 131 } else { 134 };
                format!("<{pri}>1 2026-06-12T10:00:0{i}Z h a 1 t - m{i}")
            })
            .collect();
        let s = store_with(&lines);
        let d = compute_dash(&s, 120);
        assert_eq!(d.total_rows, 10);
        assert!((d.err_rate - 0.2).abs() < 1e-9);
        let sum: u32 = d.buckets.iter().map(|b| b.total()).sum();
        assert_eq!(sum, 10);
        let errs: u32 = d.buckets.iter().map(|b| b.err).sum();
        assert_eq!(errs, 2);
        assert!(d.bucket_us >= 100_000);
    }

    #[test]
    fn top_lists() {
        let lines: Vec<String> = (0..20)
            .map(|i| {
                let tag = if i % 4 == 0 { "rare" } else { "common" };
                format!("<134>1 2026-06-12T10:00:01Z h app{} 1 {tag} - m", i % 2)
            })
            .collect();
        let s = store_with(&lines);
        let d = compute_dash(&s, 60);
        assert_eq!(d.top_tags[0].0, "common");
        assert_eq!(d.top_tags[0].1, 15);
        assert_eq!(d.top_apps.len(), 2);
    }

    #[test]
    fn empty_store() {
        let s = LogStore::new();
        let d = compute_dash(&s, 60);
        assert_eq!(d.total_rows, 0);
        assert!(d.buckets.is_empty());
    }
}
