# uf 日志系统 —— 落地纪实 / 移植建议 / TODO

本文记录 uf 日志系统的**实际实现过程**（as-built）、**移植到真机的建议**、以及**待完成事项**。
设计与原理见 [`logging-system.md`](logging-system.md)（"应该怎么做"）；本文是"实际做了什么、如何复现、
还差什么"。

> 当前状态：**设计文档 §11 的 P0–P5 + C++ 键值接口已落地，并在 aarch64 QEMU 上端到端验证通过。**
> 仅余 P6（把 EmbedMQ 现有 logger 切到 uf_log，属另一个仓库 `/home/zzy/emq`）。

---

## 1. 架构回顾（一句话）

应用/用户态驱动调用 **`uf_log`**（C 库 `LOG_I()` / C++ 键值 `LOGKV_I().kv()`）→ 库内**过滤+净化+组装
RFC5424** → 写本地 `/dev/log` → **syslog-ng** 落本地文件（`/var/log/uf/messages`）+ UDP 转发上位机；
内核日志经 `/proc/kmsg` 汇入；**logrotate** 负责文件按 size 轮转、压缩、循环保留 N 份。

---

## 2. 已落地清单（仓库内实际文件）

### 2.1 库与源码 `br2-external/src/uf_log/`
| 文件 | 作用 |
|------|------|
| `uf_log.h` | C API + `LOG_E/W/I/D` 宏 + 编译期级别裁剪（`UF_LOG_MIN_LEVEL`） |
| `uf_log.c` | 实现：RFC5424 组装、`/dev/log` 非阻塞投递+应急回退、过滤管线、`fork` 自愈 |
| `uf_log.hpp` | header-only C++ 键值接口（`LOGKV_*`/`.kv()`，`if constexpr` 递归序列化） |
| `uf_logtest.c` | C 接口验证程序（覆盖各级别/净化/脱敏/丢弃/截断） |
| `uf_logkv_test.cpp` | C++ 键值接口验证程序（标量/string/vector/set/map/嵌套） |
| `Makefile` | 产出 `.so`+`.a`+头文件+两个测试程序；装 staging+target |

### 2.2 Buildroot 包 `br2-external/package/uf_log/`
`Config.in`（`BR2_PACKAGE_UF_LOG`，依赖 threads）+ `uf_log.mk`（`generic-package`，local 源码，
`INSTALL_STAGING=YES`）。已在 `br2-external/Config.in` 注册。

### 2.3 配置 overlay `br2-external/board/overlay/`
| 文件 | 作用 |
|------|------|
| `etc/syslog-ng.conf` | syslog-ng 主配置（覆盖包内默认的 `/etc/syslog-ng.conf`）：RFC5424 源 + 文件&UDP 双路 + 内核源 + 二级过滤 |
| `etc/uf_log/filter.conf` | uf_log 过滤规则（脱敏/丢弃） |
| `etc/logrotate.conf` | logrotate 主配置（`include /etc/logrotate.d`） |
| `etc/logrotate.d/uf` | uf 日志轮转规则（size/compress/rotate/postrotate reopen） |
| `etc/init.d/S01syslogd` | **置空（exit 0）**：停用 busybox syslogd，避免它 unlink 重建 `/dev/log` 抢占 |
| `etc/init.d/S02klogd` | **置空（exit 0）**：停用 busybox klogd，避免与 syslog-ng 争抢 `/proc/kmsg` |

### 2.4 `.config` 增量启用（5 个符号）
```
BR2_PACKAGE_BUSYBOX_SHOW_OTHERS=y   # syslog-ng 的依赖
BR2_PACKAGE_SYSLOG_NG=y             # 选入 json-c/libglib2/pcre2/openssl
BR2_PACKAGE_LOGROTATE=y
BR2_PACKAGE_UF_LOG=y
BR2_ROOTFS_OVERLAY="$(BR2_EXTERNAL_SIMU_PATH)/board/overlay"
```

### 2.5 验证脚本
`scripts/qemu-logging.py`（沿用 `qemu-test.py` 的 `pty.fork()`+`select` 串口驱动范式）。

---

## 3. 复现步骤（从干净工作树到验证）

