# 嵌入式统一日志系统设计方案

面向整个产品的**通用、可靠、易移植**日志方案。设计目标按优先级：

1. **好用**：应用/用户态驱动一行接入，`LOG_I("tag", "fmt", ...)` 即可，开发者不关心底层。
2. **字段齐全**：每条日志含 **日期、毫秒级时间、进程名、进程号(PID)、等级、tag、msg**，格式参考 syslog。
3. **双路输出**：
   - **本地文件**：到一定 size 自动压缩，压缩包累积到一定数量后**循环删除最老**的。
   - **UDP 实时转发**：上位机经网络实时查看。
4. **可过滤**：支持自定义过滤，**防止打印非法/敏感内容**。
5. **易移植真机**：不依赖 QEMU/Buildroot 特性，纯 POSIX，配置全部外置。

> 状态：**P0–P5 + C++ 键值接口已落地并在 aarch64 QEMU 上端到端验证通过**
> （`uf_log` 包 + overlay + syslog-ng/logrotate 集成，验证脚本见 `scripts/qemu-logging.py`）。
> 仅余 P6：把现有 EmbedMQ 的 logger 后端切到 uf_log（改 `/home/zzy/emq`，不在本仓库）。

---

## 1. 选型结论与理由

**架构 = `liblog`（自研轻量日志库）+ `syslog-ng`（守护进程）+ `logrotate`（压缩/循环）三段式。**

| 角色 | 组件 | 职责 |
|------|------|------|
| 产生端（in-process） | **liblog** | 提供 `LOG_x()` API；采集进程名/PID/毫秒；**过滤/净化**；格式化为 RFC5424；经本地 socket 投递；失败回退 |
| 汇聚端（daemon） | **syslog-ng** | 收本地 socket(RFC5424) + 内核日志；落本地文件；UDP 转发上位机；二级过滤 |
| 轮转 | **logrotate** + busybox crond | 文件按 size 触发轮转、压缩、保留 N 份、循环删最老 |

**为什么不是 fluent-bit（虽然功能齐全）：**
- 你要的字段（毫秒 + 进程名 + PID + 独立 tag）用 **RFC5424 syslog** 原生表达最干净，任何标准 syslog
  上位机/查看器开箱即用 —— 对"移植真机、对接现成工具"最有利。
- "文件按 size 压缩 + 循环保留 N 份" 和 "UDP 实时转发" 是 syslog 守护进程 + logrotate 的标准能力；
  fluent-bit 的 `file` 输出**不原生轮转压缩**，需外挂 logrotate + copytruncate，有丢行风险。它的强项
  （复杂解析、多路由、上云）当前用不到。
- 结论：**轻量 + 可靠 + 真机原生 > 功能齐全**。fluent-bit 留作未来"需要复杂解析/上云"时的可选加工层
  （syslog-ng 再加一条 destination 转发给它即可，架构不动）。

> syslog-ng 与 rsyslog 二选一皆可（Buildroot 均自带）。本方案用 **syslog-ng**（配置更模块化）；如团队更熟
> rsyslog，可平替，liblog 与上位机侧完全不变。

---

## 2. 总体架构与数据流

```
        进程 A (sensord)          进程 B (netd)          内核态驱动
        ┌──────────────┐         ┌──────────────┐        printk
        │ LOG_I("i2c") │         │ LOG_E("tcp") │          │
        │   liblog     │         │   liblog     │          ▼
        │ 过滤+净化+格式化 │       │ 过滤+净化+格式化 │      /dev/kmsg
        └──────┬───────┘         └──────┬───────┘          │
               │ RFC5424 datagram        │                 │
               └───────────┬─────────────┘                 │
                           ▼                                │
                   /dev/log (unix-dgram)                    │
                           │                                │
                   ┌───────▼────────────────────────────────▼──────┐
                   │               syslog-ng                        │
                   │   source: unix-dgram(RFC5424) + /proc/kmsg     │
                   │   filter: 二级过滤(防御纵深)                    │
                   └───────┬───────────────────────┬────────────────┘
                           │                        │
              ┌────────────▼─────────┐   ┌──────────▼───────────────┐
              │ 本地文件             │   │ UDP 转发                  │
              │ /var/log/uf/...    │   │ network(udp 514, RFC5424) │
              │   ▲ logrotate:       │   │   → 上位机 (host 可配)     │
              │   size→压缩→保留N份  │   └──────────────────────────┘
              └──────────────────────┘
```

