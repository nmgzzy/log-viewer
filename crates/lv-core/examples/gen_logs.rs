//! 生成样例/压测日志文件。
//! 用法：gen_logs <输出文件> [行数] [格式: uf|5424|mixed]

use std::io::Write;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str).unwrap_or("sample.log");
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let format = args.get(3).map(String::as_str).unwrap_or("mixed");

    let tags = ["i2c", "uart", "spi", "net", "boot", "pwr", "sensor", "fs"];
    let apps = ["sensord", "netmgr", "kernel", "updater"];
    let hosts = ["dev1", "dev2", "dev3"];
    let levels = [
        (3, "err"),
        (4, "warning"),
        (6, "info"),
        (6, "info"),
        (6, "info"),
        (7, "debug"),
        (7, "debug"),
    ];
    let msgs = [
        "bus timeout on read addr=0x3c",
        "rx overflow, dropped 12 bytes",
        "probe ok, 4 devices found",
        "link up 100Mbps full-duplex",
        "temperature 47.5C within range",
        "config reloaded from /etc/uf.conf",
        "watchdog kicked",
        "checksum mismatch, retrying",
    ];

    let f = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
    let base_ms: u64 = 1_781_258_000_000; // 2026-06-12 前后
    for i in 0..n {
        let (lv, lvname) = levels[i % levels.len()];
        let tag = tags[i % tags.len()];
        let app = apps[i % apps.len()];
        let host = hosts[i % hosts.len()];
        let msg = msgs[i % msgs.len()];
        let ms = base_ms + (i as u64) * 7;
        let secs = ms / 1000;
        let frac = ms % 1000;
        let dt = chrono::DateTime::from_timestamp(secs as i64, (frac * 1_000_000) as u32)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3f+00:00");
        let pid = 100 + (i % 23);
        let use_5424 = format == "5424" || (format == "mixed" && i % 3 == 0);
        if use_5424 {
            let pri = 128 + lv; // local0
            let dt5424 = chrono::DateTime::from_timestamp(secs as i64, (frac * 1_000_000) as u32)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ");
            writeln!(w, "<{pri}>1 {dt5424} {host} {app} {pid} {tag} - {msg} #{i}")?;
        } else if app == "kernel" {
            writeln!(w, "{dt} {host} kernel[] {lvname} {tag}: {msg} #{i}")?;
        } else {
            writeln!(w, "{dt} {host} {app}[{pid}] {lvname} {tag}: {msg} #{i}")?;
        }
    }
    w.flush()?;
    eprintln!("wrote {n} lines to {path}");
    Ok(())
}
