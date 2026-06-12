//! LogStore：单个 Tab（或合并 Tab）的工作集存储。
//!
//! - 追加为主；合并/乱序网络流用 `append_sorted`（仅在尾部窗口内回插）。
//! - 环形保留：超过行数/字节上限时从最旧端淘汰，并记录淘汰计数用于提示。
//! - 记录以全局递增 seq 标识，视图层持有 seq 而非下标，淘汰后仍稳定。

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::arena::Arena;
use crate::model::{flags, ParsedLine, RecordMeta, SpanRef, PID_NONE};
use crate::symbols::SymbolTable;

/// 乱序回插窗口：网络日志只会在尾部这个范围内乱序（更早的视为已定序）。
pub const REORDER_WINDOW: usize = 8192;

#[derive(Clone, Copy, Debug)]
pub struct RetainLimits {
    pub max_rows: usize,
    pub max_bytes: u64,
}

impl Default for RetainLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_200_000,
            max_bytes: 768 << 20, // 768 MiB 原始文本上限
        }
    }
}

/// 一个输入源的描述（用于 source 列与合并标注）。
#[derive(Clone, Debug)]
pub struct SourceInfo {
    /// 形如 `file:D:\a\messages` 或 `udp:0.0.0.0:514`。
    pub name: String,
}

