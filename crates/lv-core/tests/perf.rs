//! 性能与容量验证（§7.1）。默认 ignore，须以 release 显式运行：
//! `cargo test --release -p lv-core --test perf -- --ignored --nocapture`

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lv_core::filter::{CompiledFilter, FilterSpec, TextCond};
use lv_core::ingest::{spawn_ingest, IngestOpts};
use lv_core::model::RecordMeta;
use lv_core::parse::{parse_auto, ParserCtx};
use lv_core::search::{run_search, SearchSpec};
use lv_core::stats::compute_dash;
use lv_core::store::LogStore;
use lv_core::view::TabView;

fn gen_line(i: usize) -> String {
    let tags = ["i2c", "uart", "spi", "net", "boot", "pwr", "sensor", "fs"];
    let apps = ["sensord", "netmgr", "kernel", "updater"];
    let hosts = ["dev1", "dev2", "dev3"];
    let pri = match i % 20 {
        0 => 131,      // err
        1 | 2 => 132,  // warning
        3..=6 => 135,  // debug
        _ => 134,      // info
    };
    let ms = 1_781_258_000_000u64 + i as u64 * 3;
    let dt = chrono::DateTime::from_timestamp((ms / 1000) as i64, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ");
    format!(
        "<{pri}>1 {dt} {} {} {} {} - sensor reading value={} status ok iteration {i}",
        hosts[i % 3],
        apps[i % 4],
        100 + i % 23,
        tags[i % 8],
        i * 37 % 100000
    )
}

/// 百万行：加载（解析+入库）、过滤 ≤1s、搜索、内存 ≤500MB、仪表盘毫秒级。
#[test]
#[ignore]
fn perf_million_rows() {
    const N: usize = 1_000_000;
    let mut store = LogStore::new();
    let src = store.add_source("perf");
    let ctx = ParserCtx::default();

    // 加载（含解析）
    let t0 = Instant::now();
    for i in 0..N {
        let line = gen_line(i);
        let p = parse_auto(&line, &ctx);
        store.append(&line, &p, src, 0);
    }
    let load = t0.elapsed();
    println!("加载 {N} 行（含生成+解析+入库）: {load:.2?}");
    assert_eq!(store.len(), N);

    // 内存（数据结构核心占用）
    let meta_bytes = N * std::mem::size_of::<RecordMeta>();
    let arena_bytes = store.arena_bytes() as usize;
    let total_mb = (meta_bytes + arena_bytes) as f64 / 1e6;
    println!(
        "内存: meta {:.1}MB + arena {:.1}MB = {total_mb:.1}MB",
        meta_bytes as f64 / 1e6,
        arena_bytes as f64 / 1e6
    );
    assert!(total_mb < 500.0, "超出 500MB 预算: {total_mb:.1}MB");

    // 过滤：文本子串（最重路径）应 ≤ ~1s
    let spec = FilterSpec {
        texts: vec![TextCond::contains("value=4242")],
        ..Default::default()
    };
    let cf = CompiledFilter::compile(spec, &store);
    let t1 = Instant::now();
    let hits = cf.eval_full(&store);
    let filter_time = t1.elapsed();
    println!("文本过滤 {N} 行: {filter_time:.2?}，命中 {}", hits.len());
    assert!(filter_time < Duration::from_secs(1), "过滤超时: {filter_time:.2?}");
    assert!(!hits.is_empty());

    // 过滤：维度组合（level+tag）
    let mut spec2 = FilterSpec::default();
    spec2.set_min_severity(4);
    spec2.include_tags = vec!["i2c".into()];
    let cf2 = CompiledFilter::compile(spec2, &store);
    let t2 = Instant::now();
    let hits2 = cf2.eval_full(&store);
    println!("维度过滤: {:.2?}，命中 {}", t2.elapsed(), hits2.len());
    assert!(t2.elapsed() < Duration::from_secs(1));

    // 视图重建（全量通过）
    let cf3 = CompiledFilter::compile(FilterSpec::default(), &store);
    let mut view = TabView::new(false);
    let t3 = Instant::now();
    view.rebuild(&store, &cf3);
    println!("视图重建（全量）: {:.2?}", t3.elapsed());
    assert_eq!(view.len(), N);

    // 搜索（视图上正则）
    let t4 = Instant::now();
    let r = run_search(
        &store,
        &view.seqs,
        &SearchSpec {
            query: r"iteration 9999\d\d".into(),
            is_regex: true,
            case_sensitive: false,
        },
    );
    println!("正则搜索: {:.2?}，命中 {}", t4.elapsed(), r.hits.len());
    assert!(t4.elapsed() < Duration::from_secs(1));

    // 仪表盘统计
    let t5 = Instant::now();
    let d = compute_dash(&store, 120);
    println!(
        "仪表盘统计: {:.2?}（{} 桶, err率 {:.2}%）",
        t5.elapsed(),
        d.buckets.len(),
        d.err_rate * 100.0
    );
    assert!(t5.elapsed() < Duration::from_millis(500));
}

/// UDP 持续 50,000 行/秒 不丢（§7.1）。
#[test]
#[ignore]
fn perf_udp_50k_per_sec() {
    use lv_core::source::udp::{spawn as spawn_udp, UdpSourceConfig};
    use std::net::UdpSocket;

    const RATE: u64 = 50_000;
    const SECS: u64 = 3;
    const TOTAL: u64 = RATE * SECS;

    let (handle, local) = spawn_udp(UdpSourceConfig {
        bind: "127.0.0.1".into(),
        port: 0,
    })
    .unwrap();
    let store = Arc::new(Mutex::new(LogStore::new()));
    let src = store.lock().unwrap().add_source("udp");
    let ingest = spawn_ingest(
        handle.rx.clone(),
        store.clone(),
        IngestOpts {
            source_id: src,
            live: true,
            ctx: ParserCtx::default(),
            archive: None,
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    );

    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let start = Instant::now();
    let mut sent = 0u64;
    let line = "<134>1 2026-06-12T10:16:41.834Z dev1 sensord 117 i2c - sustained rate test message padding padding";
    while sent < TOTAL {
        let due = ((start.elapsed().as_micros() as u64) * RATE / 1_000_000 + 1).min(TOTAL);
        while sent < due {
            sender.send_to(line.as_bytes(), local).unwrap();
            sent += 1;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let send_time = start.elapsed();
    println!("发送 {TOTAL} 行用时 {send_time:.2?}（{:.0}/s）", TOTAL as f64 / send_time.as_secs_f64());

    // 等待消费完
    let ok = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = store.lock().unwrap().len() as u64;
            if got + handle.dropped.load(Ordering::Relaxed) >= TOTAL || Instant::now() > deadline {
                break got;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let dropped = handle.dropped.load(Ordering::Relaxed);
    let received = handle.received.load(Ordering::Relaxed);
    println!("入库 {ok}，源收到 {received}，队列丢弃 {dropped}");
    drop(ingest);
    // 有界队列不允许丢
    assert_eq!(dropped, 0, "背压队列出现丢弃");
    // loopback 上 OS 层偶发丢包给出 0.5% 容差（真实瓶颈在内核 socket 缓冲）
    assert!(
        received as f64 >= TOTAL as f64 * 0.995,
        "OS 层丢包过多: {received}/{TOTAL}"
    );
    assert!(ok as f64 >= TOTAL as f64 * 0.995);
}
