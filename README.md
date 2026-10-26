# mosh-rs

[English](README.md) | [简体中文](README.zh-CN.md)

[![Rust CI](https://github.com/harryisfish/mosh/actions/workflows/rust.yml/badge.svg)](https://github.com/harryisfish/mosh/actions/workflows/rust.yml)

A remote terminal in Rust that reconnects to the same command after network
interruptions, client sleep, or a change of network. Local input prediction
reduces typing latency. SSH authenticates and starts the server, then terminal
state travels over QUIC.

This project is a Rust rewrite of [Mosh](https://github.com/mobile-shell/mosh).
The client and server are `mosh-rs` and `mosh-rs-server`.

The Rust pair uses protocol v2 and is not compatible with the original Mosh wire
protocol or Rust protocol v1. Install both endpoints from the same release.

## Install

Download the archive for each host from
[GitHub Releases](https://github.com/harryisfish/mosh/releases). Each archive
contains `bin/mosh-rs` and `bin/mosh-rs-server`.

| Host | Archive target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux arm64 | `aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |

Download `SHA256SUMS` beside the archive. Verify it before extracting:

- macOS: `shasum -a 256 --ignore-missing -c SHA256SUMS`
- Linux: `sha256sum --ignore-missing -c SHA256SUMS`

For example, install version 0.1.1 on an Apple Silicon Mac:

```sh
tar -xzf mosh-rs-0.1.1-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 mosh-rs-0.1.1-aarch64-apple-darwin/bin/mosh-rs \
	mosh-rs-0.1.1-aarch64-apple-darwin/bin/mosh-rs-server "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
```

Install `mosh-rs` on your machine and `mosh-rs-server` on the remote host, using
each host's archive. Put the server in the remote SSH environment's `PATH`, or
pass its absolute path with `--server`.

Linux packages are built on Ubuntu 22.04 for x86_64 and Ubuntu 24.04 for arm64.
Build from source for older systems. The macOS binaries are unsigned.

## Connect

Use your existing OpenSSH authentication and configuration:

```sh
mosh-rs user@host
```

To select a fixed UDP port and run a command:

```sh
mosh-rs --port 60001 user@host -- htop
```

To select a server outside the remote SSH environment's `PATH`:

```sh
mosh-rs --server /home/user/.local/bin/mosh-rs-server user@host
```

The server runs as the SSH user. After bootstrap, terminal traffic uses a direct
UDP connection to the server. Allow the selected UDP port through its firewall.
Without `--port`, the operating system selects a port and the client prints it
before entering the terminal. `--ssh-port` changes only the SSH port. An SSH jump
host alone does not provide a UDP route.

## Use and reconnect

Keep the client process running during a network interruption or sleep. It
reconnects to the same live remote session and retries unconfirmed input. Closing
or crashing the client does not leave a session file for a new client to resume.
A server worker crash ends the session. It does not restart the command.

The server expires an abandoned session after `--idle-seconds`, which defaults
to `86400` seconds.

Press `Ctrl-^`, then one of these keys:

| Key | Action |
| --- | --- |
| `.` | Close the local client |
| `q` | End the remote command |
| `Ctrl-Z` | Suspend the local client. Run `fg` to return |
| `?` | Show the escape commands |

Ordinary `Ctrl-Z` goes to the remote program.

## Terminal support and limits

- UTF-8 graphemes, combining characters, wide cells, RGB colors, styled
  underlines, and cursor shape.
- OSC 8 hyperlinks, OSC 7 directories, OSC 133 shell markers, and synchronized
  output.
- Kitty keyboard events, bracketed paste, mouse input, and focus events.
- Supported static Kitty, SIXEL, and iTerm inline images. `--graphics auto`
  selects Kitty or SIXEL output for recognized terminals and text otherwise.
  Override with `--graphics kitty`, `--graphics sixel`, or `--graphics none`.
- OSC 52 clipboard writes. Clipboard reads require `--clipboard-read`.
- Adaptive prediction of eligible printable ASCII input. Predictions are
  underlined until remote output is observed. Use `--predict never` to disable it.

Animated images and arbitrary terminal extension passthrough are unsupported.
The client shows the active screen and has no separate searchable scrollback
browser. Prediction is a heuristic and can briefly expose input during a remote
mode transition. File transfer, port forwarding, and X11 forwarding are not provided.

See the [usage guide](docs/rust-usage.md) for terminal requirements, graphics
limitations, prediction behavior, and resource limits.

## Build and develop

Build on Linux or macOS with Rust 1.88 or newer and a C toolchain:

```sh
cargo build --release --locked
```

Install `target/release/mosh-rs` on the client and
`target/release/mosh-rs-server` on the server. Build for each host's operating
system and architecture.

To try both endpoints on the same machine:

```sh
target/release/mosh-rs --local
```

Further documentation:

- [Usage and verification commands](docs/rust-usage.md)
- [Rust protocol reference](docs/rust-protocol.md)
- [Mac mini acceptance report](docs/rust-macmini-verification.md)

## Origin and license

This project is based on [Mosh](https://mosh.org/). See [AUTHORS](AUTHORS) and
[THANKS](THANKS) for the upstream authors and acknowledgements.

This project is licensed under [GPL-3.0-or-later](COPYING). The vendored WezTerm
terminal core retains its [MIT license](rust/vendor/wezterm-term/LICENSE.md).
