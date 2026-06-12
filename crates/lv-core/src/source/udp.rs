//! UDP 源（FR-S2）：监听可配地址:端口，多设备同收（按 host 区分在解析层）。
//! 有界队列 + try_send：突发超载按策略丢弃并计数，绝不静默（§4 可靠性）。

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Sender, TrySendError};

use super::{now_us, RawLine, SourceEvent, SourceHandle};

/// 队列容量（事件数；每事件最多 RECV_BATCH 行）。
const CHANNEL_CAP: usize = 4096;
/// 每个事件聚合的行数上限（减少锁/唤醒次数）。
const RECV_BATCH: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct UdpSourceConfig {
    /// 绑定地址，默认 "0.0.0.0"。
    pub bind: String,
    /// 端口，默认 514。
    pub port: u16,
}

impl Default for UdpSourceConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".into(),
            port: 514,
        }
    }
}

/// 启动 UDP 监听。绑定失败立即返回错误（端口占用/权限）。
pub fn spawn(cfg: UdpSourceConfig) -> anyhow::Result<(SourceHandle, std::net::SocketAddr)> {
    let socket = UdpSocket::bind((cfg.bind.as_str(), cfg.port))?;
    let local = socket.local_addr()?;
    socket.set_read_timeout(Some(READ_TIMEOUT))?;

    let (tx, rx) = bounded::<SourceEvent>(CHANNEL_CAP);
    let stop = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let (stop2, received2, dropped2) = (stop.clone(), received.clone(), dropped.clone());
    let join = std::thread::Builder::new()
        .name(format!("lv-udp-{}", local.port()))
        .spawn(move || run(socket, tx, stop2, received2, dropped2))
        .expect("spawn udp source thread");
    Ok((SourceHandle::new(rx, stop, received, dropped, join), local))
}

fn run(
    socket: UdpSocket,
    tx: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
    received: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
) {
    let mut buf = [0u8; 64 << 10];
    let mut batch: Vec<RawLine> = Vec::with_capacity(RECV_BATCH);
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, peer)) => {
                let ts = now_us();
                let datagram = String::from_utf8_lossy(&buf[..n]);
                // 一般一报文一条；容忍多行报文
                for line in datagram.split('\n') {
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() {
                        continue;
                    }
                    received.fetch_add(1, Ordering::Relaxed);
                    batch.push(RawLine {
                        text: line.to_owned(),
                        recv_ts_us: ts,
                        peer: Some(peer.ip()),
                    });
                }
                if batch.len() >= RECV_BATCH {
                    send_batch(&tx, &mut batch, &dropped);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // 超时窗口：把攒着的小批发出去，保证低速率时延迟可控
                if !batch.is_empty() {
                    send_batch(&tx, &mut batch, &dropped);
                }
            }
            Err(_) => {
                // 套接字错误（如网卡变化）：不崩溃，继续重试
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn send_batch(tx: &Sender<SourceEvent>, batch: &mut Vec<RawLine>, dropped: &AtomicU64) {
    let n = batch.len() as u64;
    match tx.try_send(SourceEvent::Lines(std::mem::take(batch))) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            // 背压：消费端跟不上 → 丢弃本批并显式计数
            dropped.fetch_add(n, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
    if batch.capacity() == 0 {
        *batch = Vec::with_capacity(RECV_BATCH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receives_datagrams_with_peer() {
        let (mut h, local) = spawn(UdpSourceConfig {
            bind: "127.0.0.1".into(),
            port: 0, // 测试用临时端口
        })
        .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0..100 {
            let msg = format!("<134>1 2026-06-12T10:16:41.{i:03}Z dev app 1 t - line {i}");
            sender.send_to(msg.as_bytes(), local).unwrap();
        }
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.len() < 100 && std::time::Instant::now() < deadline {
            if let Ok(SourceEvent::Lines(ls)) = h.rx.recv_timeout(Duration::from_millis(300)) {
                got.extend(ls);
            }
        }
        assert_eq!(got.len(), 100);
        assert!(got.iter().all(|l| l.peer.is_some()));
        assert!(got[0].text.starts_with("<134>1"));
        assert_eq!(h.received.load(Ordering::Relaxed), 100);
        assert_eq!(h.dropped.load(Ordering::Relaxed), 0);
        h.stop();
    }

    #[test]
    fn multiline_datagram_split() {
        let (mut h, local) = spawn(UdpSourceConfig {
            bind: "127.0.0.1".into(),
            port: 0,
        })
        .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"line a\nline b\r\nline c\n", local).unwrap();
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.len() < 3 && std::time::Instant::now() < deadline {
            if let Ok(SourceEvent::Lines(ls)) = h.rx.recv_timeout(Duration::from_millis(300)) {
                got.extend(ls.into_iter().map(|l| l.text));
            }
        }
        assert_eq!(got, vec!["line a", "line b", "line c"]);
        h.stop();
    }

    #[test]
    fn bind_conflict_reports_error() {
        let (h, local) = spawn(UdpSourceConfig {
            bind: "127.0.0.1".into(),
            port: 0,
        })
        .unwrap();
        let err = spawn(UdpSourceConfig {
            bind: "127.0.0.1".into(),
            port: local.port(),
        });
        assert!(err.is_err());
        drop(h);
    }
}