pub struct LogStore {
    pub syms: SymbolTable,
    arena: Arena,
    meta: VecDeque<RecordMeta>,
    /// meta[0] 的全局 seq。
    first_seq: u64,
    /// 因保留策略被淘汰的总行数。
    pub evicted_rows: u64,
    pub limits: RetainLimits,
    pub sources: Vec<SourceInfo>,
    // 工作集 facet 计数（淘汰时同步递减）
    pub level_counts: [u64; 8],
    pub tag_counts: HashMap<u32, u64>,
    pub host_counts: HashMap<u32, u64>,
    pub app_counts: HashMap<u32, u64>,
    pub unparsed_count: u64,
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LogStore {
    pub fn new() -> Self {
        Self {
            syms: SymbolTable::new(),
            arena: Arena::default(),
            meta: VecDeque::new(),
            first_seq: 0,
            evicted_rows: 0,
            limits: RetainLimits::default(),
            sources: Vec::new(),
            level_counts: [0; 8],
            tag_counts: HashMap::new(),
            host_counts: HashMap::new(),
            app_counts: HashMap::new(),
            unparsed_count: 0,
        }
    }

    pub fn add_source(&mut self, name: impl Into<String>) -> u16 {
        let id = self.sources.len() as u16;
        self.sources.push(SourceInfo { name: name.into() });
        id
    }

    pub fn source_name(&self, id: u16) -> &str {
        self.sources
            .get(id as usize)
            .map(|s| s.name.as_str())
            .unwrap_or("?")
    }

    pub fn len(&self) -> usize {
        self.meta.len()
    }

    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    /// 当前工作集的 seq 范围 [first, end)。
    pub fn seq_range(&self) -> (u64, u64) {
        (self.first_seq, self.first_seq + self.meta.len() as u64)
    }

    pub fn first_seq(&self) -> u64 {
        self.first_seq
    }

    pub fn next_seq(&self) -> u64 {
        self.first_seq + self.meta.len() as u64
    }

    /// 按下标访问（0..len）。
    pub fn meta_at(&self, idx: usize) -> Option<&RecordMeta> {
        self.meta.get(idx)
    }

    pub fn meta_by_seq(&self, seq: u64) -> Option<&RecordMeta> {
        let idx = seq.checked_sub(self.first_seq)? as usize;
        self.meta.get(idx)
    }

    pub fn idx_of_seq(&self, seq: u64) -> Option<usize> {
        let idx = seq.checked_sub(self.first_seq)? as usize;
        (idx < self.meta.len()).then_some(idx)
    }

    pub fn raw_text(&self, m: &RecordMeta) -> &str {
        self.arena.get(m.raw).unwrap_or("<已淘汰>")
    }

    pub fn msg_text(&self, m: &RecordMeta) -> &str {
        self.arena.get(m.msg).unwrap_or("<已淘汰>")
    }

    pub fn arena_bytes(&self) -> u64 {
        self.arena.live_bytes()
    }

    fn build_meta(
        &mut self,
        raw: &str,
        p: &ParsedLine,
        source: u16,
        fallback_ts_us: i64,
    ) -> RecordMeta {
        let span: SpanRef = self.arena.push(raw);
        let msg: SpanRef = match &p.msg_owned {
            Some(owned) => self.arena.push(owned),
            None => {
                let start = p.msg_range.0.min(raw.len());
                let end = p.msg_range.1.clamp(start, raw.len());
                SpanRef {
                    offset: span.offset + start as u64,
                    len: (end - start) as u32,
                }
            }
        };
        let (ts, mut fl) = match p.ts {
            Some(t) => (t, 0u8),
            None => (fallback_ts_us, flags::TS_SYNTHETIC),
        };
        if p.parsed {
            fl |= flags::PARSED;
        }
        RecordMeta {
            ts,
            tz_offset_min: p.tz_offset_min,
            source,
            host: self.syms.intern(p.host),
            app: self.syms.intern(p.app),
            tag: self.syms.intern(p.tag),
            pid: p.pid.unwrap_or(PID_NONE),
            level: p.level.min(7),
            facility: p.facility,
            flags: fl,
            raw: span,
            msg,
        }
    }

    fn count_in(&mut self, m: &RecordMeta) {
        self.level_counts[m.level as usize & 7] += 1;
        *self.tag_counts.entry(m.tag).or_insert(0) += 1;
        *self.host_counts.entry(m.host).or_insert(0) += 1;
        *self.app_counts.entry(m.app).or_insert(0) += 1;
        if !m.is_parsed() {
            self.unparsed_count += 1;
        }
    }

    fn count_out(&mut self, m: &RecordMeta) {
        self.level_counts[m.level as usize & 7] =
            self.level_counts[m.level as usize & 7].saturating_sub(1);
        if let Some(c) = self.tag_counts.get_mut(&m.tag) {
            *c = c.saturating_sub(1);
        }
        if let Some(c) = self.host_counts.get_mut(&m.host) {
            *c = c.saturating_sub(1);
        }
        if let Some(c) = self.app_counts.get_mut(&m.app) {
            *c = c.saturating_sub(1);
        }
        if !m.is_parsed() {
            self.unparsed_count = self.unparsed_count.saturating_sub(1);
        }
    }

    /// 尾部追加一条，返回其 seq。
    pub fn append(
        &mut self,
        raw: &str,
        p: &ParsedLine,
        source: u16,
        fallback_ts_us: i64,
    ) -> u64 {
        let m = self.build_meta(raw, p, source, fallback_ts_us);
        self.count_in(&m);
        self.meta.push_back(m);
        self.next_seq() - 1
    }

    /// 按 ts 在尾部窗口内回插（用于合并时间线 / 乱序网络流）。
    /// 返回插入位置的下标（注意：之后的行下标会变，调用方应在批后重建视图尾部）。
    pub fn append_sorted(
        &mut self,
        raw: &str,
        p: &ParsedLine,
        source: u16,
        fallback_ts_us: i64,
    ) -> usize {
        let m = self.build_meta(raw, p, source, fallback_ts_us);
        self.count_in(&m);
        let len = self.meta.len();
        let window_start = len.saturating_sub(REORDER_WINDOW);
        // 在 [window_start, len) 内找第一个 ts > m.ts 的位置（稳定：相同 ts 排后）
        let mut lo = window_start;
        let mut hi = len;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.meta[mid].ts <= m.ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.meta.insert(lo, m);
        lo
    }

    /// 应用保留策略，返回本次淘汰的行数。
    pub fn enforce_limits(&mut self) -> usize {
        let mut evicted = 0usize;
        while self.meta.len() > self.limits.max_rows
            || (self.arena.live_bytes() > self.limits.max_bytes && self.meta.len() > 1)
        {
            if let Some(m) = self.meta.pop_front() {
                self.count_out(&m);
                self.first_seq += 1;
                self.evicted_rows += 1;
                evicted += 1;
            } else {
                break;
            }
            // 字节上限的回收以块为粒度，分批检查避免每行都扫窗口
            if evicted % 1024 == 0 {
                self.evict_arena_prefix();
            }
        }
        if evicted > 0 {
            self.evict_arena_prefix();
        }
        evicted
    }

    /// 清空工作集（保留 sources / 符号表）。
    pub fn clear(&mut self) {
        let n = self.meta.len() as u64;
        self.meta.clear();
        self.first_seq += n;
        self.arena = Arena::default();
        self.level_counts = [0; 8];
        self.tag_counts.clear();
        self.host_counts.clear();
        self.app_counts.clear();
        self.unparsed_count = 0;
    }

    /// 计算前部窗口内最小存活 raw 偏移，淘汰其之前的整块。
    /// （append_sorted 只在尾部窗口乱序，但保险起见对前部窗口取最小值。）
    fn evict_arena_prefix(&mut self) {
        let probe = self.meta.len().min(REORDER_WINDOW);
        let mut min_off = u64::MAX;
        for i in 0..probe {
            min_off = min_off.min(self.meta[i].raw.offset);
        }
        if probe == 0 {
            min_off = self.arena.next_offset();
        }
        self.arena.evict_before(min_off);
    }

    /// 二分定位：第一个 ts >= 目标 的下标（工作集按 ts 基本有序时用于时间跳转）。
    pub fn lower_bound_ts(&self, ts_us: i64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.meta.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.meta[mid].ts < ts_us {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(msg_range: (usize, usize), ts: i64) -> ParsedLine<'static> {
        ParsedLine {
            ts: Some(ts),
            tz_offset_min: 0,
            host: "dev1",
            app: "app",
            tag: "i2c",
            pid: Some(117),
            level: 6,
            facility: 16,
            msg_range,
            msg_owned: None,
            parsed: true,
        }
    }

    #[test]
    fn append_and_read_back() {
        let mut s = LogStore::new();
        let src = s.add_source("file:test");
        let raw = "2026-06-12T10:16:41.834+00:00 dev1 app[117] info i2c: hello";
        let p = parsed((54, 59), 1_000_000);
        let seq = s.append(raw, &p, src, 0);
        assert_eq!(s.len(), 1);
        let m = *s.meta_by_seq(seq).unwrap();
        assert_eq!(s.raw_text(&m), raw);
        assert_eq!(s.msg_text(&m), "hello");
        assert_eq!(s.syms.get(m.host), "dev1");
        assert_eq!(s.syms.get(m.tag), "i2c");
        assert_eq!(m.pid, 117);
        assert!(m.is_parsed());
        assert_eq!(s.level_counts[6], 1);
    }

    #[test]
    fn unparsed_fallback_keeps_raw() {
        let mut s = LogStore::new();
        let src = s.add_source("file:test");
        let raw = "@@@ garbage that matches nothing";
        let p = ParsedLine::unparsed(raw);
        let seq = s.append(raw, &p, src, 42);
        let m = *s.meta_by_seq(seq).unwrap();
        assert!(!m.is_parsed());
        assert!(m.ts_is_synthetic());
        assert_eq!(m.ts, 42);
        assert_eq!(s.msg_text(&m), raw);
        assert_eq!(s.unparsed_count, 1);
    }

    #[test]
    fn eviction_by_rows_updates_seq_and_counts() {
        let mut s = LogStore::new();
        s.limits = RetainLimits {
            max_rows: 100,
            max_bytes: u64::MAX,
        };
        let src = s.add_source("udp:test");
        for i in 0..250 {
            let raw = format!("line {i}");
            let p = parsed((0, raw.len()), i);
            s.append(&raw, &p, src, 0);
            s.enforce_limits();
        }
        assert_eq!(s.len(), 100);
        assert_eq!(s.evicted_rows, 150);
        let (first, end) = s.seq_range();
        assert_eq!(first, 150);
        assert_eq!(end, 250);
        // 旧 seq 不可达，新 seq 可达
        assert!(s.meta_by_seq(149).is_none());
        assert!(s.meta_by_seq(150).is_some());
        // facet 计数与工作集一致
        assert_eq!(s.level_counts[6], 100);
        // 淘汰后最旧存活行仍可读
        let m = *s.meta_by_seq(150).unwrap();
        assert_eq!(s.raw_text(&m), "line 150");
    }

    #[test]
    fn eviction_by_bytes() {
        let mut s = LogStore::new();
        s.limits = RetainLimits {
            max_rows: usize::MAX,
            max_bytes: 4096,
        };
        let src = s.add_source("udp:test");
        for i in 0..1000 {
            let raw = format!("0123456789012345678901234567890123456789 {i}");
            let p = parsed((0, 4), i);
            s.append(&raw, &p, src, 0);
        }
        s.enforce_limits();
        assert!(s.len() < 1000);
        assert!(s.arena_bytes() <= 4096 + (1 << 20)); // 块粒度回收允许一个块的余量
        // 尾部行完好
        let last = *s.meta_at(s.len() - 1).unwrap();
        assert!(s.raw_text(&last).ends_with("999"));
    }

    #[test]
    fn append_sorted_reorders_within_window() {
        let mut s = LogStore::new();
        let src = s.add_source("udp:test");
        let ts_list = [100i64, 300, 200, 500, 400, 50];
        for (i, ts) in ts_list.iter().enumerate() {
            let raw = format!("m{i}");
            let p = parsed((0, raw.len()), *ts);
            s.append_sorted(&raw, &p, src, 0);
        }
        let got: Vec<i64> = (0..s.len()).map(|i| s.meta_at(i).unwrap().ts).collect();
        assert_eq!(got, vec![50, 100, 200, 300, 400, 500]);
    }

    #[test]
    fn append_sorted_is_stable_for_equal_ts() {
        let mut s = LogStore::new();
        let src = s.add_source("udp:test");
        for i in 0..5 {
            let raw = format!("same{i}");
            let p = parsed((0, raw.len()), 100);
            s.append_sorted(&raw, &p, src, 0);
        }
        for i in 0..5 {
            let m = *s.meta_at(i).unwrap();
            assert_eq!(s.raw_text(&m), format!("same{i}"));
        }
    }

    #[test]
    fn lower_bound_ts_finds_position() {
        let mut s = LogStore::new();
        let src = s.add_source("file:test");
        for ts in [10i64, 20, 30, 40, 50] {
            let raw = format!("t{ts}");
            let p = parsed((0, raw.len()), ts);
            s.append(&raw, &p, src, 0);
        }
        assert_eq!(s.lower_bound_ts(5), 0);
        assert_eq!(s.lower_bound_ts(30), 2);
        assert_eq!(s.lower_bound_ts(31), 3);
        assert_eq!(s.lower_bound_ts(99), 5);
    }

    #[test]
    fn clear_resets_but_seq_monotonic() {
        let mut s = LogStore::new();
        let src = s.add_source("file:test");
        for i in 0..10 {
            let raw = format!("x{i}");
            let p = parsed((0, raw.len()), i);
            s.append(&raw, &p, src, 0);
        }
        s.clear();
        assert_eq!(s.len(), 0);
        let seq = {
            let raw = "after-clear";
            let p = parsed((0, raw.len()), 999);
            s.append(raw, &p, src, 0)
        };
        assert_eq!(seq, 10); // seq 不回退
    }
}
