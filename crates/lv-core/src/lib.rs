//! lv-core：日志查看器核心库。
//!
//! 负责解析（RFC5424 / uf_log 模板 / RFC3164 / JSON 行）、输入源（文件 / .gz /
//! 目录 / follow / UDP）、紧凑存储与保留策略、过滤 / 搜索 / 高亮规则 / 合并 /
//! 归档 / 导出。UI 层（lv-app）只依赖本库的公开接口。

pub mod arena;
pub mod archive;
pub mod export;
pub mod filter;
pub mod highlight;
pub mod ingest;
pub mod merge;
pub mod model;
pub mod parse;
pub mod search;
pub mod source;
pub mod store;
pub mod symbols;
pub mod view;

pub use model::{ParsedLine, RecordMeta, SpanRef, LEVEL_NAMES, PID_NONE};
pub use parse::{parse_auto, FieldMap, ParserCtx};
pub use store::{LogStore, RetainLimits};
pub use symbols::SymbolTable;
