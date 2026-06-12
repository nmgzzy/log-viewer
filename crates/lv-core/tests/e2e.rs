//! 端到端集成测试：完整数据通路闭环（源 → 解析 → 存储 → 过滤/合并/
//! 归档 → 重新打开归档）。

use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lv_core::archive::{archive_files, ArchiveConfig, ArchiveSplit};
use lv_core::filter::{CompiledFilter, FilterSpec};
use lv_core::ingest::{spawn_ingest, IngestOpts};
use lv_core::merge::{merge_snapshot, MergeInput};
use lv_core::parse::ParserCtx;
use lv_core::source::file::{expand_and_order, spawn as spawn_file, FileSourceConfig};
use lv_core::source::udp::{spawn as spawn_udp, UdpSourceConfig};
use lv_core::store::LogStore;
use lv_core::view::TabView;

fn wait_for<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn default_opts(source_id: u16, live: bool) -> IngestOpts {
    IngestOpts {
        source_id,
        live,
        ctx: ParserCtx::default(),
        archive: None,
        paused: Arc::new(AtomicBool::new(false)),
    }
}

/// UDP 接收 → 入库 → 归档 → 重启后重新打开归档，行数与内容一致（FR-3 闭环）。
#[test]
fn e2e_udp_ingest_archive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let n = 5000u64;

    // 1) 启动 UDP 源 + 归档 ingest
    let (handle, local) = spawn_udp(UdpSourceConfig {
        bind: "127.0.0.1".into(),
        port: 0,
    })
    .unwrap();
    let store = Arc::new(Mutex::new(LogStore::new()));
    let src = store.lock().unwrap().add_source("udp:test");
    let mut opts = default_opts(src, true);
    opts.archive = Some(ArchiveConfig {
        dir: dir.path().into(),
        prefix: "udp".into(),
        split: ArchiveSplit::Unified,
        max_file_bytes: 256 << 10, // 触发若干次轮转
        keep_rotated: 64,
        ..Default::default()
    });
    let mut ingest = spawn_ingest(handle.rx.clone(), store.clone(), opts);

    // 2) 发送 n 条（两个"设备"）
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    for i in 0..n {
        let host = if i % 2 == 0 { "devA" } else { "devB" };
        let lvl = if i % 100 == 0 { 131 } else { 134 };
        let msg = format!(
            "<{lvl}>1 2026-06-12T10:{:02}:{:02}.{:03}Z {host} app {} i2c - event #{i}",
            (i / 3600) % 60,
            (i / 60) % 60,
            i % 1000,
            100 + i % 7
        );
        sender.send_to(msg.as_bytes(), local).unwrap();
        if i % 500 == 0 {
            std::thread::sleep(Duration::from_millis(1)); // 避免瞬时淹没 loopback
        }
    }

    // 3) 等待全部入库
    assert!(
        wait_for(Duration::from_secs(15), || store.lock().unwrap().len() == n as usize),
        "只收到 {}/{n}",
        store.lock().unwrap().len()
    );
    {
        let s = store.lock().unwrap();
        // host 维度区分（FR-S2 多设备）
        let hosts: Vec<&str> = s
            .host_counts
            .iter()
            .filter(|(_, c)| **c > 0)
            .map(|(id, _)| s.syms.get(*id))
            .collect();
        assert!(hosts.contains(&"devA") && hosts.contains(&"devB"));
        // err 计数正确
        assert_eq!(s.level_counts[3], (n / 100) as u64);
    }
    // 无超载丢弃（有界队列容量足够本速率）
    assert_eq!(handle.dropped.load(Ordering::Relaxed), 0);

    // 4) 停源 → ingest 退出并落盘
    drop(handle);
    ingest.join();

    // 5) "重启"：把归档（含轮转）按时间序重新打开
    let files = archive_files(dir.path(), "udp");
    assert!(files.len() > 1, "应有轮转产物，实际 {}", files.len());
    let mut h2 = spawn_file(FileSourceConfig {
        paths: files,
        follow: false,
    });
    let store2 = Arc::new(Mutex::new(LogStore::new()));
    let src2 = store2.lock().unwrap().add_source("archive");
    let mut ingest2 = spawn_ingest(h2.rx.clone(), store2.clone(), default_opts(src2, false));
    assert!(wait_for(Duration::from_secs(15), || {
        ingest2.stats.load_done.load(Ordering::Relaxed)
    }));
    h2.stop();
    ingest2.join();
    let s2 = store2.lock().unwrap();
    assert_eq!(s2.len(), n as usize, "归档重开行数不一致");
    // 第一条与最后一条原文完整保留
    let first = s2.raw_text(s2.meta_at(0).unwrap());
    assert!(first.contains("event #0"), "{first}");
    let last = s2.raw_text(s2.meta_at(s2.len() - 1).unwrap());
    assert!(last.contains(&format!("event #{}", n - 1)), "{last}");
}

