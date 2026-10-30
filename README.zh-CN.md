# mosh-rs

[English](README.md) | [简体中文](README.zh-CN.md)

[![Rust CI](https://github.com/harryisfish/mosh/actions/workflows/rust.yml/badge.svg)](https://github.com/harryisfish/mosh/actions/workflows/rust.yml)

用 Rust 编写的远程终端。网络中断、客户端休眠或切换网络后，可以恢复到同一个远程命令。
本地输入预测降低打字时的延迟感。SSH 负责认证和启动服务端，随后通过 QUIC 同步终端状态。

本项目是 [Mosh](https://github.com/mobile-shell/mosh) 的 Rust 重写版本。
客户端和服务端分别为 `mosh-rs` 与 `mosh-rs-server`。

Rust 版使用协议 v2，与原版 Mosh 协议和 Rust 协议 v1 均不兼容。
客户端和服务端请安装同一版本。

## 安装

安装最新版本到 `~/.local/bin`：

```sh
curl -fsSL https://raw.githubusercontent.com/harryisfish/mosh/master/install.sh | bash
```

脚本会识别操作系统、CPU 和 Linux libc，校验 release 的 checksum 后再安装两个二进制文件。
使用 `--version 0.1.3` 指定版本，或用 `--install-dir DIRECTORY` 指定安装目录。
需要先审阅脚本时，参考[使用指南](docs/rust-usage.md#install-a-release)。

在本机安装 `mosh-rs`，在远程主机安装 `mosh-rs-server`，两端必须使用同一版本。
将服务端放入远程 SSH 环境的 `PATH`，或用 `--server` 指定它的绝对路径。

### Homebrew

macOS 和 Linux 均可直接使用本仓库作为 tap：

```sh
brew tap harryisfish/mosh https://github.com/harryisfish/mosh
brew install harryisfish/mosh/mosh-rs
```

Formula 固定已发布版本及其 SHA-256，同时安装客户端和服务端。

### Debian、Ubuntu、Fedora 和 openSUSE

从 [GitHub Releases](https://github.com/harryisfish/mosh/releases)
下载对应安装包和 `SHA256SUMS`，先执行校验：

```sh
sha256sum --ignore-missing -c SHA256SUMS
```

Debian 或 Ubuntu 的 x86_64 主机：

```sh
sudo apt install ./mosh-rs_0.1.3-1_amd64.deb
```

Fedora 的 x86_64 主机：

```sh
sudo dnf install ./mosh-rs-0.1.3-1.x86_64.rpm
```

openSUSE 的 x86_64 主机：

```sh
sudo zypper install --allow-unsigned-rpm ./mosh-rs-0.1.3-1.x86_64.rpm
```

arm64 主机改用文件名以 `arm64.deb` 或 `aarch64.rpm` 结尾的安装包。
每个包都包含客户端和服务端，并通过包管理器安装 OpenSSH 客户端依赖。
安装包使用静态 musl 构建；本项目暂未提供 apt 或 RPM 软件源。

### 压缩包

[GitHub Releases](https://github.com/harryisfish/mosh/releases) 中的每个压缩包
都包含 `bin/mosh-rs` 和 `bin/mosh-rs-server`。

| 主机 | 压缩包目标平台 |
| --- | --- |
| Linux x86_64，glibc | `x86_64-unknown-linux-gnu` |
| Linux arm64，glibc | `aarch64-unknown-linux-gnu` |
| Linux x86_64，musl 或 glibc | `x86_64-unknown-linux-musl` |
| Linux arm64，musl 或 glibc | `aarch64-unknown-linux-musl` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |

将 `SHA256SUMS` 下载到压缩包所在目录，解压前执行校验：

- macOS：`shasum -a 256 --ignore-missing -c SHA256SUMS`
- Linux：`sha256sum --ignore-missing -c SHA256SUMS`

例如，在 Apple Silicon Mac 上安装 0.1.3：

```sh
tar -xzf mosh-rs-0.1.3-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 mosh-rs-0.1.3-aarch64-apple-darwin/bin/mosh-rs \
	mosh-rs-0.1.3-aarch64-apple-darwin/bin/mosh-rs-server "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
```

GNU Linux 压缩包在 Ubuntu 22.04 x86_64 和 Ubuntu 24.04 arm64 上构建，并需要 glibc。
静态 musl 压缩包也适用于 Alpine 和 glibc 较旧的 Linux 主机。macOS 二进制文件未签名。

## 连接

沿用现有的 OpenSSH 认证和配置：

```sh
mosh-rs user@host
```

指定固定的 UDP 端口并运行命令：

```sh
mosh-rs --port 60001 user@host -- htop
```

如果服务端不在远程 SSH 环境的 `PATH` 中，指定其路径：

```sh
mosh-rs --server /home/user/.local/bin/mosh-rs-server user@host
```

服务端以 SSH 登录用户的身份运行。启动后，终端数据通过直达服务端的 UDP 连接传输。
请在服务端防火墙中放行所选 UDP 端口。未指定 `--port` 时，由操作系统分配端口，
客户端会在进入终端前打印端口号。`--ssh-port` 只改变 SSH 端口。
仅配置 SSH 跳板机不能提供 UDP 路由。

## 使用与断线恢复

网络中断或休眠期间保持客户端进程运行。它会重连到仍在运行的远程会话，并重试未确认的输入。
关闭客户端或客户端崩溃后，不会留下供新客户端恢复的会话文件。
服务端会话进程崩溃会结束会话，不会重新启动命令。

遗留的断线会话会在 `--idle-seconds` 指定的期限后过期，默认值为 `86400` 秒。

先按 `Ctrl-^`，再按下列按键：

| 按键 | 操作 |
| --- | --- |
| `.` | 关闭本地客户端 |
| `q` | 结束远程命令 |
| `Ctrl-Z` | 挂起本地客户端，运行 `fg` 返回 |
| `?` | 显示转义命令帮助 |

直接按 `Ctrl-Z` 会发送给远程程序。

## 终端能力与限制

- 支持 UTF-8 字素、组合字符、宽字符单元格、RGB 颜色、下划线样式和光标形状。
- 支持 OSC 8 超链接、OSC 7 目录、OSC 133 shell 标记和同步输出。
- 支持 Kitty 键盘事件、括号粘贴、鼠标输入和焦点事件。
- 支持规定范围内的 Kitty、SIXEL 和 iTerm 内联静态图片。`--graphics auto` 为识别出的终端
  选择 Kitty 或 SIXEL 输出，其他终端使用文本输出。可用 `--graphics kitty`、
  `--graphics sixel` 或 `--graphics none` 覆盖自动选择。
- 支持 OSC 52 剪贴板写入。读取剪贴板需要启用 `--clipboard-read`。
- 对符合条件的可打印 ASCII 输入进行自适应预测。在观察到远程输出前，以带下划线的文字
  显示预测。使用 `--predict never` 关闭预测。

不支持动画图片或任意终端扩展透传。客户端显示当前屏幕，不提供独立的终端历史搜索界面。
输入预测使用启发式判断，在远程终端模式切换时可能短暂显示输入内容。
本客户端不提供文件传输、端口转发或 X11 转发。

终端要求、图片限制、预测行为和资源上限详见[使用指南](docs/rust-usage.md)。

## 源码构建与开发

在 Linux 或 macOS 上安装 Rust 1.88 或更新版本及 C 工具链，然后构建：

```sh
cargo build --release --locked
```

在客户端安装 `target/release/mosh-rs`，在服务端安装 `target/release/mosh-rs-server`。
请分别为每台主机的操作系统和架构构建。

在本机体验客户端和服务端：

```sh
target/release/mosh-rs --local
```

详细文档目前为英文：

- [使用指南与验证命令](docs/rust-usage.md)
- [Rust 协议参考](docs/rust-protocol.md)
- [Mac mini 验收报告](docs/rust-macmini-verification.md)
- [版本发布维护](docs/rust-release.md)

## 来源与许可

本项目基于 [Mosh](https://mosh.org/)。上游作者和致谢记录保留在
[AUTHORS](AUTHORS) 与 [THANKS](THANKS) 中。

本项目采用 [GPL-3.0-or-later](COPYING) 许可证。
内置的 WezTerm 终端核心保留其 [MIT 许可证](rust/vendor/wezterm-term/LICENSE.md)。
