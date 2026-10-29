# Use the Rust client and server

## Install a release

Install the latest release into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/harryisfish/mosh/master/install.sh | bash
```

The script selects the host's OS, CPU, and Linux libc, then verifies the release
checksum before installing both binaries. Use `--install-dir DIRECTORY` to
choose another directory. To pin a release:

```sh
curl -fsSL https://raw.githubusercontent.com/harryisfish/mosh/master/install.sh | \
	bash -s -- --version 0.1.2
```

To inspect the script first, download it and run:

```sh
curl -fsSL https://raw.githubusercontent.com/harryisfish/mosh/master/install.sh -o install.sh
cat install.sh
bash install.sh
```

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
sudo apt install ./mosh-rs_0.1.2-1_amd64.deb
```

For Fedora on x86_64:

```sh
sudo dnf install ./mosh-rs-0.1.2-1.x86_64.rpm
```

For openSUSE on x86_64:

```sh
sudo zypper install --allow-unsigned-rpm ./mosh-rs-0.1.2-1.x86_64.rpm
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

For example, install version 0.1.2 on an Apple Silicon Mac:

```sh
tar -xzf mosh-rs-0.1.2-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 mosh-rs-0.1.2-aarch64-apple-darwin/bin/mosh-rs \
	mosh-rs-0.1.2-aarch64-apple-darwin/bin/mosh-rs-server "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
```

GNU Linux archives are built on Ubuntu 22.04 for x86_64 and Ubuntu 24.04 for
arm64, and require glibc. Static musl archives also run on Alpine and Linux hosts
with older glibc. The macOS binaries are unsigned.

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

## Diagnose a stalled session

Start a session with diagnostics enabled on both endpoints:

```sh
mosh-rs --diagnostics user@host
```

Before the terminal opens, the client prints a local JSONL path and the server
prints its remote JSONL path. Each file is in a private directory and has
owner-only permissions. Both endpoints need a version that supports
`--diagnostics`. The flag does not change the terminal wire protocol.

Save both files after a stall. Copy the remote file using its printed path:

```sh
scp user@host:/path/from/server/output/diagnostics.jsonl ./server-diagnostics.jsonl
```

The records describe connection transitions, input and PTY queue progress,
missing image resources, and time spent in transport, terminal parsing, frame
encoding, rendering, and local terminal writes. `attempt`, `epoch`, `sequence`,
and `revision` link observations within an endpoint. Each process measures time
from its own start, so compare these identifiers rather than subtracting client
and server timestamps.

Summary records contain cumulative stage counts, total and maximum durations
in microseconds, and byte counts. `pending_age_ms` lists the ages of unconfirmed
input, queued PTY writes, and missing resources, in that order. `high_water`
lists peak pending-input bytes, PTY-write bytes, control-queue entries,
event-queue entries, and missing-resource counts. `dropped` counts observations
skipped when diagnostic capacity is exhausted. Overlapping stages have separate
totals; adding those totals does not measure wall time.

An independent thread records operations that remain active for at least one
second. This includes synchronous terminal writes that block the main event
loop. A `waiting` record describes an observation, not a confirmed cause. A
`resumed` record means that the observed operation ended; it does not imply
success. Normal idle reads do not count as stalled operations.

Diagnostics are off by default. The files contain counters, durations, and
fixed event names. They exclude input text, screen contents, remote command
arguments, addresses, credentials, and dynamic error messages. Capture stops
at a 16 MiB limit per file. Start another session to collect a new capture.
Remove the files when they are no longer needed. No data is uploaded.

An in-process monitor cannot run while its process is stopped or the host is
asleep. A disk stall can also delay diagnostic output. Neither condition proves
a terminal protocol failure.

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

To verify diagnostics, including a blocked local terminal and a healthy idle
session, run:

```sh
python3 scripts/verify-rust-cli.py --network --diagnostics --evidence-dir /tmp/mosh-diagnostics-check
```

The diagnostic check also validates both endpoint logs and copies them to the
evidence directory. It checks the schema, file permissions, size limit, and
absence of fixture input and screen text.

The [Mac mini acceptance report](rust-macmini-verification.md) records real SSH,
debug/release, weak-network, Vim, tmux, and cmux checks, including their limits.

Both Rust endpoints must use protocol version 2. Earlier Rust version 1 workers
cannot resume with the new client. No compatibility with the old Mosh wire
protocol is provided. Worker crashes do
not cause the command to restart. Effects are deduplicated within the live client
process; external clipboard or notification side effects are not transactional
across a client crash. Scrollback remains bounded on the server; the client shows
the active screen and does not provide a separate searchable history browser.