```bash
# 1) 增量启用（严禁 defconfig；用 olddefconfig 解析 select 链）
{ echo 'BR2_PACKAGE_BUSYBOX_SHOW_OTHERS=y'
  echo 'BR2_PACKAGE_SYSLOG_NG=y'
  echo 'BR2_PACKAGE_LOGROTATE=y'
  echo 'BR2_PACKAGE_UF_LOG=y'
  echo 'BR2_ROOTFS_OVERLAY="$(BR2_EXTERNAL_SIMU_PATH)/board/overlay"'; } >> buildroot/.config
./build.sh olddefconfig

# 2) 全量构建（首次连带 glib2/openssl/pcre2/json-c/syslog-ng，主机交叉编译，约十余分钟）
./build.sh

# 3) 改动 uf_log 源码后的快速迭代
./build.sh rebuild uf_log    # 仅重编+装 uf_log
./build.sh                   # 重打 rootfs（改 overlay/配置也是这一步，秒级）

# 4) 验证
python3 scripts/qemu-logging.py   # 报告写到仓库根 qemu-logging.log
```

> 注意（CLAUDE.md 规则）：Buildroot 在 **x86 主机交叉编译**（不在 TCG 里），故构建是主机速度；
> 只有"运行 guest"才是 TCG 软件模拟。改任何源码/配置后都要**重打 rootfs** 再重启 QEMU。

---

## 4. 验证结果（aarch64 QEMU 实测，全部通过）

- syslog-ng 接管 `/dev/log`（无 busybox syslogd 进程）。
- **C 接口字段齐全**：`2026-…T10:16:41.834+00:00 buildroot uf_logtest[117] info boot: …`
  （日期+毫秒 / 主机 / 进程名 / PID / 级别 / tag / msg）。
- 控制字符净化为单行；密码/令牌脱敏为 `***`；`INTERNAL-ONLY` 被丢弃；按 tag 提级别过滤生效；超长截断。
- **C++ 键值接口**：`connect: host=10.0.0.5; port=8080; load=0.75; up=true; ids=[1, 2, 3];
  counts={err=2, ok=10}; tags=[io, net]`；嵌套 `deep: kvs=[a=1, b=2]; rows=[{x=1}, {y=2}]`。
- 内核日志经 `/proc/kmsg` 汇入。
- **logrotate**：`logrotate -f` 两次后得到 `messages` / `messages.1` / `messages.2.gz`
  （轮转→delaycompress→gzip），`postrotate` 触发 `syslog-ng-ctl reopen`（非 copytruncate，无丢行）。

---

## 5. 实现中的关键决策与踩坑（给后来者）

1. **进程名/PID 必须在产生端采集**：采集端无法可靠还原是哪个进程写的哪行——这决定了"自研 liblog 在
   产生端格式化"而非"各自写文件再 tail"。
2. **busybox syslogd/klogd 必须停用**：rootfs 默认含 `S01syslogd`/`S02klogd`，busybox syslogd 会
   `unlink` 重建 `/dev/log` 抢占 socket，klogd 会独占 `/proc/kmsg`。用 overlay 把这两个 init 脚本
   置空（`exit 0`）即可，且可逆（删 overlay 文件即恢复）。
3. **syslog-ng 默认配置路径是 `/etc/syslog-ng.conf`（单文件，非目录）**：Buildroot 的包会装一份默认到
   这里；overlay 同路径覆盖即可。`@version:` 要与包版本（当前 **4.11.0**）匹配。
4. **过滤正则不能含字面空格**：解析器按空白分词，`[^ ]+` 里的空格会把正则截断 → 用 `[^[:space:]]+`。
5. **logrotate 不支持行内注释**：`size 1M  # 注释` 会报 `unknown unit`；注释必须独立成行。
   （syslog-ng、本库 filter.conf 的行首 `#` 注释都没问题，仅 logrotate 严格。）
6. **开发文件不进 target**：Buildroot 默认把头文件/`.a` 从 target 剥离，留在 `staging/`。测试程序
   **静态链接** `libuf_log.a` 故自包含可直接跑；其它应用在交叉编译期从 staging 链接。
7. **C/C++ 混合链接**：`uf_log.h` 用 `extern "C"` 包裹，C 归档可被 C++ 直接链接（已验证）。
8. **默认运行期级别是 DEBUG（全开）**：未配置时不静默丢任何级别；生产建议显式设 info
   （`uf_log_set_level(LOG_INFO)` 或 env `UF_LOG_LEVEL=6`），见 TODO。

---

## 6. 移植到真机建议

核心原则：**代码零环境假设，环境差异全部落在配置文件**。逐项 checklist：

