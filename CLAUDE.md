# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概览

`logviewer` 是 `uf_log` 日志系统的桌面查看端（Rust + egui）：实时接收嵌入式设备经 UDP 发来的
RFC5424 日志，打开本地日志文件（含 `.gz` 轮转产物），提供过滤、搜索、高亮、多标签、合并时间线
与仪表盘。也是通用的 RFC5424 / RFC3164 / JSON 行日志查看器。需要 Rust 1.85+。

用户母语为简体中文，代码注释与文档均为中文 —— 保持一致。

## 常用命令

```sh
cargo build --release                  # 产物 target/release/logviewer(.exe)
cargo test                             # 单元 + 端到端（workspace 全量）
cargo test -p lv-core parse            # 跑某 crate 中名字含 "parse" 的测试
cargo test --release -p lv-core --test perf -- --ignored --nocapture   # 性能验证（默认 #[ignore]）

# 联调工具（examples，非二进制）
cargo run -p lv-core --example gen_logs -- sample.log 100000 mixed     # 生成样例日志
cargo run -p lv-core --example udp_send -- 127.0.0.1:514 100 10000 simdev   # 模拟 UDP 设备
```

CI（`.github/workflows/ci.yml`）在 ubuntu/windows/macos 三平台跑 `cargo build --workspace` +
`cargo test --workspace` + release 构建。Linux 需要 GTK/xcb GUI 依赖（见 CI 文件）。

## 架构

两个 crate 的 workspace，**核心库无任何 UI 依赖**，UI 层只调用核心库的公开接口：

- **`crates/lv-core`** —— 解析、输入源、存储、过滤/搜索/高亮/合并/归档/导出。
- **`crates/lv-app`** —— egui 桌面应用（`logviewer` 二进制），标签页、虚拟化表格、仪表盘、i18n、会话。

### 数据流（关键，需读多个文件才能理解）

```
source 线程  →(有界 channel)→  ingest 线程  →  LogStore (Arc<Mutex>)  ←每帧读→  UI
   每个源一线程              每个 Tab 一线程        +归档 +转发给合并 Tab
```

- **`source/`**（`source/mod.rs`）：每个输入源在独立线程跑，经有界 `crossbeam-channel` 发
  `SourceEvent`。文件源用阻塞 send（读盘背压）；**UDP 源用 `try_send` + 丢弃计数**，超载显式
  计数显示在状态栏，绝不静默丢弃（`SourceHandle.dropped`）。这是「源插件点」：新增源实现同款
  `SourceHandle` 行流接口即可。
- **`ingest.rs`**：每个 Tab 一条 ingest 线程，消费 `SourceEvent` → `parse_auto` → 写入
  `LogStore`。`store` 用 `Arc<Mutex<LogStore>>` 与 UI 共享：UI 每帧短暂加锁读可见区，ingest
  按批加锁写。`IngestOpts.live` 区分实时流（无时间戳回退到接收时间）与文件（回退到邻行时间戳保
  持文件顺序）。合并 Tab 通过 `Taps` 订阅成员 Tab 的行。
- **`parse/`**：`parse_auto`（`parse/detect.rs`）按**行首字节**分派候选解析器（`<`→RFC5424/3164，
  `{`→JSON，数字→uf_log 模板，字母→无 PRI 的 RFC3164），全部失败回退 `ParsedLine::unparsed`
  ——**绝不丢行**，未解析行按原文显示并标记。新增格式在 `detect.rs` 注册。
- **`store.rs` / `arena.rs` / `symbols.rs`**：紧凑列存。定长 `RecordMeta` + 分块字符串 arena +
  符号驻留（tag/host/app 去重）。环形保留上限（`RetainLimits`，默认 ~120 万行 / 768MB），超限丢
  最旧；**归档不受影响（磁盘留全量）**。append-only 设计。
- **`view.rs`**：显示层。乱序网络日志的时间窗重排由 `view::TabView::sort_by_ts` 负责（不改存储顺序）。

### 扩展点

新增日志源 → 实现 `source` 同款接口；新增解析格式 → `parse/detect.rs` 注册；新增导出格式 →
`export.rs` 枚举；高亮规则包 / 过滤器是可分享的 JSON。

## 注意事项

- **egui/eframe 锁定 0.34.x**（`egui_plot` 为 0.35）。升级时注意 0.34 的 API 变更。
- **PowerShell 默认 UTF-16 输出**：生成给其它工具读的文件时用 `-Encoding utf8`，否则 Rust 侧读取乱码。
- 所有正则走 `regex` crate（线性时间、无回溯），天然防 ReDoS —— 不要引入回溯型正则库。
- 数据/配置位置：会话设置 `%APPDATA%\logviewer\session.json`（Linux/macOS `~/.config/logviewer/`）；
  UDP 归档默认 `%LOCALAPPDATA%\logviewer\archive\`（Linux/macOS `~/.local/share/logviewer/archive/`）。
- 需求与设计详见 `docs/00-requirements.md`（FR-* / R* / §8 编号在代码注释中被大量引用）。
