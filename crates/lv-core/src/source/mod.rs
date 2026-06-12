//! 输入源插件层（需求 §8）：file / udp / …，统一产出"行流"。
//!
//! 每个源在独立线程运行，经有界 channel 向 ingest 线程发送 `SourceEvent`。
//! 文件源用阻塞 send（读盘有背压）；UDP 源用 try_send + 丢弃计数（FR-S2）。

pub mod file;
pub mod udp;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::Receiver;

/// 一行原始日志（尚未解析）。
#[derive(Clone, Debug)]
pub struct RawLine {
    pub text: String,
    /// 到达本机的时间（epoch 微秒），无内容时间戳时作回退排序键。
    pub recv_ts_us: i64,
    /// UDP 来源地址（解析不出 HOSTNAME 时的 host 回退）。
    pub peer: Option<std::net::IpAddr>,
}

#[derive(Debug)]
pub enum SourceEvent {
    Lines(Vec<RawLine>),
    /// 初始加载完成（文件源；follow 模式下之后仍会继续发 Lines）。
    LoadDone { total_lines: u64 },
    Error(String),
}

/// 源线程的控制句柄。drop 时自动停止。
pub struct SourceHandle {
    pub rx: Receiver<SourceEvent>,
    pub stop: Arc<AtomicBool>,
    pub received: Arc<AtomicU64>,
    /// 因队列满被丢弃的行数（仅 UDP 会增长；显式提示，不静默）。
    pub dropped: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

impl SourceHandle {
    pub fn new(
        rx: Receiver<SourceEvent>,
        stop: Arc<AtomicBool>,
        received: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
        join: JoinHandle<()>,
    ) -> Self {
        Self {
            rx,
            stop,
            received,
            dropped,
            join: Some(join),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 当前时间（epoch 微秒）。
pub fn now_us() -> i64 {
    chrono::Utc::now().timestamp_micros()
}
