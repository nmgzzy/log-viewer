//! 网络日志自动落盘归档（FR-3，MVP 必须）。
//!
//! 收到的原始行原样写入归档文件（格式与设备侧一致，便于互通），
//! 支持按 host 分文件或统一文件、按大小/时长轮转、保留份数。
//! 重启后归档可作为普通文件重新打开分析。"内存里看最近的，磁盘上留全的"。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveSplit {
    /// 所有 host 写入同一个文件。
    Unified,
    /// 每个 host 一个文件。
    PerHost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveConfig {
    pub dir: PathBuf,
    /// 文件名前缀，如 "udp-514"。
    pub prefix: String,
    pub split: ArchiveSplit,
    /// 单文件大小上限，超过即轮转。
    pub max_file_bytes: u64,
    /// 单文件时长上限（秒），None 表示不按时长轮转。
    pub max_file_secs: Option<u64>,
    /// 轮转保留份数（不含当前文件）。
    pub keep_rotated: usize,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("archive"),
            prefix: "udp".into(),
            split: ArchiveSplit::Unified,
            max_file_bytes: 64 << 20, // 64 MiB
            max_file_secs: None,
            keep_rotated: 8,
        }
    }
}

struct Stream {
    writer: BufWriter<File>,
    path: PathBuf,
    bytes: u64,
    opened_at: Instant,
}

pub struct ArchiveWriter {
    cfg: ArchiveConfig,
    streams: HashMap<String, Stream>,
    pub written_lines: u64,
    pub write_errors: u64,
    last_flush: Instant,
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    if s.is_empty() {
        "unknown".into()
    } else {
        s
    }
}

impl ArchiveWriter {
    pub fn new(cfg: ArchiveConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        Ok(Self {
            cfg,
            streams: HashMap::new(),
            written_lines: 0,
            write_errors: 0,
            last_flush: Instant::now(),
        })
    }

    pub fn config(&self) -> &ArchiveConfig {
        &self.cfg
    }

    fn stream_key(&self, host: &str) -> String {
        match self.cfg.split {
            ArchiveSplit::Unified => "all".to_owned(),
            ArchiveSplit::PerHost => sanitize(if host.is_empty() { "unknown" } else { host }),
        }
    }

    fn base_path(&self, key: &str) -> PathBuf {
        self.cfg.dir.join(format!("{}-{}.log", self.cfg.prefix, key))
    }

    /// 写一行（自动补换行）。错误计数而不上抛——归档故障不能拖垮接收。
    pub fn write_line(&mut self, host: &str, line: &str) {
        let key = self.stream_key(host);
        if let Err(e) = self.write_line_inner(&key, line) {
            self.write_errors += 1;
            let _ = e; // 错误细节由计数体现；UI 显示 write_errors
        } else {
            self.written_lines += 1;
        }
        // 周期性落盘，避免崩溃丢太多
        if self.last_flush.elapsed() > Duration::from_secs(1) {
            self.flush();
        }
    }

    fn write_line_inner(&mut self, key: &str, line: &str) -> anyhow::Result<()> {
        let max_bytes = Self::max_bytes(&self.cfg);
        let max_secs = self.cfg.max_file_secs;
        let need_rotate = {
            let s = self.ensure_stream(key)?;
            let over_size = s.bytes + line.len() as u64 + 1 > max_bytes;
            let over_age =
                max_secs.is_some_and(|secs| s.opened_at.elapsed() >= Duration::from_secs(secs));
            (over_size || over_age) && s.bytes > 0
        };
        if need_rotate {
            self.rotate(key)?;
            self.ensure_stream(key)?;
        }
        let s = self.streams.get_mut(key).expect("stream ensured");
        s.writer.write_all(line.as_bytes())?;
        s.writer.write_all(b"\n")?;
        s.bytes += line.len() as u64 + 1;
        Ok(())
    }

    fn max_bytes(cfg: &ArchiveConfig) -> u64 {
        cfg.max_file_bytes.max(1024) // 防呆下限
    }

    fn ensure_stream(&mut self, key: &str) -> anyhow::Result<&mut Stream> {
        if !self.streams.contains_key(key) {
            let path = self.base_path(key);
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
            self.streams.insert(
                key.to_owned(),
                Stream {
                    writer: BufWriter::with_capacity(64 << 10, file),
                    path,
                    bytes,
                    opened_at: Instant::now(),
                },
            );
        }
        Ok(self.streams.get_mut(key).expect("just inserted"))
    }