**为什么进程名/PID 必须在产生端打**：采集端（守护进程）读到的若是文件/裸文本，**无法可靠还原是哪个
进程写的哪行**。所以这些字段由 liblog 在进程内采集后随每条日志带出 —— 这也是选 "liblog 产生端格式化"
而非 "各自写文件再 tail" 的根本原因。

---

## 3. 日志格式（RFC5424）

每条日志在 liblog 内组装为标准 **RFC5424** 行：

```
<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
```

字段映射到你的需求：

| 需求字段 | RFC5424 位置 | 说明 |
|----------|--------------|------|
| 日期 + 毫秒时间 | `TIMESTAMP` | RFC3339，含毫秒：`2026-06-12T15:04:05.123+08:00`（`clock_gettime(CLOCK_REALTIME)`） |
| 进程名 | `APP-NAME` | 默认自动取 `/proc/self/comm`，可 `log_init()` 覆盖 |
| 进程号 PID | `PROCID` | `getpid()` |
| 等级 | `PRI` 低 3 位 | `PRI = facility*8 + severity`，标准 syslog 8 级 |
| tag | `MSGID` | 业务模块标签，如 `i2c`/`tcp`/`discovery` |
| msg | `MSG` | 净化、单行化后的正文 |

示例行：

```
<134>1 2026-06-12T15:04:05.123+08:00 myhost sensord 482 i2c - temp=25.000 C
 │      └ ver                         │      │       │   │  └ SD(无→"-")  └ msg
 └ PRI=local0(16)*8+info(6)=134        host   app     pid msgid(tag)
```

**等级约定**（采用标准 syslog severity，便于真机/标准工具对齐）：

| 宏 | severity | 数值 |
|----|----------|------|
| `LOG_E` | error | 3 |
| `LOG_W` | warning | 4 |
| `LOG_I` | info | 6 |
| `LOG_D` | debug | 7 |

（如需 notice/crit/alert/emerg 等其余 severity，liblog 暴露通用 `log_write(severity, tag, ...)`。）
**facility** 默认 `local0`，可在 `log_init()` 指定，用于把"应用日志"与"内核/系统日志"在上位机分流。

---

## 4. liblog 设计（核心，开发者唯一接触的东西）

作为 br2-external 包 `uf_log`（参照仓库现有 `mylib` 模板：库 + 头文件装 staging，供其它包/应用链接）。
纯 C 实现 + header-only 的 C++ 键值前端（见 §4.6），静态库 `.a` + 共享库 `.so` + `uf_log.h`/`uf_log.hpp` 皆产出。

### 4.1 API（开发者视角）

```c
#include <uf_log.h>

int main() {
    log_init("sensord", LOG_FAC_LOCAL0);   /* 可省略：自动取进程名、默认 facility */

    LOG_I("i2c", "temp=%d.%03d C", t/1000, t%1000);
    LOG_W("i2c", "bus busy, retry %d", n);
    LOG_E("net", "connect failed: %s", strerror(errno));

    log_set_level(LOG_INFO);                /* 运行期全局级别 */
    log_set_tag_level("i2c", LOG_WARNING);  /* 按 tag 单独提级别 */
    log_set_filter(my_filter_cb);           /* 注册自定义过滤回调 */
}
```

宏 `LOG_x(tag, fmt, ...)` 是开发者唯一需要记的东西。**编译期** 可用 `-DUF_LOG_MIN_LEVEL=LOG_INFO`
把低于阈值的调用整段编译掉（零运行时开销，对嵌入式重要）。

### 4.2 字段采集
- 进程名：启动时读 `/proc/self/comm`（或 `log_init` 显式传入），缓存。
- PID：`getpid()` 缓存；`fork` 后由调用方 `log_reinit()` 或库内 `pthread_atfork` 刷新。
- 时间：`clock_gettime(CLOCK_REALTIME)` → 秒 + 毫秒 → RFC3339。
- hostname：`gethostname()` 缓存。

### 4.3 过滤管线（实现"自定义过滤 + 防非法内容"——重点）

每条日志在投递前依次过这几道，任一道判定丢弃即不外发：

1. **编译期级别裁剪**：`UF_LOG_MIN_LEVEL` 以下的 `LOG_x` 宏展开为空。
2. **运行期级别门限**：全局级别 + 按 tag 级别（`log_set_level`/`log_set_tag_level`），也可从配置文件加载、
   运行期经控制 socket 热调（无需重启进程）。
