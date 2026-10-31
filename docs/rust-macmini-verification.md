# Mac mini 实机验证

## 可重复基线驱动

R01 的基线入口在本机或 Mac mini 上使用固定 80×24 PTY、`seed=12345` 和本地
IPv4 UDP relay 记录同一组输入与输出指标：健康路径、128 kbit/s 的相似大小连续帧、
慢资源、上下行分别丢包，以及 relay 中模拟的 1 字节 MTU 黑洞后的恢复。命令如下：

```sh
cargo build -p mosh-rs --bins --examples --locked
python3 scripts/verify-rust-cli.py --baseline --bin-dir target/debug \
  --evidence-dir /tmp/mosh-rs-baseline
```

`baseline.json` 保存 commit、系统、窗口、seed、每个场景的输入字节、PTY 字节、
PTY marker screen age、资源 fixture duration、恢复时间、relay 包/字节计数、
队列高水位和端点诊断阶段统计。
`terminal.tty` 与 relay/diagnostics JSON 是原始证据。CPU 和 RSS 取子进程在当前
系统上的 `getrusage` 读数，Mac 与 Linux 的 RSS 单位可能不同，比较时必须保留
平台和 commit。

首轮记录对应提交 `2ce8e3d`。文末的协议 v2 复测记录后续修复及当前状态。

2026-10-26 使用现有 `ssh macmini` 配置执行。客户端和服务端都是 Apple Silicon，远端为 macOS 27.0.1。本机终端为 cmux 0.65.2，底层为 Ghostty。程序放在独立临时目录。

本轮修复后的 debug 和 release 双端都通过真实 SSH 的默认主机名、两个局域网接口和完整现代 CLI 验证，并分别通过跨机弱网、端口变化和断网恢复。Shell、Vim、tmux、生命周期和 GUI 视觉验收使用 debug 构建。当前修复源码构建的四个 example 也已在 Mac mini 全部通过。

## 首轮实测结果

| 路径 | 结果 |
| --- | --- |
| 真实 SSH 引导和直接 UDP | 默认主机名、两个局域网接口均通过完整 CLI 验证。 |
| 现代输入 | UTF-8、组合字符、Kitty 按下/重复/释放、关联文本、鼠标、焦点、括号粘贴和控制键通过。 |
| 窗口与退出 | 远端 PTY 尺寸、退出码 7/9/23/129、备用屏幕和本地 termios 恢复通过。 |
| 交互 shell | zsh 中的中文、表情、缩放和输出洪流中的 Ctrl-C 通过。 |
| 真实 cmux 图像 | 默认 auto 模式显示真彩色、中文、组合字符、表情、下划线、OSC 8 和静态 RGBA 图片。 |
| 真实 Vim | Ghostty 实际按键、中文和表情粘贴、Escape、保存退出通过。远端文件与预期逐字节一致。 |
| 真实 tmux | 独立 socket 中分屏、缩放到 90×17、中文粘贴和分离通过。 |
| 弱网 | 自建 UDP 中继注入每向 65 ms 延迟、55 ms 抖动、12% 随机丢包、10% 重包和乱序，8 KiB 括号粘贴完整到达。 |
| 地址变化 | 中继后端源端口改变后，会话继续使用原命令进程。 |
| 断网恢复 | 双向丢弃数据包 24 秒后自动重新附着。worker PID 和命令 PID 均不变，排队输入只执行一次。 |
| 会话生命周期 | Kitty escape 分离、终止本地客户端、显式关闭远端和断开后自动过期均通过。 |
| 核心和安全 | Unicode、静态图像、双向证书拒绝、输入去重、旧 epoch 拒绝、资源与 effects、输出阻塞和最终输出屏障通过。 |

中继一次成功运行记录 188 个收到的数据报、40 次丢弃、4 次复制和一次端口变化。该记录包含完整断网阶段，不能把总丢包比例当作随机丢包配置值。输入完成记录中的 worker PID 为 28512，命令 PID 为 28513，两者与断网前一致。

## 首轮发现并修复的问题

- 客户端原先总是使用 IPv6 双栈 socket。在本机网络环境中，IPv4 映射地址的数据包无法跨局域网送达。现在分别使用原生 IPv4、IPv6 socket，并归一化 IPv4 映射地址。
- 地址逐一拨号会等待每个不可用候选超时。原 localhost 探针耗时 15.015 秒，修复后为 6.922 毫秒。现在并发执行正常 mTLS 握手，只有获胜连接发送 Attach。
- 首个远端画面之前，连接错误只存在内部状态中。现在立即显示连接状态和错误原因。
- Ghostty 实际发送 `ESC[105;;105u`。合法的空修饰字段原先会令客户端退出，现在按默认值 1 处理，保留非法值检查。
- Ghostty 和 cmux 的自动图像选择原先为 none。现在自动选择已实测可用的 Kitty 输出。

