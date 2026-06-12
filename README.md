# Log Viewer / 日志查看器

`uf_log` 日志系统的桌面查看端：实时接收嵌入式设备经 **UDP** 发来的 RFC5424 日志，
打开/分析本地导出的日志文件（含 `.gz` 轮转产物），提供过滤、搜索、高亮、多标签、
合并时间线与轻量仪表盘。亦可作为通用 **RFC5424 / RFC3164 / JSON 行** 日志查看器。

A desktop log viewer for the `uf_log` logging system: receives RFC5424 logs from
embedded devices over **UDP** in real time, opens local log files (incl. rotated
`.gz`), with filtering, search, highlighting, multi-tab, merged timeline and a
lightweight dashboard. Also works as a generic RFC5424 / RFC3164 / JSON-lines viewer.

## 构建 / Build

需要 Rust 1.85+（[rustup](https://rustup.rs)）。Windows 10+ / macOS 12+ / Linux（x86_64 与 arm64）。

```sh
cargo build --release
# 产物 / binary: target/release/logviewer(.exe)
```

## 使用 / Usage

```sh
logviewer [--udp 端口] [文件或目录 ...]   # 可选：启动时直接打开文件 / 启动 UDP 监听
```

- **打开文件/目录**：`文件 → 打开文件…`（每个文件一个标签页）或 `打开目录…`
  （目录视为一个轮转集：`messages.2.gz → messages.1 → messages` 自动解压并按时间序拼接，
  并自动 tail -f 跟随最新文件）。
- **UDP 监听**：`文件 → UDP 监听…`，默认 `0.0.0.0:514`（Linux/macOS 上 <1024 端口需要
  root，可改用如 5514 并相应调整设备侧配置）。收到的日志**自动落盘归档**（可按主机分文件，
  按大小/时长轮转、保留份数可配）；重启后用"打开目录"重新打开归档继续分析。
- **过滤**（每标签页独立）：级别多选、tag/host/app 包含与排除（点击 facet 或表格右键
  "仅此值/排除此值"）、PID、时间范围、文本子串/正则（AND/OR、排除）。过滤器可命名保存、
  导入导出（顶栏 📁）。
- **搜索**：`Ctrl+F` 聚焦，`F3` / `Shift+F3` 下一处/上一处；与过滤解耦，命中行高亮。
- **高亮**：🎨 按钮打开规则编辑器；按消息/原文/tag/app/host 匹配子串或正则，设置前景/
  背景/粗体；规则有序、可启停、可导入导出（JSON 规则包）。
- **合并**：`文件 → 合并标签页…` 把多个文件/网络源合成统一时间线（按时间戳归并，
  乱序网络日志在显示层按时间窗重排），每行标注来源。
- **仪表盘**：📊 按钮；按时间桶的 level 计数曲线、错误率（可设阈值变红）、Top
  tag/app/host、当前接收速率；点击柱状图或 Top 项可下钻为过滤条件。
- **显示**：列可选/可调宽、紧凑/详情密度、长行换行/截断、时间显示
  绝对（原始/本地时区）/相对（首行/上一行）切换；单元格/整行复制。
- **导出**：💾 把当前过滤结果导出为 文本 / JSON 行 / CSV。
- **其它**：中英文界面切换、深浅色主题、会话持久化（重启恢复打开的标签页与视图状态）。

### 数据与配置位置 / Data locations

| 内容 | Windows | Linux/macOS |
|---|---|---|
| 会话与设置 | `%APPDATA%\logviewer\session.json` | `~/.config/logviewer/session.json` |
| UDP 归档默认目录 | `%LOCALAPPDATA%\logviewer\archive\` | `~/.local/share/logviewer/archive/` |

## 设备侧对接 / Device integration

设备 `syslog-ng.conf`（uf_log 默认即此格式）：

```
destination d_net { network("<上位机IP>" transport("udp") port(514) flags(syslog-protocol)); };
```

客户端在 514 端口监听即可。本地文件 `/var/log/uf/messages*` 导出到 PC 后用"打开目录"。

## 可靠性与性能 / Reliability & performance

- 解析失败的行**不丢弃**，按原文显示并标记"未解析"；混合格式逐行自动探测。
- UDP 有界队列 + 背压，超载丢弃**显式计数**显示在状态栏，绝不静默。
- 内存工作集环形上限（默认 120 万行 / 768MB 原文），超限滚动丢最旧并提示；
  **归档不受影响**（磁盘上留全的）。
- 实测（release，参见 `crates/lv-core/tests/perf.rs`）：百万行加载数秒、
  文本过滤/正则搜索 < 1s、虚拟化滚动恒定帧率、内存 < 500MB、UDP 持续 50k 行/秒不丢。
- 所有正则线性时间执行（`regex` crate，无回溯），天然防 ReDoS。

## 联调工具 / Dev tools

```sh
# 生成样例日志（混合 RFC5424 + uf_log 文件模板）
cargo run -p lv-core --example gen_logs -- sample.log 100000 mixed
# 模拟设备发送 UDP 日志（100 行/秒，共 10000 行）
cargo run -p lv-core --example udp_send -- 127.0.0.1:514 100 10000 simdev
```

## 测试 / Tests

```sh
cargo test                                                        # 单元 + 端到端
cargo test --release -p lv-core --test perf -- --ignored --nocapture   # 性能验证
```

## 架构 / Architecture

```
crates/lv-core   核心库（无 UI 依赖）
  parse/    RFC5424 | uf_log 模板 | RFC3164 | JSON 行 | 自动探测
  source/   file(.gz/目录/follow) | udp（有界队列+丢弃计数）   ← 源插件点
  store     紧凑列存：56B 定长 meta + 分块字符串 arena + 符号驻留
  ingest    源 → 解析 → 入库/归档/taps 转发（每标签页一线程）
  filter/search/highlight/merge/export/stats/view/archive
crates/lv-app    egui 桌面应用（标签页、虚拟化表格、仪表盘、i18n、会话）
```

扩展点（§8）：新增日志源实现 `source` 模块同款 `SourceHandle` 行流接口；新增解析格式在
`parse/detect.rs` 注册；导出器在 `export.rs` 扩展枚举；高亮规则包/过滤器为可分享的 JSON。