3. **非法字符净化（强制，"防非法内容"的核心）**：
   - 把 `CR`/`LF`/控制字符(0x00–0x1F) 转义为可见形式 → **防止多行注入破坏 syslog 分帧 / 日志伪造**。
   - 非可打印字节按策略转义；强制单行。
   - 长度上限截断（带 `…[truncated]` 标记），防超长撑爆缓冲。
4. **规则过滤（自定义，drop / 脱敏 redact）**：从 `/etc/uf_log/filter.conf` 加载规则，可热重载：
   ```
   # action   scope   ERE-pattern                              [replacement(缺省 ***)]
   redact     msg     (password|passwd|token|secret)=[^[:space:]]+   ***
   drop       tag     spammy
   drop       msg     INTERNAL-ONLY
   ```
   - `redact` 命中即把子串替换为 replacement（脱敏，如密码/令牌打码），`drop` 命中即整条丢弃。
   - scope ∈ `tag` / `msg` / `any`。用内置 POSIX `regcomp`（ERE，大小写敏感，避免重依赖）。
   - 解析器按空白分词，故**模式不能含字面空格**：要匹配非空白用 `[^[:space:]]` 而非 `[^ ]`。
5. **自定义回调**：`log_set_filter(cb)`，`cb(log_record_t*)` 返回 `LOG_KEEP/LOG_DROP`，且可**就地改写**
   `rec->msg` 实现任意业务级脱敏/重写 —— 应用特有的"非法内容"判定走这里。
6. **限流（可靠性）**：按 tag 令牌桶，防某模块刷屏拖垮系统（尤其 QEMU/TCG 下宝贵）；被限流的条数周期性
   汇总成一条 `dropped N msgs` 输出，不静默丢。

> **防御纵深**：上述在产生端拦截"非法内容"最干净（根本不出进程）。syslog-ng 端再加一道过滤（见 §6），
> 即便某个进程绕过/误用 liblog，也能在汇聚端兜底丢弃。

### 4.4 传输与可靠性
- 默认写 **`/dev/log`**（标准 syslog unix-dgram socket），datagram 边界天然对齐一条日志。socket 路径可配
  （`UF_LOG_SOCK`）。
- **非阻塞发送**：socket 满时不阻塞业务线程；可选丢弃或短重试，丢弃计数汇总上报。
- **失败回退**：守护进程未起/socket 不可达时，回退写本地应急文件 `/var/log/uf/fallback.log`，**避免整丢**。
- **启动早期**：守护进程起来前的日志走回退文件，daemon 起来后由 syslog-ng 的 file source 补采（可选）。

### 4.5 可移植性（移植真机要点）
- 纯 POSIX：`socket`/`clock_gettime`/`/proc/self/comm`，任意 Linux 工具链可编译，不含 QEMU/Buildroot 假设。
- 所有可变项（socket 路径、默认级别、facility、过滤规则文件路径）经**环境变量或配置文件**注入，代码零硬编码。
- daemon 无关：liblog 只写 `/dev/log`；真机若用 systemd，`journald` 同样监听 `/dev/log`，liblog 无需改动
  （把 UDP/文件那套换成 journald + 转发即可，或继续用 syslog-ng）。

### 4.6 C++ 键值接口（`uf_log.hpp`）

在 C 库之上提供 header-only 的 C++ 接口，用于打印 **key-value 数组**：key 是字符串，value 可为
**任意 C++ 类型**（算术/bool/string、`std::pair`、任意 map、任意可迭代容器，以及它们的任意嵌套，
或任何提供 `operator<<` 的类型）。采用流式链式构建，临时对象析构时一次性发往底层 C 库：

```cpp
#include <uf_log.hpp>

LOGKV_I("net", "connect")
    .kv("host", host)        // std::string
    .kv("port", 8080)        // int
    .kv("load", 0.75)        // double
    .kv("ids", ids)          // std::vector<int>
    .kv("counts", counts);   // std::map<std::string,int>
```
产生的 msg：
```
connect: host=10.0.0.5; port=8080; load=0.75; ids=[1, 2, 3]; counts={err=2, ok=10}
```

渲染规则（`if constexpr` 递归）：标量/字符串原样；容器 → `[a, b, c]`；map → `{k=v, ...}`；
`pair` → `a=b`；嵌套自动展开（如 `vector<map<...>>` → `[{x=1}, {y=2}]`）。
`LOGKV_E/W/I/D(tag, head)` 与 C 宏一样受 `UF_LOG_MIN_LEVEL` 编译期裁剪（被裁级别退化为零开销空对象）。
底层级别门限/过滤/净化/双路输出与 C 接口**完全共用**——C++ 接口只是格式化前端。