原有 CLI 验证驱动也修复了 PTY 短写和双向背压。采样证实，单次阻塞写入大量字节时，驱动和客户端可能同时阻塞在 write。驱动现在分块非阻塞写入，并持续读取终端输出。

## 首轮验证边界

OSC 52 写入已在 macOS 剪贴板中核对，原剪贴板记录随后恢复。真实 GUI 读取出现 cmux 的授权弹窗，未完成用户批准后的原生读取往返。协议侧的允许、拒绝和超时路径通过 PTY 验证。

在上述弱网下，一次性注入 8192 个独立按键没有通过驱动的 30 秒写入期限。成功的 8 KiB 验证使用现代终端实际提供的括号粘贴。该失败已由下述协议 v2 修复并复验；这仍不代表任意输入洪流的吞吐率。

源端口变化和断网作用于本轮中继。没有关闭真实网卡或让整台设备休眠，因此未声称验证了硬件切换、系统休眠或公网 NAT。实际 GUI 图像验证使用 Kitty。SIXEL 和 iTerm 输入在核心验证中通过，未在对应真实终端中进行视觉验收。其余静态图像、预测和会话恢复限制见 [Rust 使用说明](rust-usage.md)。

## 首轮构建与清理记录

本轮执行 `cargo build -p mosh-rs --release --bins --locked`。本地与远端 release 文件的 SHA-256 一致。

```text
c3169384558953443ebe486ce1995af8889eae5ad51810117fb339119ab26274  mosh-rs
4a7720cfc97fe67f7b7a098c43b78ad23111e0bb8fface8cb3bc4c8015cb62cf  mosh-rs-server
```

Mac mini 上的 `verify_core`、`verify_transport`、`verify_display`、`verify_session` 全部通过。最后一项验证安装了 24 个画面，覆盖部分 PTY 写入后的重连去重、旧连接 fencing、资源与查询、输出洪流、超过 3 MB 画面的取消和退出屏障。传输验证包含原生 IPv4、IPv6 和 IPv4 映射地址回归。

测试前仅为临时服务端新增一条 macOS 应用防火墙规则。测试后已移除此规则，规则数量从 119 恢复到 118，其余应用路径逐项一致。确认没有本轮远端进程残留后，已删除私有测试目录。独立 tmux server 和自建 cmux 窗口均已关闭，原有窗口保留。