/// 文件 + .gz 轮转集 → 目录打开 → 按时间序拼接 → 过滤（FR-S1/FR-4 闭环）。
#[test]
fn e2e_rotated_dir_load_and_filter() {
    let dir = tempfile::tempdir().unwrap();
    // messages.2.gz（最旧）/ messages.1 / messages（最新）
    {
        let f = std::fs::File::create(dir.path().join("messages.2.gz")).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        for i in 0..100 {
            writeln!(
                enc,
                "2026-06-12T09:00:{:02}.000+00:00 dev1 app[1] info boot: oldest {i}",
                i % 60
            )
            .unwrap();
        }
        enc.finish().unwrap();
    }
    {
        let mut f = std::fs::File::create(dir.path().join("messages.1")).unwrap();
        for i in 0..100 {
            writeln!(
                f,
                "2026-06-12T10:00:{:02}.000+00:00 dev1 app[1] warning i2c: middle {i}",
                i % 60
            )
            .unwrap();
        }
    }
    {
        let mut f = std::fs::File::create(dir.path().join("messages")).unwrap();
        for i in 0..100 {
            writeln!(
                f,
                "2026-06-12T11:00:{:02}.000+00:00 dev1 app[1] err net: newest {i}",
                i % 60
            )
            .unwrap();
        }
    }

    let files = expand_and_order(&[dir.path().to_path_buf()]);
    let mut h = spawn_file(FileSourceConfig {
        paths: files,
        follow: false,
    });
    let store = Arc::new(Mutex::new(LogStore::new()));
    let src = store.lock().unwrap().add_source("dir");
    let mut ingest = spawn_ingest(h.rx.clone(), store.clone(), default_opts(src, false));
    assert!(wait_for(Duration::from_secs(10), || {
        ingest.stats.load_done.load(Ordering::Relaxed)
    }));
    h.stop();
    ingest.join();

    let s = store.lock().unwrap();
    assert_eq!(s.len(), 300);
    // 顺序：最旧在前
    assert!(s.raw_text(s.meta_at(0).unwrap()).contains("oldest 0"));
    assert!(s.raw_text(s.meta_at(299).unwrap()).contains("newest 99"));

    // 过滤：仅 err
    let mut spec = FilterSpec::default();
    spec.set_min_severity(3);
    spec.show_unparsed = false;
    let cf = CompiledFilter::compile(spec, &s);
    let mut view = TabView::new(false);
    view.rebuild(&s, &cf);
    assert_eq!(view.len(), 100);
    // 过滤 + tag 维度
    let mut spec2 = FilterSpec::default();
    spec2.include_tags = vec!["i2c".into()];
    let cf2 = CompiledFilter::compile(spec2, &s);
    let mut view2 = TabView::new(false);
    view2.rebuild(&s, &cf2);
    assert_eq!(view2.len(), 100);
}