> 注意：链式 `.kv(...)` 的实参在被裁剪级别下仍会求值（C++ 链式调用的固有特性），与 C 宏不同；
> 但运行期级别门限仍会拦截投递。避免在被频繁裁剪的 `LOGKV_D` 上放昂贵的实参计算。

---

## 5. 内核/驱动日志

- **内核态驱动**（真正的内核模块）只能 `printk → /dev/kmsg`，无法链接 liblog。由 syslog-ng 的
  `file("/proc/kmsg")` source 采入，统一进同一文件与 UDP 流。其字段较少：无进程名/PID，facility=`kern`，
  以子系统名作 tag。
- **用户态"驱动"/HAL 进程**：当作普通应用，正常用 liblog，字段齐全。

---

## 6. syslog-ng 配置

`/etc/syslog-ng/syslog-ng.conf`（经 overlay 交付，见 §8）：

```
@version: 4.8
options {
    keep-hostname(yes);
    ts-format(rfc3339);
    frac-digits(3);          # 毫秒
    create-dirs(yes);
};

# ── 来源 ───────────────────────────────────────────────
source s_app {
    unix-dgram("/dev/log" flags(syslog-protocol));   # liblog 的 RFC5424
    internal();
};
source s_kernel {
    file("/proc/kmsg" program-override("kernel"));
};

# ── 二级过滤：防御纵深，丢弃含非法标记的内容 ─────────────
filter f_clean { not match("INTERNAL-ONLY" value("MESSAGE")); };

# ── 本地文件（交给 logrotate 做压缩/循环，见 §7）────────
destination d_file {
    file("/var/log/uf/messages"
         template("${ISODATE} ${HOST} ${PROGRAM}[${PID}] ${LEVEL} ${MSGID}: ${MSG}\n"));
};

# ── UDP 实时转发上位机（RFC5424）──────────────────────
destination d_net {
    network("`LOG_UDP_HOST`"          # 启动时从环境变量展开，真机/QEMU 仅改这个
            transport("udp")
            port(`LOG_UDP_PORT`)
            flags(syslog-protocol));
};

# ── 汇聚 ───────────────────────────────────────────────
log { source(s_app); source(s_kernel); filter(f_clean);
      destination(d_file); destination(d_net); };
```

- UDP 默认发 **RFC5424**（标准、上位机用任意 syslog 查看器即可解析）。若上位机是自研程序、更想要纯文本或
  JSON，只需改 `d_net` 的 `template(...)` / `flags`，**liblog 与文件侧不动**。
- `LOG_UDP_HOST` / `LOG_UDP_PORT` 由 `/etc/default/syslog-ng`（或 init 脚本）导出环境变量。QEMU user-net
  下主机即 `10.0.2.2`；真机填上位机 IP。**这是唯一与环境相关的配置点。**

---

## 7. 文件轮转：size → 压缩 → 循环保留 N 份

用 **logrotate**（Buildroot 自带）+ busybox `crond` 定时触发。
`/etc/logrotate.d/uf`（overlay 交付）：

```
/var/log/uf/messages {
    size 1M             # 超过 1M 触发轮转（按 size）
    rotate 10           # 保留 10 个历史 → 第 11 个生成时删最老（循环删除）
    compress            # gzip 压缩历史文件
    delaycompress       # 最近一个先不压缩，便于查看
    missingok
    notifempty
    copytruncate? 否    # 见下：用 postrotate 让 syslog-ng 重开文件，避免丢行
    postrotate
        /usr/sbin/syslog-ng-ctl reopen 2>/dev/null || true
    endscript
}
```

定时触发（busybox crontab，例如每分钟检查一次 size）：
```
* * * * * /usr/sbin/logrotate /etc/logrotate.conf
```

> 取舍：logrotate 是"周期检查 size"而非"实时按字节"切分，故实际切分粒度 = cron 周期。要更紧的 size 控制
> 可缩短 cron 周期，或改用 syslog-ng 自身按时间/大小的多文件策略。本方案优先**标准、可靠、真机一致**，
> 用 logrotate + `syslog-ng-ctl reopen`（**不用 copytruncate**，避免拷贝-截断之间的丢行）。

---

## 8. Buildroot 集成

