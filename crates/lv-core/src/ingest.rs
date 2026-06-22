//! Ingest 线程：源事件 → 解析 → 入库（+归档 +转发给合并 Tab）。
//!
//! 每个 Tab 一条 ingest 线程，store 用 `Arc<Mutex<LogStore>>` 与 UI 共享；
//! UI 每帧短暂加锁读取可见区，ingest 按批加锁写入。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::archive::{ArchiveConfig, ArchiveWriter};
use crate::model::ParsedLine;
use crate::parse::{parse_auto, ParserCtx};
use crate::source::{RawLine, SourceEvent};
use crate::store::LogStore;

/// `IngestStats.errors` 保留的最近错误条数上限（防止长跑无界增长）。
const MAX_ERRORS: usize = 200;

/// 追加一条错误信息，超过上限时丢弃最旧（环形）。
fn push_error(stats: &IngestStats, msg: String) {
    let mut v = stats.errors.lock().unwrap();
    if v.len() >= MAX_ERRORS {
        v.remove(0);
    }
    v.push(msg);
}

pub struct IngestOpts {
    pub source_id: u16,
    /// true：实时流（UDP/合并），无内容时间戳的行回退到接收时间；
    /// false：文件，回退到邻行时间戳保持文件顺序。
    /// 显示层的乱序重排由 `view::TabView::sort_by_ts` 负责。
    pub live: bool,
    pub ctx: ParserCtx,
    /// UDP 自动落盘归档（FR-3）。
    pub archive: Option<ArchiveConfig>,
    /// 暂停：暂停期间不入内存工作集（继续归档，丢弃计入 skipped_paused）。
    pub paused: Arc<AtomicBool>,
}

/// ingest 运行状态（UI 状态栏展示）。
#[derive(Default)]
pub struct IngestStats {
    pub appended: AtomicU64,
    pub evicted: AtomicU64,
    pub skipped_paused: AtomicU64,
    pub archived: AtomicU64,
    pub archive_errors: AtomicU64,
    pub load_done: AtomicBool,
    pub errors: Mutex<Vec<String>>,
}

/// 合并 Tab 的订阅出口：成员 Tab 的 ingest 把行转发给这些 sender。
pub type Taps = Arc<Mutex<Vec<Sender<SourceEvent>>>>;

pub struct IngestHandle {
    pub stats: Arc<IngestStats>,
    pub taps: Taps,
    join: Option<JoinHandle<()>>,
}

