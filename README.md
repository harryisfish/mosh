# mosh-rs

[English](README.md) | [简体中文](README.zh-CN.md)

[![Rust CI](https://github.com/harryisfish/mosh/actions/workflows/rust.yml/badge.svg)](https://github.com/harryisfish/mosh/actions/workflows/rust.yml)

A remote terminal in Rust that reconnects to the same command after network
interruptions, client sleep, or a change of network. Local input prediction
reduces typing latency. SSH authenticates and starts the server, then terminal
state travels over QUIC.

This project is a Rust rewrite of [Mosh](https://github.com/mobile-shell/mosh).
The client and server are `mosh-rs` and `mosh-rs-server`.

The current source uses protocol v7 and is not compatible with the original Mosh
wire protocol or earlier Rust protocols. Install both endpoints from the same release.

## Install

Install the latest release into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/harryisfish/mosh/master/install.sh | bash
```

The script selects the host's OS, CPU, and Linux libc, then verifies the release
checksum before installing both binaries. Use `--version 0.1.3` to pin a release
or `--install-dir DIRECTORY` to choose another directory. See the
[usage guide](docs/rust-usage.md#install-a-release) to review the script before running it.

Install `mosh-rs` on your machine and `mosh-rs-server` on the remote host.
Both endpoints must use the same release. Put the server in the remote SSH
environment's `PATH`, or pass its absolute path with `--server`.

### Homebrew

Use this repository as a tap on macOS or Linux:

```sh
brew tap harryisfish/mosh https://github.com/harryisfish/mosh
brew install harryisfish/mosh/mosh-rs
```

The formula installs both binaries from a pinned release with SHA-256 checksums.

### Debian, Ubuntu, Fedora, and openSUSE

Download your package and `SHA256SUMS` from
[GitHub Releases](https://github.com/harryisfish/mosh/releases), then verify it:

```sh
sha256sum --ignore-missing -c SHA256SUMS
```

For Debian or Ubuntu on x86_64:

```sh
sudo apt install ./mosh-rs_0.1.3-1_amd64.deb
```

For Fedora on x86_64:

```sh
sudo dnf install ./mosh-rs-0.1.3-1.x86_64.rpm
```

For openSUSE on x86_64:

```sh
sudo zypper install --allow-unsigned-rpm ./mosh-rs-0.1.3-1.x86_64.rpm
```

On arm64, use the `arm64.deb` or `aarch64.rpm` asset instead.
These packages include both binaries and install the OpenSSH client dependency.
They use static musl builds. There is no project apt or RPM repository.

### Archives

Each archive in [GitHub Releases](https://github.com/harryisfish/mosh/releases)
contains `bin/mosh-rs` and `bin/mosh-rs-server`.

| Host | Archive target |
| --- | --- |
| Linux x86_64, glibc | `x86_64-unknown-linux-gnu` |
| Linux arm64, glibc | `aarch64-unknown-linux-gnu` |
| Linux x86_64, musl or glibc | `x86_64-unknown-linux-musl` |
| Linux arm64, musl or glibc | `aarch64-unknown-linux-musl` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |

Download `SHA256SUMS` beside the archive. Verify it before extracting:

- macOS: `shasum -a 256 --ignore-missing -c SHA256SUMS`
- Linux: `sha256sum --ignore-missing -c SHA256SUMS`

For example, install version 0.1.3 on an Apple Silicon Mac:

```sh
tar -xzf mosh-rs-0.1.3-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 mosh-rs-0.1.3-aarch64-apple-darwin/bin/mosh-rs \
	mosh-rs-0.1.3-aarch64-apple-darwin/bin/mosh-rs-server "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
```

GNU Linux archives are built on Ubuntu 22.04 for x86_64 and Ubuntu 24.04 for
arm64, and require glibc. Static musl archives also run on Alpine and Linux hosts
with older glibc. The macOS binaries are unsigned.

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
- [Release maintenance](docs/rust-release.md)

## Origin and license

This project is based on [Mosh](https://mosh.org/). See [AUTHORS](AUTHORS) and
[THANKS](THANKS) for the upstream authors and acknowledgements.

This project is licensed under [GPL-3.0-or-later](COPYING). The vendored WezTerm
terminal core retains its [MIT license](rust/vendor/wezterm-term/LICENSE.md).
