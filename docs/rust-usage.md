# Use the Rust client and server

## Install a release

Download the archive for each host from
[GitHub Releases](https://github.com/harryisfish/mosh/releases).
Each archive contains both `bin/mosh-rs` and `bin/mosh-rs-server`.

| Host | Archive target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux arm64 | `aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |

Download `SHA256SUMS` into the same directory as the archive. Verify your download
with `shasum -a 256 --ignore-missing -c SHA256SUMS` on macOS or
`sha256sum --ignore-missing -c SHA256SUMS` on Linux.

For example, install release 0.1.0 on an Apple Silicon Mac:

```sh
tar -xzf mosh-rs-0.1.0-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 mosh-rs-0.1.0-aarch64-apple-darwin/bin/mosh-rs \
  mosh-rs-0.1.0-aarch64-apple-darwin/bin/mosh-rs-server "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
mosh-rs --local
```

Install the server host's matching package there as well. Use `--server` with
its absolute path when the remote SSH environment does not include its directory
in `PATH`.

Linux packages are built on Ubuntu 22.04 for x86_64 and Ubuntu 24.04 for arm64.
Build from source for older systems. The macOS binaries are unsigned.

## Build from source

Build on macOS or Linux with Rust 1.88 or newer and a C toolchain:

```sh
cargo build --release --locked
```

Install `target/release/mosh-rs` on the client and
`target/release/mosh-rs-server` on the server. Build for each host's operating
system and architecture. Put the server in the remote user's `PATH`, or specify
its absolute path with `--server`.

## Connect

Connect with your existing OpenSSH authentication and configuration:

```sh
mosh-rs user@host
mosh-rs --port 60001 user@host -- htop
mosh-rs --server /home/user/bin/mosh-rs-server user@host -- /bin/sh -lc 'cd project && exec vim'
```

The server runs as the SSH user. The UDP port must be reachable directly from
the client. `--port` selects a fixed server port for firewall rules. Without it,
the operating system selects a port, which the client prints before entering
the terminal. `--ssh-port` changes the SSH port. `--bind` selects a server IP
to listen on and advertise; an explicit interface IP takes priority over SSH
connection metadata and the SSH hostname.
An SSH jump host alone does not provide a UDP route.

To run both binaries locally:

```sh
target/release/mosh-rs --local
target/release/mosh-rs --local -- /bin/sh
```

The client reconnects to the same live worker after a network interruption or
sleep. Unconfirmed input stays in a bounded queue and is retried with the same
event numbers. The server completes accepted partial PTY writes before reporting
an input commit. It skips obsolete screen updates rather than replaying a log of
all remote output. See [the protocol reference](rust-protocol.md).

Press Ctrl-^, then `.` to close the local client, or Ctrl-^, then `q` to end the
remote command. Ctrl-^, then `^` sends a literal Ctrl-^. Ctrl-^, then `?` shows
these commands. Press Ctrl-^, then Ctrl-Z to return to the local shell without
ending the remote command. Run `fg` to restore the session, including its cached
text and images. Ordinary Ctrl-Z goes to the remote program. A client resumed
with `bg` waits for `fg` before reading or drawing on the terminal.

Reconnection credentials live in the running client process.
Closing or crashing that process does not provide a resumable session file.
An abandoned worker expires after `--idle-seconds`, which defaults to 86400.
The server also retains unconfirmed final output within this lifetime.

## Terminal capabilities

| Capability | Behavior |
| --- | --- |
| Text | UTF-8 graphemes, combining characters, wide cells, RGB colors, styled and colored underlines, cursor shape and visibility |
| Hyperlinks | OSC 8 parameters and URL survive screen updates and reconnects |
| Input | Legacy keys, Kitty press/repeat/release, alternate key codes, associated text, UTF-8 bracketed paste, mouse movement and vertical/horizontal wheel, focus |
| Shell integration | OSC 7 directory and OSC 133 prompt/input/output/end markers |
| Synchronized output | Server publication and local frame installation respect synchronization, with a bounded server timeout |
| Clipboard | OSC 52 UTF-8 writes; reads require `--clipboard-read` and can be refused or time out |
| Images | Static Kitty direct RGB/RGBA/PNG, SIXEL, and iTerm inline PNG/JPEG become bounded image resources |

`--graphics auto` selects Kitty for recognized Kitty, WezTerm, and Ghostty
environments, including cmux, and SIXEL for recognized foot, mlterm, and Contour
environments. Other terminals use text output. Override with `--graphics kitty`,
`--graphics sixel`, or
`--graphics none` when the terminal's capability differs from its environment.
The terminal must support the selected output protocol. Automatic selection is
based on environment variables, not a complete terminal capability probe.

Kitty output preserves RGBA tiles and their layer order. SIXEL uses a 216-color
palette and an opacity threshold; it cannot reproduce all Kitty transparency or
negative-layer behavior. Cell-based crop projection can approximate fractional
scaling boundaries. Animated images, Kitty Unicode placeholders, local-file and
shared-memory image sources are outside this profile. The input mouse profile
uses cell coordinates; precise subcell pixels and extra mouse buttons are not
captured. Unknown terminal extensions are not forwarded as opaque bytes.

Adaptive prediction underlines eligible printable ASCII input while waiting for
observed remote output. In a raw full-screen application it first learns matching
echo behavior. Mode changes, a mismatch, expiry, or a new attachment retract the
overlay. `--predict conservative` uses PTY echo eligibility only;
`--predict never` disables prediction. Input commits are separate from echo
observations. Prediction is a heuristic and cannot guarantee that a secret will
never appear briefly during a remote mode transition.

The terminal model enforces content and memory limits. A legal window size alone
does not exempt oversized graphemes, hyperlinks, image objects, parser strings,
or effects from those limits. The supported static profile and exact quotas are
recorded in [the core maintenance notes](../rust/vendor/wezterm-term/MOSH_PATCHES.md).

## Run the verification tools

```sh
cargo fmt -p mosh-rs -- --check
cargo clippy -p mosh-rs --all-targets --locked -- -D warnings
cargo build -p mosh-rs --bins --examples --locked
target/debug/examples/verify_core
target/debug/examples/verify_transport
target/debug/examples/verify_display
target/debug/examples/verify_session
python3 scripts/verify-rust-cli.py --network
```

These tools drive real core, QUIC, renderer, and PTY paths. The CLI tool creates
only its own local sessions. Its network option places an actual UDP relay
between the binaries, injects delay, jitter, loss, duplication, and reordering,
and verifies 8192 raw keys byte-for-byte. It also temporarily drops all packets
and checks the original command survives and queued input is applied once.

The [Mac mini acceptance report](rust-macmini-verification.md) records real SSH,
debug/release, weak-network, Vim, tmux, and cmux checks, including their limits.

Both Rust endpoints must use protocol version 2. Earlier Rust version 1 workers
cannot resume with the new client. No compatibility with the old Mosh wire
protocol is provided. Worker crashes do
not cause the command to restart. Effects are deduplicated within the live client
process; external clipboard or notification side effects are not transactional
across a client crash. Scrollback remains bounded on the server; the client shows
the active screen and does not provide a separate searchable history browser.
