//! 文件源（FR-S1/S3）：单文件 / 多文件 / 目录展开；.gz 自动解压；
//! 轮转产物按时间序拼接（messages.2.gz → messages.1 → messages）；
//! follow 模式跟随追加与轮转切换，不丢行不重读。

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Sender};
use flate2::read::GzDecoder;

use super::{now_us, RawLine, SourceEvent, SourceHandle};

const BATCH_LINES: usize = 8192;
const CHANNEL_CAP: usize = 16;
const FOLLOW_POLL: Duration = Duration::from_millis(150);

#[derive(Clone, Debug)]
pub struct FileSourceConfig {
    /// 已按时间序排列的文件列表（用 `expand_and_order` 生成）。
    pub paths: Vec<PathBuf>,
    /// 跟随最后一个（非 .gz）文件的追加与轮转。
    pub follow: bool,
}

/// 把文件/目录混合输入展开为按轮转序（最旧在前）排列的文件列表。
pub fn expand_and_order(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in inputs {
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let fp = e.path();
                    if fp.is_file() {
                        files.push(fp);
                    }
                }
            }
        } else {
            files.push(p.clone());
        }
    }
    order_rotation(&mut files);
    files
}

/// 轮转序：同一 base 的 `base.N[.gz]` 按 N 降序（最旧在前），`base` 殿后。
pub fn order_rotation(files: &mut [PathBuf]) {
    files.sort_by_key(|p| {
        let (base, n) = rotation_key(p);
        (base, std::cmp::Reverse(n))
    });
}

fn rotation_key(p: &Path) -> (String, i64) {
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".gz").unwrap_or(&name);
    if let Some(dot) = stem.rfind('.') {
        if let Ok(n) = stem[dot + 1..].parse::<i64>() {
            return (stem[..dot].to_owned(), n);
        }
    }
    (stem.to_owned(), -1) // 当前文件（无序号）最新
}

pub fn is_gz(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gz"))
}

/// 启动文件源线程。
pub fn spawn(cfg: FileSourceConfig) -> SourceHandle {
    let (tx, rx) = bounded::<SourceEvent>(CHANNEL_CAP);
    let stop = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let stop2 = stop.clone();
    let received2 = received.clone();
    let join = std::thread::Builder::new()
        .name("lv-file-source".into())
        .spawn(move || run(cfg, tx, stop2, received2))
        .expect("spawn file source thread");
    SourceHandle::new(rx, stop, received, dropped, join)
}

fn run(
    cfg: FileSourceConfig,
    tx: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
    received: Arc<AtomicU64>,
) {
    let mut total: u64 = 0;
    let mut batch: Vec<RawLine> = Vec::with_capacity(BATCH_LINES);
    let mut tail_pos: u64 = 0; // 最后一个文件消费到的字节数（follow 用）

    let last_idx = cfg.paths.len().saturating_sub(1);
    for (idx, path) in cfg.paths.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let consumed = read_whole(
            path,
            &tx,
            &stop,
            &received,
            &mut total,
            &mut batch,
        );
        if idx == last_idx {
            tail_pos = consumed;
        }
    }
    flush(&tx, &mut batch);
    let _ = tx.send(SourceEvent::LoadDone { total_lines: total });

    // follow：跟随最后一个非 gz 文件
    if cfg.follow {
        if let Some(path) = cfg.paths.last() {
            if !is_gz(path) {
                follow_loop(path, tail_pos, &tx, &stop, &received, &mut total);
            }
        }
    }
}

/// 整读一个文件（.gz 自动解压），返回消费的字节数（非 gz 文件）。
fn read_whole(
    path: &Path,
    tx: &Sender<SourceEvent>,
    stop: &AtomicBool,
    received: &AtomicU64,
    total: &mut u64,
    batch: &mut Vec<RawLine>,
) -> u64 {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(SourceEvent::Error(format!(
                "打开失败 {}: {e}",
                path.display()
            )));
            return 0;
        }
    };
    let mut consumed: u64 = 0;
    if is_gz(path) {
        let mut reader = BufReader::with_capacity(256 << 10, GzDecoder::new(file));
        read_lines_from(&mut reader, tx, stop, received, total, batch, &mut consumed);
        u64::MAX // gz 不参与 follow
    } else {
        let mut reader = BufReader::with_capacity(256 << 10, file);
        read_lines_from(&mut reader, tx, stop, received, total, batch, &mut consumed);
        consumed
    }
}

fn read_lines_from<R: BufRead>(
    reader: &mut R,
    tx: &Sender<SourceEvent>,
    stop: &AtomicBool,
    received: &AtomicU64,
    total: &mut u64,
    batch: &mut Vec<RawLine>,
    consumed: &mut u64,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                *consumed += n as u64;
                push_line(&buf, batch, received, total);
                if batch.len() >= BATCH_LINES {
                    flush(tx, batch);
                }
            }
            Err(e) => {
                let _ = tx.send(SourceEvent::Error(format!("读取失败: {e}")));
                break;
            }
        }
    }
}

fn push_line(buf: &[u8], batch: &mut Vec<RawLine>, received: &AtomicU64, total: &mut u64) {
    let mut end = buf.len();
    while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
        end -= 1;
    }
    let text = String::from_utf8_lossy(&buf[..end]).into_owned();
    batch.push(RawLine {
        text,
        recv_ts_us: now_us(),
        peer: None,
    });
    received.fetch_add(1, Ordering::Relaxed);
    *total += 1;
}

fn flush(tx: &Sender<SourceEvent>, batch: &mut Vec<RawLine>) {
    if !batch.is_empty() {
        let _ = tx.send(SourceEvent::Lines(std::mem::take(batch)));
        *batch = Vec::with_capacity(BATCH_LINES);
    }
}