严格遵守 CLAUDE.md：**增量改 `.config` + `olddefconfig`，绝不 `defconfig`**；全程经 `./build.sh`。

### 8.1 启用守护进程与轮转工具
```bash
{
  echo 'BR2_PACKAGE_SYSLOG_NG=y'
  echo 'BR2_PACKAGE_LOGROTATE=y'
  # busybox 默认含 crond/syslogd applet；确认 crond 已启用
} >> buildroot/.config
./build.sh olddefconfig     # 解析 select 链，不触碰精简 defconfig
./build.sh
```
（启用后**不要** `savedefconfig`——会写回精简 defconfig 丢包，与 embedmq/zmqbench 同理留在 `.config` 中。）

### 8.2 liblog 作为 br2-external 包 `uf_log`
照 CLAUDE.md 第 "新增包" 流程（参照 `mylib`）：
1. `br2-external/package/uf_log/Config.in` —— `config BR2_PACKAGE_UF_LOG`，`depends on BR2_TOOLCHAIN_HAS_THREADS`。
2. `br2-external/package/uf_log/uf_log.mk` —— `UF_LOG_SITE_METHOD = local`，源码放
   `br2-external/src/uf_log/`；`UF_LOG_INSTALL_STAGING = YES`（装 `.a`+头文件供应用链接）；
   `$(eval $(cmake-package))` 或 `generic-package`。
3. `br2-external/Config.in` 加 `source ".../package/uf_log/Config.in"`。
4. 应用包（用到日志的）加 `<APP>_DEPENDENCIES += uf_log` 并 `-luf_log`。

```bash
echo 'BR2_PACKAGE_UF_LOG=y' >> buildroot/.config
./build.sh olddefconfig && ./build.sh rebuild uf_log
```

### 8.3 配置文件经 rootfs overlay 交付（纯文件、免编译、进仓库、可持久）
```
br2-external/board/overlay/
  ├─ etc/syslog-ng/syslog-ng.conf
  ├─ etc/default/syslog-ng            # 导出 LOG_UDP_HOST / LOG_UDP_PORT
  ├─ etc/logrotate.d/uf
  ├─ etc/uf_log/filter.conf         # liblog 过滤规则（热重载）
  └─ etc/cron/crontabs/root           # logrotate 定时项（busybox crontab）
```
```bash
echo 'BR2_ROOTFS_OVERLAY="$(BR2_EXTERNAL_SIMU_PATH)/board/overlay"' >> buildroot/.config
./build.sh olddefconfig && ./build.sh
```

> 改动任何配置/库/包后必须**重跑 `./build.sh` 重打 rootfs 再重启 QEMU**（CLAUDE.md 规则 3）。

---

## 9. 上位机 / 网络侧

- guest 经 UDP 把 RFC5424 日志发到 `LOG_UDP_HOST:LOG_UDP_PORT`。
- QEMU 下两种联通方式：
  - user-net：guest 直接发 `10.0.2.2:514`（主机），或
  - `./run-qemu.sh -p 主机端口:guest端口` 做端口转发（如需反向/特定拓扑）。
- 上位机选型：
  - **现成 syslog 服务器**（rsyslog/syslog-ng/`journalctl`/图形 syslog 查看器）：因发的是标准 RFC5424，
    **零适配**直接看。
  - **自研接收程序**：UDP 收包后按 RFC5424 解析；若嫌解析麻烦，把 §6 的 `d_net` template 改成纯文本/JSON
    即可，上位机更易处理。

---

## 10. 移植到真机 checklist

| 项 | QEMU | 真机 | 是否需改代码 |
|----|------|------|--------------|
| liblog 源码 | 同一份 | 同一份 | 否（纯 POSIX） |
| 上位机地址 | `LOG_UDP_HOST=10.0.2.2` | 填真实 IP | 否，改 `/etc/default/syslog-ng` |
| 日志目录/分区 | `/var/log/uf`（tmpfs/ext4） | 指向**持久分区**(flash)，注意写磨损→调大 size/拉长轮转 | 否，改配置 |
| init 系统 | busybox SysV | SysV 或 systemd | 否；systemd 下 journald 亦监听 `/dev/log`，或继续用 syslog-ng |
| 时钟 | guest 时钟 | 真机 RTC/NTP，确保 CLOCK_REALTIME 准（毫秒时间正确性依赖它） | 否 |
| 守护进程 | syslog-ng | syslog-ng / rsyslog | 否，平替 |

