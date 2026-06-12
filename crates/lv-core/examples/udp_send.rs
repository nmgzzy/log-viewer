//! 向查看器发送 RFC5424 UDP 日志（联调/压测用）。
//! 用法：udp_send <目标 ip:port> [行/秒] [总行数] [host名]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let target = args.get(1).map(String::as_str).unwrap_or("127.0.0.1:514");
    let rate: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let total: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let host = args.get(4).map(String::as_str).unwrap_or("simdev");

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let tags = ["i2c", "uart", "net", "pwr"];
    let levels = [134u16, 134, 134, 132, 131, 135]; // info x3, warning, err, debug
    let start = Instant::now();
    let mut sent = 0u64;
    while sent < total {
        // 按目标速率分批（每 10ms 一批）
        let due = (start.elapsed().as_millis() as u64) * rate / 1000 + 1;
        while sent < due.min(total) {
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
            let pri = levels[(sent % levels.len() as u64) as usize];
            let tag = tags[(sent % tags.len() as u64) as usize];
            let msg = format!(
                "<{pri}>1 {now} {host} sensord {} {tag} - simulated event #{sent} value={}",
                100 + sent % 17,
                sent * 37 % 1000
            );
            sock.send_to(msg.as_bytes(), target)?;
            sent += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!(
        "sent {sent} lines to {target} in {:.2}s",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