/// tail -f：轮询文件增长；长度回退视为轮转，重新从头读新文件（不丢行不重读）。
fn follow_loop(
    path: &Path,
    mut pos: u64,
    tx: &Sender<SourceEvent>,
    stop: &AtomicBool,
    received: &AtomicU64,
    total: &mut u64,
) {
    let mut partial: Vec<u8> = Vec::new();
    let mut batch: Vec<RawLine> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(FOLLOW_POLL);
        let len = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => continue, // 轮转间隙文件可能短暂不存在
        };
        if len < pos {
            // 轮转/截断：从头读新文件
            pos = 0;
            partial.clear();
        }
        if len == pos {
            continue;
        }
        let Ok(mut f) = File::open(path) else { continue };
        if f.seek(SeekFrom::Start(pos)).is_err() {
            continue;
        }
        let mut reader = BufReader::with_capacity(64 << 10, f.take(len - pos));
        let mut buf: Vec<u8> = Vec::with_capacity(512);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    pos += n as u64;
                    if buf.ends_with(b"\n") {
                        if partial.is_empty() {
                            push_line(&buf, &mut batch, received, total);
                        } else {
                            partial.extend_from_slice(&buf);
                            push_line(&partial, &mut batch, received, total);
                            partial.clear();
                        }
                    } else {
                        // 行未写完整，攒着等下个轮询
                        partial.extend_from_slice(&buf);
                    }
                }
                Err(_) => break,
            }
        }
        flush(tx, &mut batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rotation_order_oldest_first() {
        let mut files = vec![
            PathBuf::from("d/messages"),
            PathBuf::from("d/messages.2.gz"),
            PathBuf::from("d/messages.1"),
            PathBuf::from("d/messages.10.gz"),
        ];
        order_rotation(&mut files);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["messages.10.gz", "messages.2.gz", "messages.1", "messages"]
        );
    }

    #[test]
    fn reads_plain_and_gz_in_order() {
        let dir = tempfile::tempdir().unwrap();
        // messages.1.gz（旧） + messages（新）
        let gz_path = dir.path().join("messages.1.gz");
        {
            let f = File::create(&gz_path).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            writeln!(enc, "old line 1").unwrap();
            writeln!(enc, "old line 2").unwrap();
            enc.finish().unwrap();
        }
        let plain = dir.path().join("messages");
        std::fs::write(&plain, "new line 1\nnew line 2\n").unwrap();

        let files = expand_and_order(&[dir.path().to_path_buf()]);
        let mut h = spawn(FileSourceConfig {
            paths: files,
            follow: false,
        });
        let mut lines = Vec::new();
        let mut done = false;
        while !done {
            match h.rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                SourceEvent::Lines(ls) => lines.extend(ls.into_iter().map(|l| l.text)),
                SourceEvent::LoadDone { total_lines } => {
                    assert_eq!(total_lines, 4);
                    done = true;
                }
                SourceEvent::Error(e) => panic!("error: {e}"),
            }
        }
        assert_eq!(
            lines,
            vec!["old line 1", "old line 2", "new line 1", "new line 2"]
        );
        h.stop();
    }

    #[test]
    fn crlf_and_invalid_utf8_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.log");
        let mut f = File::create(&p).unwrap();
        f.write_all(b"win line\r\n\xff\xfe bad utf8\nlast").unwrap();
        drop(f);
        let mut h = spawn(FileSourceConfig {
            paths: vec![p],
            follow: false,
        });
        let mut lines = Vec::new();
        loop {
            match h.rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                SourceEvent::Lines(ls) => lines.extend(ls.into_iter().map(|l| l.text)),
                SourceEvent::LoadDone { .. } => break,
                SourceEvent::Error(e) => panic!("{e}"),
            }
        }
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "win line");
        assert!(lines[1].contains("bad utf8"));
        assert_eq!(lines[2], "last");
        h.stop();
    }

    #[test]
    fn follow_picks_up_appends_and_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages");
        std::fs::write(&p, "first\n").unwrap();
        let mut h = spawn(FileSourceConfig {
            paths: vec![p.clone()],
            follow: true,
        });
        // 初始加载
        let mut lines: Vec<String> = Vec::new();
        loop {
            match h.rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                SourceEvent::Lines(ls) => lines.extend(ls.into_iter().map(|l| l.text)),
                SourceEvent::LoadDone { .. } => break,
                SourceEvent::Error(e) => panic!("{e}"),
            }
        }
        assert_eq!(lines, vec!["first"]);

        // 追加（含分两次写入的半行）
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(b"second\npart-").unwrap();
            f.flush().unwrap();
        }
        std::thread::sleep(Duration::from_millis(400));
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(b"ial\n").unwrap();
        }
        let mut got: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.len() < 2 && std::time::Instant::now() < deadline {
            if let Ok(SourceEvent::Lines(ls)) = h.rx.recv_timeout(Duration::from_millis(200)) {
                got.extend(ls.into_iter().map(|l| l.text));
            }
        }
        assert_eq!(got, vec!["second", "part-ial"]);

        // 轮转：新文件更短 → 从头读
        std::fs::write(&p, "after-rotate\n").unwrap();
        let mut got2: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got2.is_empty() && std::time::Instant::now() < deadline {
            if let Ok(SourceEvent::Lines(ls)) = h.rx.recv_timeout(Duration::from_millis(200)) {
                got2.extend(ls.into_iter().map(|l| l.text));
            }
        }
        assert_eq!(got2, vec!["after-rotate"]);
        h.stop();
    }
}
