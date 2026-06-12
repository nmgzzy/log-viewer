//! lv-core：日志查看器核心库。
//!
//! 负责解析（RFC5424 / uf_log 模板 / RFC3164 / JSON 行）、输入源（文件 / .gz /
//! 目录 / follow / UDP）、紧凑存储与保留策略、过滤 / 搜索 / 高亮规则 / 合并 /
//! 归档 / 导出。UI 层（lv-app）只依赖本库的公开接口。

pub mod arena;
pub mod model;
pub mod store;
pub mod symbols;

pub use model::{ParsedLine, RecordMeta, SpanRef, LEVEL_NAMES, PID_NONE};
pub use store::{LogStore, RetainLimits};
pub use symbols::SymbolTable;