    /// base.log → base.log.1 → … → base.log.N，超出保留数删除。
    fn rotate(&mut self, key: &str) -> anyhow::Result<()> {
        if let Some(mut s) = self.streams.remove(key) {
            let _ = s.writer.flush();
            drop(s);
        }
        let base = self.base_path(key);
        let keep = self.cfg.keep_rotated.max(1);
        let nth = |n: usize| -> PathBuf {
            PathBuf::from(format!("{}.{}", base.display(), n))
        };
        let oldest = nth(keep);
        if oldest.exists() {
            let _ = std::fs::remove_file(&oldest);
        }
        for n in (1..keep).rev() {
            let from = nth(n);
            if from.exists() {
                let _ = std::fs::rename(&from, nth(n + 1));
            }
        }
        if base.exists() {
            std::fs::rename(&base, nth(1))?;
        }
        Ok(())
    }

    pub fn flush(&mut self) {
        for s in self.streams.values_mut() {
            let _ = s.writer.flush();
        }
        self.last_flush = Instant::now();
    }

    /// 当前各流的归档文件路径（UI"打开归档"用）。
    pub fn current_paths(&self) -> Vec<PathBuf> {
        self.streams.values().map(|s| s.path.clone()).collect()
    }
}

/// 列出某归档流的全部文件（含轮转），按时间序（最旧在前）。
pub fn archive_files(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned());
            if let Some(n) = name {
                if n.starts_with(&format!("{prefix}-")) && n.contains(".log") {
                    files.push(p);
                }
            }
        }
    }
    crate::source::file::order_rotation(&mut files);
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_cfg(dir: PathBuf, split: ArchiveSplit) -> ArchiveConfig {
        ArchiveConfig {
            dir,
            prefix: "udp".into(),
            split,
            max_file_bytes: 1024,
            max_file_secs: None,
            keep_rotated: 3,
        }
    }

    #[test]
    fn unified_write_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ArchiveWriter::new(small_cfg(dir.path().into(), ArchiveSplit::Unified)).unwrap();
        w.write_line("dev1", "<134>1 2026-06-12T10:16:41Z dev1 a 1 t - hello");
        w.write_line("dev2", "<134>1 2026-06-12T10:16:42Z dev2 a 1 t - world");
        w.flush();
        let content = std::fs::read_to_string(dir.path().join("udp-all.log")).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("dev2"));
        assert_eq!(w.written_lines, 2);
        assert_eq!(w.write_errors, 0);
    }

    #[test]
    fn per_host_split() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ArchiveWriter::new(small_cfg(dir.path().into(), ArchiveSplit::PerHost)).unwrap();
        w.write_line("dev1", "line from dev1");
        w.write_line("dev/2", "line from dev2"); // 名字含非法字符
        w.write_line("", "line no host");
        w.flush();
        assert!(dir.path().join("udp-dev1.log").exists());
        assert!(dir.path().join("udp-dev_2.log").exists());
        assert!(dir.path().join("udp-unknown.log").exists());
    }

    #[test]
    fn rotation_and_retention() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ArchiveWriter::new(small_cfg(dir.path().into(), ArchiveSplit::Unified)).unwrap();
        let line = "x".repeat(100);
        for _ in 0..120 {
            // 120*101 字节 ≈ 12 KB，1KB 上限 → 多次轮转
            w.write_line("h", &line);
        }
        w.flush();
        let base = dir.path().join("udp-all.log");
        assert!(base.exists());
        assert!(dir.path().join("udp-all.log.1").exists());
        assert!(dir.path().join("udp-all.log.3").exists());
        // 保留 3 份：.4 不应存在
        assert!(!dir.path().join("udp-all.log.4").exists());
        // 每个轮转文件不超过上限（含一行余量）
        let len1 = std::fs::metadata(dir.path().join("udp-all.log.1")).unwrap().len();
        assert!(len1 <= 1024 + 101);
    }

    #[test]
    fn reopen_appends_not_truncates() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut w =
                ArchiveWriter::new(small_cfg(dir.path().into(), ArchiveSplit::Unified)).unwrap();
            w.write_line("h", "before restart");
            w.flush();
        }
        {
            let mut w =
                ArchiveWriter::new(small_cfg(dir.path().into(), ArchiveSplit::Unified)).unwrap();
            w.write_line("h", "after restart");
            w.flush();
        }
        let content = std::fs::read_to_string(dir.path().join("udp-all.log")).unwrap();
        assert_eq!(content, "before restart\nafter restart\n");
    }

    #[test]
    fn archive_files_listed_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("udp-all.log"), "new\n").unwrap();
        std::fs::write(dir.path().join("udp-all.log.1"), "mid\n").unwrap();
        std::fs::write(dir.path().join("udp-all.log.2"), "old\n").unwrap();
        std::fs::write(dir.path().join("other.txt"), "x\n").unwrap();
        let files = archive_files(dir.path(), "udp");
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["udp-all.log.2", "udp-all.log.1", "udp-all.log"]);
    }
}