核心思想：**代码零环境假设，环境差异全部落在配置文件**。

---

## 11. 分阶段落地

| 阶段 | 目标 | 验收 |
|------|------|------|
| **P0** | 启用 syslog-ng + logrotate + crond，跑通空管线 | guest 内守护进程在跑，`/proc/kmsg` 入文件 |
| **P1** | liblog 最小版（格式化 + 写 /dev/log + LOG_x 宏） | demo 程序 `LOG_I` 的行出现在 `/var/log/uf/messages`，字段齐全(含 ms/PID/进程名/tag) |
| **P2** | 双路输出 | 同一条日志既落文件、又被上位机 UDP 收到（RFC5424 可解析） |
| **P3** | 文件轮转 | 灌日志至 >1M，确认轮转、压缩、超 10 份删最老 |
| **P4** | 过滤管线 | 级别门限生效；含 `password=` 被脱敏；含 `INTERNAL-ONLY` 被丢；非法控制字符被净化；自定义回调生效 |
| **P5** | 可靠性 | 杀掉守护进程时 liblog 回退本地文件不丢；限流刷屏不拖垮；重启不乱 |
| **P6** | 接入现有应用 | 把 EmbedMQ 等改用 liblog（或先用 stderr→file 过渡），全产品统一 |

> 现有 EmbedMQ 用的是自带 `src/util/logger.h`（stderr，无日期/PID）。P6 时把其 logger 后端切到 liblog
> （改 `/home/zzy/emq` 那个独立仓库，不在本仓库），即可纳入统一体系。

---

## 12. 验证 harness

新增 `scripts/qemu-logging.py`，复用现有 `scripts/qemu-*.py` 的 `pty.fork()` + `select` 串口驱动范式：

1. 启动 guest，确认 syslog-ng/crond 在跑。
2. 跑一个 demo（或带 liblog 的节点），打各级别、带特殊内容（密码串、控制字符、超长行、刷屏）。
3. 断言 `/var/log/uf/messages` 行格式正确、字段齐全；脱敏/丢弃/净化/限流符合预期。
4. 在**主机侧**起一个 UDP 收包小程序（脚本内 `socket`），断言收到对应 RFC5424 行。
5. 灌量触发轮转，断言压缩文件数受 `rotate 10` 限制、最老被删。
6. 采 syslog-ng 自身 `VmRSS`/CPU（按现有口径，TCG 下只作相对比较，RSS 最具代表性）。
7. `poweroff`，报告写**仓库根**（与其它 harness 一致）。

---

## 13. 风险与权衡

- **logrotate 是周期检查、非实时按字节切**：切分粒度 = cron 周期；要更紧可缩短周期。已用
  `reopen`（非 copytruncate）避免丢行。
- **守护进程单点**：crash 则集中链路中断；已用 liblog **回退本地文件** + init 自动重启 兜底。
- **/dev/log datagram 可能丢**：高压下内核 socket buffer 满会丢；已用 **限流 + 丢弃计数汇总** 让丢弃可见、
  不静默。需要"绝不丢"的关键日志可在 liblog 对特定级别走阻塞/落盘优先策略（可选项）。
- **毫秒时间正确性依赖系统时钟**：真机须有 RTC/NTP；时钟未同步前的时间戳不可信（可由守护进程在收时补
  接收时间作旁证）。
- **TCG 性能**：日志链路与被测程序抢 CPU；编译期级别裁剪 + 限流 + 合理 flush 降低占用；做性能基线时可临时
  停守护进程对照。

---

## 14. 一句话落地清单

```bash
# 1) 守护进程 + 轮转（增量，绝不 defconfig）
{ echo 'BR2_PACKAGE_SYSLOG_NG=y'; echo 'BR2_PACKAGE_LOGROTATE=y'; } >> buildroot/.config
# 2) liblog 包 + 配置 overlay
echo 'BR2_PACKAGE_UF_LOG=y' >> buildroot/.config
echo 'BR2_ROOTFS_OVERLAY="$(BR2_EXTERNAL_SIMU_PATH)/board/overlay"' >> buildroot/.config
./build.sh olddefconfig
./build.sh
./run-qemu.sh
# 3) 应用侧：#include <uf_log.h> 后 LOG_I("tag", "fmt", ...) 即可
```
```

应用开发者只需记住一件事：
    LOG_I("tag", "...");   // 其余（字段、过滤、双路输出、轮转）全由日志系统负责
```
