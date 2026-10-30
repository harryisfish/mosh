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
connection metadata and the SSH hostname. The worker's returned IP must match
that explicit bind, including equivalent IPv4-mapped addresses. Without a
concrete bind, the client uses the effective hostname from local `ssh -G` for
UDP and requires that query to succeed before starting the remote worker.
It does not fall back to an address chosen by the worker. Reconnection resolves
the same hostname again.
By default the server listens on both available address families using one
session port. An explicit `--bind` keeps its chosen interface and family:
`--bind 0.0.0.0` selects IPv4 and `--bind ::` selects IPv6 only. A fixed-port
conflict fails startup rather than silently changing the port or address family.
macOS uses separate IPv4 and IPv6 listeners because one IPv6 socket cannot
reliably reserve an already occupied IPv4 port. Both listeners share the session.
An SSH jump host alone does not provide a UDP route.

The remote command requires a usable UTF-8 locale on the server. The client
sends its `LC_ALL`, `LC_CTYPE`, and `LANG` values as candidates. The server checks
their actual encoding, then tries its own environment and common UTF-8 locales.
On macOS and GNU Linux, if those fail, it reads the server's installed names
with `locale -a`. On Linux with musl, it runs `locale -a` first and requires
each candidate to appear in that list. Discovery has output and runtime limits.
Musl requires a working locale utility even when a usable UTF-8 locale is
already configured.
It removes `LC_ALL` from the command's environment and sets `LC_CTYPE` and `LANG`
to the selected locale. If startup reports `no usable UTF-8 locale found`,
use `locale -a` on the server to find an installed UTF-8 locale, or install or
generate one, then set one of these variables before connecting again.

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

When the pending input budget is full, the client keeps reading local commands
and explicitly refuses new ordinary input. Refused input receives no event
number, is not predicted, and is never sent after reconnection. Input resumes
when the pending budget falls to half its limit; the status asks you to retype
the refused content. A complete escape fallback group is accepted or refused
together. Keyboard phases received in separate reads are admitted separately;
a refused release may require you to press and release the key again.

Bracketed paste is limited to 128 KiB. Oversized or invalid UTF-8 paste is
refused. The client discards oversized paste through its closing marker,
including any embedded local escape commands. An unfinished paste can still be
interrupted with an operating system signal. Clipboard replies share the input
budget; a refused reply is discarded and the remote query may time out.