已安装的 `/opt/homebrew/bin/mosh-server` 仍为 Mosh 1.4.0，未覆盖。本轮没有合并 PR 或替换正式使用的命令。基础验证命令见 [Rust 使用说明](rust-usage.md#run-the-verification-tools)。

## 后续协议 v2 修复与复测

旧实现每个原始按键使用一条 JSON control 消息，8202 个事件的编码体积为 1262033 字节，包含每条四字节长度头。运行计数表明输入在持续前进，受丢包拥塞和事件体积影响，最终超过原有 30 秒写入期限。v2 使用现有 postcard 编码，同一组完整语义事件为 139320 字节。此数值不包含 QUIC、UDP、重传或显示流量。输入序号、按键阶段、关联文本、JSON 内存记账、去重摘要和 PTY 提交算法均保留。

协议版本与 ALPN 一起升为 `2`、`mosh-rs/2`，SSH 引导继续使用换行 JSON。两端需要一起更新，不能用 v2 客户端恢复 v1 worker。真实 QUIC 验证覆盖全部 control 变体、Unicode 与完整 Key 字段、严格尾部消费、截断、长度上限、解码期集合上限和 v1 拒绝。

PNG 的原问题来自未用于 RGBA 输出的 ICC 元数据解压。现在在尺寸探测、查询与实际解码的公共入口保留全部 critical 块和 tRNS，丢弃其余 ancillary 元数据，并在过滤前拒绝 APNG。原像素转换保持。相同的 40 MiB ICC 样本在查询、Kitty 上传和 iTerm 三条真实入口中，最大单次 Rust 内存申请均从 34603008 字节降为 65536 字节。重新编译原独立 allocator 探针得到相同最大值，新增请求峰值为 120452 字节。它们都不是进程 RSS 硬上限。

回归同时核对合法 ICC、压缩文字和 EXIF 与无元数据 PNG 的资源 digest 一致，以及调色板/灰度透明度、16 位舍入、后置元数据、非法块名、未知 critical、截断和 APNG。丢弃元数据时不验证其 CRC，保留块遵循原解码器的容错规则；此路径不是完整原 PNG 验证器，也不实现 ICC 色彩管理。

| v2 修复构建 | 原始按键写入本地 TTY | 远端逐字节收全 | SSH 与恢复结果 |
| --- | ---: | ---: | --- |
| debug | 7.741 秒 | 12.215 秒 | 默认主机名、.99/.101 完整现代 CLI、源端口变化和 24 秒断网后恢复均通过。 |
| release | 8.214 秒 | 12.921 秒 | 同上；排队输入只执行一次，退出码和本地终端恢复通过。 |

两行均使用每向 65 ms 延迟、55 ms 抖动、12% 随机丢包、10% 重包和乱序，发送的是 8192 个独立原始按键及前缀/Enter，未改为 bracketed paste，也未延长 30 秒写入期限。release 恢复前后的 worker PID 均为 59355、命令 PID 均为 59356，五个完成阶段及 SHA-256 严格匹配。

该轮源码对应的四个 example 已在 Mac mini 全部重跑通过。`verify_core` 包含新 PNG 内存回归；`verify_transport` 包含 v2 编码边界；`verify_session` 安装了 23 个画面，覆盖部分写入去重、旧 epoch、资源、effects、查询、输出洪流、超过 3 MB 画面的取消和最终退出屏障。本轮未重复首轮 GUI 视觉与 Vim/tmux 手动验收，真实 SSH 的现代输入/剪贴板协议往返已重新验证。

该轮本地与远端 release SHA-256 一致。

```text
ea486ce986cb3a5d17759cee2f3122031e47787cfcbd998b7563a2d0f11ad9d9  mosh-rs
97551da83c4bb0af6d6d24e526f5401871dfaad96c0122babade9a170b93cfa2  mosh-rs-server
```

本轮临时进程均已退出，私有远端目录已删除。防火墙的 120 条开测前规则与清理后的路径和状态逐项相同；没有新增的私有路径条目需要删除。已安装 Mosh 的 SHA-256 未改变。

仍未验收的环境组合包括原生剪贴板用户同意后的读取、真实网卡切换/休眠、公网 NAT 和真实 SIXEL/iTerm 视觉输出。静态图像范围、启发式预测、进程内恢复凭据以及分开的内存预算限制仍适用。

## 终端修复与本地挂起恢复

新增原版 Mosh 的本地挂起操作。按 Ctrl-^、Ctrl-Z 返回本地 shell，`fg` 恢复原远端会话。普通 Ctrl-Z 继续发给远端。恢复会重画缓存中的文字、图像、标题、目录、反色和光标，不重放已执行的剪贴板动作、铃声或 OSC 133 标记。`bg` 后客户端等到重新成为前台才读写终端，可运行的后台客户端仍可用 TERM 或 HUP 退出。

| 行为 | 本轮实际验证 |
| --- | --- |
| 挂起与恢复 | 本地交互式 zsh、真实 SSH Mac mini 的 debug 和 release 两端均通过 legacy/Kitty 快捷键、多轮 suspend、bg、重复 CONT 和 fg。 |
| 长挂起与输入 | 挂起 22 秒后使用原有重连路径，保持相同 worker、命令 PID；挂起前尚未发出的各组输入按原序只执行一次。 |
| 现代显示恢复 | 无需新画面即可恢复缓存文字、Kitty 图片和元数据，effects/OSC 133 次数保持一次；已有 renderer 验证另覆盖文字、Kitty、SIXEL 的缩小与完整缓存重画。 |
| 本地终端 | 挂起后与交互 shell 原有 termios 一致；恢复与最终退出的 title/keyboard push、pop 配对。外部 STOP、bg、fg 也通过，但 SIGSTOP 之前无法预清理。 |
| 图片与窗口 | Kitty、显式原生大小 iTerm、SIXEL 图片越过窗口右边界后，20→16→20 列恢复原图像单元，21 列显示原隐藏尾部；两种 renderer 安装全部画面成功。2→1→2 列宽字符恢复通过。 |
| 查询与控制 | 80×24 全屏 checksum、屏外/反向矩形、模 65536 真零、wide-cell 列索引和最大 u32 控制参数通过。真实 SSH 的宽图、checksum、CUB/DSR 和退出恢复在 debug/release 两端通过。 |
| 网络与回归 | 本地与 Mac mini 的四个 example、现代 CLI、8192 独立按键弱网、22 秒双向断网、输入去重和原进程恢复均通过。 |

图像修复仅在导出画面时投影当前窗口。核心隐藏单元及其原图像采样坐标保留，避免缩小窗口后无法恢复。checksum 使用物理列和模 65536 求和。光标/Tab 在固定点停止，reverse-wrap 跳过完整周期；REP 单动作上限见核心维护说明。

本轮 release 文件在本地和 Mac mini 的 SHA-256 相同。

```text
27b875162350e3030136d1089b3812f8357c43e07bc6bcf8aab707f3009a1c85  mosh-rs
21983010bdbb66c610045621e83fc784b05e90c4c670b8e4a30c7d41536befda  mosh-rs-server
```

release 新测试路径首次连接受应用防火墙阻挡，放行本轮临时服务端后通过。结束后移除了这条规则，开测前的 120 条应用路径和状态逐项一致。已确认零临时远端进程，删除私有目录，已安装 Mosh 的 hash 不变。首轮记录中的原生剪贴板同意后读取、硬件休眠/网卡切换、公网 NAT 和真实 SIXEL/iTerm 视觉验收仍未补测。本轮不将 stable 构建当作 Rust 1.88 MSRV 验证。