impl IngestHandle {
    /// 等待线程退出（源 channel 断开后自然结束）。
    pub fn join(&mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for IngestHandle {
    fn drop(&mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn spawn_ingest(
    rx: Receiver<SourceEvent>,
    store: Arc<Mutex<LogStore>>,
    opts: IngestOpts,
) -> IngestHandle {
    let stats = Arc::new(IngestStats::default());
    let taps: Taps = Arc::new(Mutex::new(Vec::new()));
    let stats2 = stats.clone();
    let taps2 = taps.clone();
    let join = std::thread::Builder::new()
        .name("lv-ingest".into())
        .spawn(move || run(rx, store, opts, stats2, taps2))
        .expect("spawn ingest thread");
    IngestHandle {
        stats,
        taps,
        join: Some(join),
    }
}

fn run(
    rx: Receiver<SourceEvent>,
    store: Arc<Mutex<LogStore>>,
    opts: IngestOpts,
    stats: Arc<IngestStats>,
    taps: Taps,
) {
    let mut archive = opts.archive.as_ref().and_then(|cfg| {
        match ArchiveWriter::new(cfg.clone()) {
            Ok(w) => Some(w),
            Err(e) => {
                push_error(&stats, format!("归档初始化失败: {e}"));
                None
            }
        }
    });

    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(SourceEvent::Lines(lines)) => {
                ingest_batch(&lines, &store, &opts, &stats, &mut archive);
                forward_taps(&taps, lines);
            }
            Ok(SourceEvent::LoadDone { .. }) => {
                stats.load_done.store(true, Ordering::Relaxed);
            }
            Ok(SourceEvent::Error(e)) => {
                push_error(&stats, e);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(a) = archive.as_mut() {
                    a.flush();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(a) = archive.as_mut() {
        a.flush();
    }
}

fn ingest_batch(
    lines: &[RawLine],
    store: &Arc<Mutex<LogStore>>,
    opts: &IngestOpts,
    stats: &IngestStats,
    archive: &mut Option<ArchiveWriter>,
) {
    let paused = opts.paused.load(Ordering::Relaxed);
    // 暂停且无归档：无事可做（不解析、不入库）
    if paused && archive.is_none() {
        stats
            .skipped_paused
            .fetch_add(lines.len() as u64, Ordering::Relaxed);
        return;
    }
    // 解析一次，归档与入库共用（此前两条路径各 parse_auto 一次）。
    // peer 回退主机名单独拥有所有权，供两处按引用复用。
    let parsed: Vec<ParsedLine> = lines.iter().map(|l| parse_auto(&l.text, &opts.ctx)).collect();
    let peer_hosts: Vec<Option<String>> =
        lines.iter().map(|l| l.peer.map(|ip| ip.to_string())).collect();

    // 归档独立于暂停：磁盘上留全的
    if let Some(a) = archive.as_mut() {
        for ((line, p), peer) in lines.iter().zip(&parsed).zip(&peer_hosts) {
            let host: &str = if !p.host.is_empty() {
                p.host
            } else if let Some(ps) = peer {
                ps.as_str()
            } else {
                ""
            };
            a.write_line(host, &line.text);
        }
        stats.archived.store(a.written_lines, Ordering::Relaxed);
        stats.archive_errors.store(a.write_errors, Ordering::Relaxed);
    }
    if paused {
        stats
            .skipped_paused
            .fetch_add(lines.len() as u64, Ordering::Relaxed);
        return;
    }
    let mut s = store.lock().unwrap();
    let mut last_ts = s
        .meta_at(s.len().saturating_sub(1))
        .map(|m| m.ts)
        .unwrap_or(0);
    for ((line, mut p), peer) in lines.iter().zip(parsed).zip(&peer_hosts) {
        if p.parsed && p.host.is_empty() {
            if let Some(ps) = peer {
                p.host = ps.as_str();
            }
        }
        // 无内容时间戳的回退：文件按邻行保持顺序，网络按接收时间
        let fallback = if opts.live || last_ts == 0 {
            line.recv_ts_us
        } else {
            last_ts
        };
        s.append(&line.text, &p, opts.source_id, fallback);
        if let Some(m) = s.meta_at(s.len() - 1) {
            last_ts = last_ts.max(m.ts);
        }
    }
    let evicted = s.enforce_limits();
    drop(s);
    stats.appended.fetch_add(lines.len() as u64, Ordering::Relaxed);
    stats.evicted.fetch_add(evicted as u64, Ordering::Relaxed);
}

fn forward_taps(taps: &Taps, lines: Vec<RawLine>) {
    let mut guard = taps.lock().unwrap();
    if guard.is_empty() {
        return;
    }
    guard.retain(|tx| tx.send(SourceEvent::Lines(lines.clone())).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveSplit;

    fn opts(source_id: u16, live: bool) -> IngestOpts {
        IngestOpts {
            source_id,
            live,
            ctx: ParserCtx::default(),
            archive: None,
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    fn line(text: &str, ts: i64) -> RawLine {
        RawLine {
            text: text.into(),
            recv_ts_us: ts,
            peer: None,
        }
    }

    #[test]
    fn ingest_parses_and_appends() {
        let store = Arc::new(Mutex::new(LogStore::new()));
        let src = store.lock().unwrap().add_source("test");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut h = spawn_ingest(rx, store.clone(), opts(src, false));
        tx.send(SourceEvent::Lines(vec![
            line("<134>1 2026-06-12T10:16:41.834Z dev1 app 117 i2c - hello", 0),
            line("not parseable garbage", 0),
        ]))
        .unwrap();
        tx.send(SourceEvent::LoadDone { total_lines: 2 }).unwrap();
        drop(tx);
        h.join();
        let s = store.lock().unwrap();
        assert_eq!(s.len(), 2);
        let m0 = *s.meta_at(0).unwrap();
        assert!(m0.is_parsed());
        assert_eq!(s.syms.get(m0.host), "dev1");
        let m1 = *s.meta_at(1).unwrap();
        assert!(!m1.is_parsed());
        // 未解析行回退到邻行时间戳，保持文件顺序
        assert_eq!(m1.ts, m0.ts);
        assert!(h.stats.load_done.load(Ordering::Relaxed));
    }

    #[test]
    fn peer_ip_as_host_fallback() {
        let store = Arc::new(Mutex::new(LogStore::new()));
        let src = store.lock().unwrap().add_source("udp");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut h = spawn_ingest(rx, store.clone(), opts(src, true));
        tx.send(SourceEvent::Lines(vec![RawLine {
            text: "<13>1 - - - - - - no host here".into(),
            recv_ts_us: 1000,
            peer: Some("192.168.1.7".parse().unwrap()),
        }]))
        .unwrap();
        drop(tx);
        h.join();
        let s = store.lock().unwrap();
        let m = *s.meta_at(0).unwrap();
        assert_eq!(s.syms.get(m.host), "192.168.1.7");
        assert_eq!(m.ts, 1000); // 接收时间回退
    }

    #[test]
    fn live_mode_appends_in_arrival_order_and_archives() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(LogStore::new()));
        let src = store.lock().unwrap().add_source("udp");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut o = opts(src, true);
        o.archive = Some(ArchiveConfig {
            dir: dir.path().into(),
            prefix: "udp".into(),
            split: ArchiveSplit::Unified,
            ..Default::default()
        });
        let mut h = spawn_ingest(rx, store.clone(), o);
        // 乱序到达
        tx.send(SourceEvent::Lines(vec![
            line("<134>1 2026-06-12T10:00:02Z d a 1 t - second", 0),
            line("<134>1 2026-06-12T10:00:01Z d a 1 t - first", 0),
            line("<134>1 2026-06-12T10:00:03Z d a 1 t - third", 0),
        ]))
        .unwrap();
        drop(tx);
        h.join();
        let s = store.lock().unwrap();
        // 存储保持到达顺序（seq 即身份）；显示层 TabView 负责按 ts 重排
        let msgs: Vec<String> = (0..s.len())
            .map(|i| s.msg_text(s.meta_at(i).unwrap()).to_owned())
            .collect();
        assert_eq!(msgs, vec!["second", "first", "third"]);
        // 归档按到达顺序原样保留
        let content = std::fs::read_to_string(dir.path().join("udp-all.log")).unwrap();
        let archived: Vec<&str> = content.lines().collect();
        assert_eq!(archived.len(), 3);
        assert!(archived[0].ends_with("second"));
        assert_eq!(h.stats.archived.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn paused_skips_store_but_archives() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(LogStore::new()));
        let src = store.lock().unwrap().add_source("udp");
        let (tx, rx) = crossbeam_channel::unbounded();
        let paused = Arc::new(AtomicBool::new(true));
        let mut o = opts(src, true);
        o.paused = paused.clone();
        o.archive = Some(ArchiveConfig {
            dir: dir.path().into(),
            prefix: "udp".into(),
            split: ArchiveSplit::Unified,
            ..Default::default()
        });
        let mut h = spawn_ingest(rx, store.clone(), o);
        tx.send(SourceEvent::Lines(vec![line("paused line", 1)]))
            .unwrap();
        drop(tx);
        h.join();
        assert_eq!(store.lock().unwrap().len(), 0);
        assert_eq!(h.stats.skipped_paused.load(Ordering::Relaxed), 1);
        let content = std::fs::read_to_string(dir.path().join("udp-all.log")).unwrap();
        assert_eq!(content, "paused line\n");
    }

    #[test]
    fn errors_vec_is_capped() {
        let stats = IngestStats::default();
        for i in 0..(MAX_ERRORS + 50) {
            push_error(&stats, format!("err {i}"));
        }
        let v = stats.errors.lock().unwrap();
        // 长度封顶，且保留的是最近的错误（最旧被丢弃）
        assert_eq!(v.len(), MAX_ERRORS);
        assert_eq!(v.last().unwrap(), &format!("err {}", MAX_ERRORS + 49));
        assert_eq!(v.first().unwrap(), &format!("err {}", 50));
    }

    #[test]
    fn taps_receive_forwarded_lines() {
        let store = Arc::new(Mutex::new(LogStore::new()));
        let src = store.lock().unwrap().add_source("test");
        let (tx, rx) = crossbeam_channel::unbounded();
        let h = spawn_ingest(rx, store.clone(), opts(src, false));
        let (tap_tx, tap_rx) = crossbeam_channel::unbounded();
        h.taps.lock().unwrap().push(tap_tx);
        tx.send(SourceEvent::Lines(vec![line("forwarded", 1)]))
            .unwrap();
        let got = tap_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        match got {
            SourceEvent::Lines(ls) => assert_eq!(ls[0].text, "forwarded"),
            _ => panic!("期望 Lines"),
        }
        drop(tx);
    }
}