| 项 | 现状（QEMU） | 真机要改/注意 |
|----|--------------|---------------|
| **上位机地址** | `syslog-ng.conf` 里硬编码 `network("10.0.2.2" … port(514))` | 改成上位机真实 IP/端口；或改用 `/etc/default/syslog-ng` 注入环境变量再在 conf 里展开，避免改主配置 |
| **日志分区** | `/var/log/uf` 在根 ext4（构建重生不持久） | 指向**独立持久分区**（flash/eMMC）；注意**写磨损**：调大 `size`、拉长 `rotate`，必要时用 f2fs/UBIFS，或降低落盘频率 |
| **时钟** | guest 时钟 | 真机须有 **RTC/NTP**：毫秒时间戳取自 `CLOCK_REALTIME`，时钟未同步前的时间不可信（可让 syslog-ng 另记接收时间旁证） |
| **init 系统** | busybox SysV（`/etc/init.d/SXX`） | 若用 **systemd**：`journald` 也监听 `/dev/log`，uf_log 无需改；但要决定是 journald 还是 syslog-ng 接管，并相应改启动单元（不要两者同抢 `/dev/log`） |
| **守护进程** | syslog-ng 4.11 | 可平替 **rsyslog**（uf_log 与上位机侧不变）；嵌入式若想更省，也可只保留本地文件去掉 UDP |
| **logrotate 触发** | 手动/需配 crond（见 TODO） | 真机务必把 logrotate 接到 cron/timer 周期触发，否则文件只长不轮转 |
| **权限/能力** | root 跑 syslog-ng | 真机若降权运行，注意 `/dev/log`、`/proc/kmsg`、日志目录的属主与 capabilities |
| **UDP 可靠性** | UDP 直发，丢包不重传 | 跨不可靠链路要实时可靠，考虑 syslog-ng 的 TCP/TLS destination（`network(... transport("tls"))`），或本地先落盘再异步上送 |
| **库的可移植性** | 纯 POSIX，`socket`/`clock_gettime`/`/proc/self/comm` | 任意 Linux 工具链可直接编译链接 `libuf_log`；非 Linux（无 `/proc`）需替换进程名获取方式 |

---

## 7. 待完成 TODO

### 设计已述、尚未实现（在 `uf_log` 内补齐）
- [ ] **限流（令牌桶，按 tag）+ "dropped N msgs" 汇总**：设计文档 §4.3 第 6 点，当前**未实现**。
      防某模块刷屏拖垮系统（TCG/嵌入式尤其重要）。
- [ ] **运行期级别热调**：当前可经 API `uf_log_set_level/_tag_level` 与启动期 env `UF_LOG_LEVEL`，
      但**无控制 socket/信号热调**（不重启进程动态改级别）。建议加一个 unix socket 或 `SIGUSR1/2`。
- [ ] **过滤规则自动热重载**：`uf_log_load_rules()` 只能手动调用；建议加 `SIGHUP` 触发重载
      `/etc/uf_log/filter.conf`。

### 集成/运维
- [ ] **logrotate 定时触发**：overlay 已装 `logrotate.conf` + `logrotate.d/uf`，但**未接 crond**
      （当前靠手动 `logrotate -f` 验证）。需加 busybox crontab 项（如每 5 分钟），并确认 `S50crond` 在跑。
- [ ] **生产默认级别决策**：库默认 DEBUG 全开；确定产品默认（建议 info），通过 overlay 的
      `/etc/default` 或编译期 `UF_LOG_MIN_LEVEL` 固化。
- [ ] **上位机接收侧**：提供/约定上位机 UDP 接收程序（标准 syslog 服务器即可，因发的是 RFC5424）。
      验证脚本目前只在 guest 内校验落盘，**未实测主机侧 UDP 收包**。

### P6：接入现有应用
- [ ] **EmbedMQ 切到 uf_log**：现用自带 `src/util/logger.h`（stderr，无日期/PID）。把其 logger 后端
      改为调用 `uf_log_write`（改动在独立仓库 `/home/zzy/emq`，不在本仓库）。
- [ ] 其它应用/用户态驱动逐步接入 `LOG_*` / `LOGKV_*`。

### 小优化（非阻塞）
- [ ] 内核日志行 `kernel[]` 的空 PID 字段（PROCID 为 nil），可在 syslog-ng 模板里对内核来源单独成型。
- [ ] `uf_log.c` 两处 `strncpy` 截断告警（无害，缓冲已置零保证终止），可改 `snprintf` 消除告警。
- [ ] 考虑把"配置 overlay"收敛成一个可开关的包 `uf-log-config`（设计文档 §6.2 备选），便于整体启停。