/// 双源合并：快照 + 实时 taps → 统一时间线（FR-8 闭环，模拟 app 的合并流程）。
#[test]
fn e2e_merge_snapshot_plus_live() {
    // 两个"文件 Tab"已各有 3 行
    let mk = |name: &str, base: &str| {
        let store = Arc::new(Mutex::new(LogStore::new()));
        let src = store.lock().unwrap().add_source(name);
        let ctx = ParserCtx::default();
        for i in 0..3 {
            let line = format!(
                "<134>1 2026-06-12T10:00:0{}Z {base} app 1 t - {base} snap {i}",
                i * 2 + if base == "devA" { 0 } else { 1 }
            );
            let p = lv_core::parse::parse_auto(&line, &ctx);
            store.lock().unwrap().append(&line, &p, src, 0);
        }
        store
    };
    let store_a = mk("A", "devA");
    let store_b = mk("B", "devB");

    // 成员 ingest（仅作为 taps 载体，接实时流）
    let (tx_a, rx_a) = crossbeam_channel::unbounded();
    let (tx_b, rx_b) = crossbeam_channel::unbounded();
    let ia = spawn_ingest(rx_a, store_a.clone(), default_opts(0, true));
    let ib = spawn_ingest(rx_b, store_b.clone(), default_opts(0, true));

    // 合并：快照
    let target = Arc::new(Mutex::new(LogStore::new()));
    {
        let ga = store_a.lock().unwrap();
        let gb = store_b.lock().unwrap();
        let mut t = target.lock().unwrap();
        merge_snapshot(
            &mut t,
            &[
                MergeInput { store: &ga, name: "A".into() },
                MergeInput { store: &gb, name: "B".into() },
            ],
            &ParserCtx::default(),
        );
        assert_eq!(t.len(), 6);
        // 快照按 ts 交错
        let hosts: Vec<&str> = (0..6).map(|i| t.syms.get(t.meta_at(i).unwrap().host)).collect();
        assert_eq!(hosts, vec!["devA", "devB", "devA", "devB", "devA", "devB"]);
    }
    // 订阅实时流
    let (tap_tx, tap_rx) = crossbeam_channel::unbounded();
    ia.taps.lock().unwrap().push(tap_tx.clone());
    ib.taps.lock().unwrap().push(tap_tx);
    let src_m = target.lock().unwrap().add_source("merged-live");
    let im = spawn_ingest(tap_rx, target.clone(), default_opts(src_m, true));

    // 两源各来一条实时（乱序 ts），应都进入合并 store
    use lv_core::source::{RawLine, SourceEvent};
    tx_b.send(SourceEvent::Lines(vec![RawLine {
        text: "<134>1 2026-06-12T10:00:08Z devB app 1 t - live b".into(),
        recv_ts_us: 1,
        peer: None,
    }]))
    .unwrap();
    tx_a.send(SourceEvent::Lines(vec![RawLine {
        text: "<134>1 2026-06-12T10:00:07Z devA app 1 t - live a".into(),
        recv_ts_us: 2,
        peer: None,
    }]))
    .unwrap();
    assert!(wait_for(Duration::from_secs(5), || target.lock().unwrap().len() == 8));

    // 显示层按 ts 排序（TabView sort_by_ts）
    let t = target.lock().unwrap();
    let cf = CompiledFilter::compile(FilterSpec::default(), &t);
    let mut view = TabView::new(true);
    view.rebuild(&t, &cf);
    let msgs: Vec<String> = view
        .seqs
        .iter()
        .map(|&q| t.msg_text(t.meta_by_seq(q).unwrap()).to_owned())
        .collect();
    assert_eq!(msgs[6], "live a"); // 10:00:07
    assert_eq!(msgs[7], "live b"); // 10:00:08
    drop(t);
    drop((tx_a, tx_b));
    drop((ia, ib, im));
}

/// 混合格式 + 超长行 + 非法 UTF-8 不崩溃、不丢行（§7.3 可靠性）。
#[test]
fn e2e_hostile_input_no_crash_no_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hostile.log");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "<134>1 2026-06-12T10:00:01Z h a 1 t - normal").unwrap();
        writeln!(f, "{}", "x".repeat(2_000_000)).unwrap(); // 2MB 超长行
        f.write_all(b"\xff\xfe\xfd binary garbage \x00\x01\n").unwrap();
        writeln!(f, "{{\"msg\": \"json line\", \"level\": \"err\"}}").unwrap();
        writeln!(f, "<999>1 bad pri").unwrap();
        writeln!(f).unwrap(); // 空行
        writeln!(f, "Jun 12 10:00:02 host proc[1]: rfc3164 line").unwrap();
    }
    let mut h = spawn_file(FileSourceConfig {
        paths: vec![path],
        follow: false,
    });
    let store = Arc::new(Mutex::new(LogStore::new()));
    let src = store.lock().unwrap().add_source("hostile");
    let mut ingest = spawn_ingest(h.rx.clone(), store.clone(), default_opts(src, false));
    assert!(wait_for(Duration::from_secs(10), || {
        ingest.stats.load_done.load(Ordering::Relaxed)
    }));
    h.stop();
    ingest.join();
    let s = store.lock().unwrap();
    assert_eq!(s.len(), 7, "一行都不能丢");
    // 超长行完整保留
    let long = s.raw_text(s.meta_at(1).unwrap());
    assert_eq!(long.len(), 2_000_000);
    // JSON 与 RFC3164 解析成功，垃圾行回退 raw
    assert!(s.meta_at(3).unwrap().is_parsed());
    assert!(s.meta_at(6).unwrap().is_parsed());
    assert!(!s.meta_at(2).unwrap().is_parsed());
    assert_eq!(s.unparsed_count, 4); // 超长行/垃圾/bad pri/空行
}
