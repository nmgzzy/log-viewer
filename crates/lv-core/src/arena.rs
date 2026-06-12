//! 分块字符串 Arena：所有原始日志行按追加顺序存入定长块，记录只持有
//! (全局偏移, 长度)。淘汰从最旧的整块开始，保证内存不随行数线性失控。

use std::collections::VecDeque;

use crate::model::SpanRef;

const DEFAULT_CHUNK_CAP: usize = 1 << 20; // 1 MiB

struct Chunk {
    start: u64,
    data: Vec<u8>,
}

pub struct Arena {
    chunks: VecDeque<Chunk>,
    next_offset: u64,
    chunk_cap: usize,
}

impl Default for Arena {
    fn default() -> Self {
        Self::with_chunk_cap(DEFAULT_CHUNK_CAP)
    }
}

impl Arena {
    pub fn with_chunk_cap(chunk_cap: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            next_offset: 0,
            chunk_cap: chunk_cap.max(64),
        }
    }

    /// 追加一行，返回其引用。行不会跨块存储；超过块容量的行独占一个块。
    pub fn push(&mut self, s: &str) -> SpanRef {
        let bytes = s.as_bytes();
        let need = bytes.len();
        let fits = match self.chunks.back() {
            Some(c) => c.data.len() + need <= c.data.capacity(),
            None => false,
        };
        if !fits {
            let cap = self.chunk_cap.max(need);
            self.chunks.push_back(Chunk {
                start: self.next_offset,
                data: Vec::with_capacity(cap),
            });
        }
        let chunk = self.chunks.back_mut().expect("chunk just ensured");
        let offset = chunk.start + chunk.data.len() as u64;
        chunk.data.extend_from_slice(bytes);
        self.next_offset = offset + need as u64;
        SpanRef {
            offset,
            len: need as u32,
        }
    }

    /// 取回字符串。引用已被淘汰时返回 None（调用方应显示降级文案）。
    pub fn get(&self, span: SpanRef) -> Option<&str> {
        if span.len == 0 {
            return Some("");
        }
        let idx = self
            .chunks
            .partition_point(|c| c.start + c.data.len() as u64 <= span.offset);
        let chunk = self.chunks.get(idx)?;
        if span.offset < chunk.start {
            return None;
        }
        let begin = (span.offset - chunk.start) as usize;
        let end = begin + span.len as usize;
        if end > chunk.data.len() {
            return None;
        }
        // 写入时即为合法 UTF-8 且不跨块，安全。
        std::str::from_utf8(&chunk.data[begin..end]).ok()
    }

    /// 淘汰完全位于 `offset` 之前的整块。
    pub fn evict_before(&mut self, offset: u64) {
        while let Some(front) = self.chunks.front() {
            if front.start + front.data.len() as u64 <= offset {
                self.chunks.pop_front();
            } else {
                break;
            }
        }
    }

    /// 当前驻留字节数（含块内未用容量不计）。
    pub fn live_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.data.len() as u64).sum()
    }

    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_get_roundtrip() {
        let mut a = Arena::with_chunk_cap(64);
        let r1 = a.push("hello");
        let r2 = a.push("世界，你好");
        assert_eq!(a.get(r1), Some("hello"));
        assert_eq!(a.get(r2), Some("世界，你好"));
    }

    #[test]
    fn line_never_splits_across_chunks() {
        let mut a = Arena::with_chunk_cap(64);
        // 填满第一块后，长行应进入新块且完整可读
        let mut refs = Vec::new();
        for i in 0..100 {
            refs.push((a.push(&format!("line-{i:04}")), format!("line-{i:04}")));
        }
        for (r, expect) in &refs {
            assert_eq!(a.get(*r), Some(expect.as_str()));
        }
    }

    #[test]
    fn oversized_line_gets_own_chunk() {
        let mut a = Arena::with_chunk_cap(64);
        let big = "x".repeat(1000);
        let r = a.push(&big);
        assert_eq!(a.get(r), Some(big.as_str()));
    }

    #[test]
    fn evict_drops_whole_chunks_only() {
        let mut a = Arena::with_chunk_cap(64);
        let mut refs = Vec::new();
        for i in 0..50 {
            refs.push(a.push(&format!("0123456789-{i:03}"))); // 14 字节/行
        }
        let mid = refs[25];
        a.evict_before(mid.offset);
        // mid 之前的部分块被回收，mid 自身仍可读
        assert!(a.get(refs[0]).is_none());
        assert!(a.get(mid).is_some());
        assert!(a.get(refs[49]).is_some());
        assert!(a.live_bytes() < 50 * 14);
    }

    #[test]
    fn empty_line_ok() {
        let mut a = Arena::default();
        let r = a.push("");
        assert_eq!(a.get(r), Some(""));
    }
}