Legacy ESC key prefixes have a fixed 30 ms deadline from their first ESC byte.
With no suffix, ESC becomes Escape, `ESC [` becomes Alt-[, and `ESC O` becomes
Alt-O. Later bytes do not extend that deadline. Unfinished parameterized CSI,
SS3, and Linux-console key sequences are refused with `Input sequence timed
out`; their tail is discarded through its final byte. Retype the key after the
notice. Ctrl-^ or a new ESC sequence can end this tail discard, so local commands
remain available. Suspension does not restart the keyboard deadline.

Paste and OSC/DCS/APC reply bodies retain their own parsing rules. An identified
X10 mouse event waits for its button and two coordinate bytes. These bodies do
not become keys after this deadline. A bare `ESC [` is indistinguishable from a
terminal reply whose next byte arrives after the deadline; the client uses the
Alt-[ convention in that case. Complete cursor reports still use the existing
filter, including the F3/CPR ambiguity when their bytes are identical.

Linux-console F1-F5 and xterm F21-F24 are sent as semantic keys and encoded for
the remote application's current keyboard mode. Keypad Begin and Equal retain
their normal, application-keypad, or Kitty meaning.

Press Ctrl-^, then `.` to close the local client, or Ctrl-^, then `q` to end the
remote command. Ctrl-^, then `^` sends a literal Ctrl-^. Ctrl-^, then `?` shows
these commands. Press Ctrl-^, then Ctrl-Z to return to the local shell without
ending the remote command. Run `fg` to restore the session, including its cached
text and images. Ordinary Ctrl-Z goes to the remote program. A client resumed
with `bg` waits for `fg` before reading or drawing on the terminal.

Press Ctrl-L to redraw the cached remote screen immediately, even during a
network outage or input refusal. This clears local predictions and restores
cached text, images, title, and directory without repeating bells, clipboard
writes, notifications, or shell markers. The original key still follows normal
input admission; a refused key is never sent later. Kitty press and repeat
events with primary `l` or `L` and Ctrl trigger redraw. Shift and Caps/Num lock
are allowed; other modifiers and release events do not trigger it. Paste text
does not trigger redraw.

`Ctrl-^ q` stops accepting new remote input and requests closure after the server
confirms the PTY writes for all previously accepted events. This does not
confirm that the remote application has processed every event. The client keeps
its connection until the remote exit and final display/effects are confirmed.
During an outage the status
remains unconfirmed. `Ctrl-^ .` or TERM, INT, or HUP leaves locally without
waiting for the network. If closure is unconfirmed, the client restores the
terminal and warns that the remote command may still be running. A client that
has lost foreground ownership cannot leave this warning on another job's
terminal.

Reconnection credentials live in the running client process.
Closing or crashing that process does not provide a resumable session file.
An abandoned worker expires after `--idle-seconds`, which defaults to 86400.
The server also retains unconfirmed final output within this lifetime.

The client probes the session actor during otherwise quiet sessions. After six
seconds without a confirmed application reply, its status shows `Reply
unconfirmed`, reply age, and traffic age. Screen output and QUIC acknowledgements
cannot clear that warning. `accepted inputs waiting` reports inputs accepted
locally that still lack a PTY commit. An application reply does not clear those
inputs or prove that the remote command consumed them. A fresh timely reply
shows `Application replies resumed` for two seconds. Normal idle sessions have
no recurring status redraw.

Suspension and SIGCONT begin a new observation window, so the suspended interval
does not immediately trigger a warning. Queued old replies cannot confirm
recovery. Local close and input-refusal notices keep their priority.
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
An orphan non-default-ignorable zero-width Mn/Me mark that can attach to NBSP
receives a one-column U+00A0 carrier. It is included in cell text, snapshots and
copied text; the original mark bytes are preserved. Format-only and default-ignorable marks
do not receive a carrier. Mixed prefixes and flush boundaries retain the limits
described in the maintenance notes.

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
python3 scripts/verify-rust-cli.py --saturated-input
python3 scripts/verify-rust-cli.py --legacy-input --evidence-dir /tmp/mosh-rs-legacy-input
python3 scripts/verify-rust-cli.py --redraw --evidence-dir /tmp/mosh-rs-redraw
python3 scripts/verify-rust-cli.py --combining --evidence-dir /tmp/mosh-rs-combining
python3 scripts/verify-rust-cli.py --soft-wrap --evidence-dir /tmp/mosh-rs-soft-wrap
python3 scripts/verify-rust-cli.py --liveness --evidence-dir /tmp/mosh-rs-liveness
```

These tools drive real core, QUIC, renderer, and PTY paths. The CLI tool creates
only its own local sessions. Its network option places an actual UDP relay
between the binaries, injects delay, jitter, loss, duplication, and reordering,
and verifies 8192 raw keys byte-for-byte. It also temporarily drops all packets
and checks the original command survives and queued input is applied once.
`--network` also runs the liveness cases. `--liveness` runs only healthy idle,
separate upstream and downstream loss, input confirmation, and SIGCONT cases.
The relay checks `drop_up` and `drop_down` on both packet enqueue and dequeue.
Its per-direction counters describe encrypted UDP traffic, not actor replies.
`--evidence-dir` saves raw TTY output and relay observations for these cases.
The SIGCONT case checks a fresh six-second warning window. It retains the
original eight-second recovery result and separately observes recovery for up
to 30 seconds from restoring the relay. This cap only bounds the fixture
observation.
`verify_session --verify-liveness` separately pauses only actor polling while
Quinn continues, then records raw probe replies and QUIC ACK counts.
`--network` also runs the legacy-input cases. `--legacy-input` checks real PTY
bytes for prefix deadlines, function and keypad keys, late-tail refusal,
management recovery, reply filters, UTF-8, Kitty phases, and SIGCONT.
Its evidence directory contains the TTY transcript and exact received bytes.
`--network` also runs the redraw cases. `--redraw` damages local cells and a
cached Kitty image while UDP is blocked, then checks Ctrl-L restores cells,
styles, cursor, and image through the terminal core. It checks key identity and
phase, group counts, resize, input refusal, closing, and the first-frame boundary.
It compares exact remote bytes and confirms one-time effects are not repeated.
`--network` also runs the combining cases. `--combining` checks leading marks,
erase/newline boundaries, byte-split UTF-8, repeated marks, normal bases,
format filtering and trailing selectors. It saves remote raw chunks, TTY
transcripts and process IDs, then checks exact cell text, widths and cursor
positions by replaying the CLI output through the terminal core.
`--network` also runs `--soft-wrap`. This case checks soft and hard URL rows,
wide characters, regional indicators, cached Ctrl-L, resize, and SIGCONT mode
restoration over a real PTY and UDP relay. Its evidence includes source chunks,
TTY output, process IDs, and replayed cells, styles, cursor, and wrap flags.
Reported frame and row metadata sizes come from reconstructed snapshots and
postcard encoding. They are not captured server frames or encrypted wire sizes.
These transcript checks do not verify native terminal visuals or selection.
They do not establish other-platform or comprehensive performance acceptance.

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

Both Rust endpoints must use protocol version 7. Earlier Rust workers
cannot resume with the new client. No compatibility with the old Mosh wire
protocol is provided. Worker crashes do
not cause the command to restart. Effects are deduplicated within the live client
process; external clipboard or notification side effects are not transactional
across a client crash. Scrollback remains bounded on the server; the client shows
the active screen and does not provide a separate searchable history browser.
