# mosh-rs 0.1.0

The first Rust release provides a client and server for persistent remote
terminal sessions over authenticated QUIC. SSH uses your existing login and
configuration to start the server. Both endpoints must come from this release.

Downloads include paired binaries for Linux and macOS on x86_64 and arm64,
installation instructions, protocol documentation, and licenses.
Use `SHA256SUMS` to verify the archive before installation.

## What's included

- A remote PTY that survives interrupted UDP connections, with input retry,
  screen synchronization, and local prediction.
- Unicode text, RGB colors, styled underlines, hyperlinks, Kitty keyboard
  phases, bracketed paste, mouse and focus events, and synchronized output.
- Shell integration, OSC 52 clipboard writes, optional clipboard reads,
  and static Kitty, SIXEL, and iTerm image input with documented output profiles.
- Local suspend with Ctrl-^ Ctrl-Z, `bg` and `fg`, and cached screen restoration.

Start with `mosh-rs --local`, then connect with `mosh-rs user@host`.
Use `--server /absolute/path/to/mosh-rs-server` if the remote binary is outside
the SSH user's `PATH`. The selected UDP port must be reachable from the client.

## Compatibility and validation

This release uses protocol v2. It does not interoperate with the original Mosh
protocol or Rust protocol v1. The original C++ commands remain available.
Reconnect credentials remain in the running client process, so a closed or
crashed client cannot reattach using a session file.

The release workflow checks extracted binaries with real PTYs, modern terminal
input, local job control, impaired UDP, and outage recovery on each native build.
Linux and macOS source checks and real SSH sessions on a Mac mini cover the
runtime release. See the bundled usage and Mac mini verification guides for
supported profiles and the full record.

Hardware sleep, network interface switching, public NAT, GUI-approved clipboard
reads, and visual output in real SIXEL or iTerm terminals remain unverified.
Linux binaries are built on Ubuntu 22.04 for x86_64 and Ubuntu 24.04 for arm64.
The macOS binaries are unsigned. Rust 1.88 is declared as the source minimum,
but that compiler version has not been independently verified.
