# mosh-rs 0.1.3

This release adds opt-in diagnostics for intermittent terminal stalls.

Run `mosh-rs --diagnostics user@host` to record a private JSONL file on each
endpoint. Both paths appear before the terminal opens. The records describe
connection state, queue progress, and time spent in transport, PTY processing,
frame encoding, rendering, and local terminal writes. An independent monitor
can record a synchronous operation while the main event loop is blocked.

Diagnostics are off by default. Each capture stops at 16 MiB and excludes
terminal contents, input text, command arguments, addresses, and credentials.
A waiting record is evidence of unfinished work, not a confirmed root cause.
See [the diagnostic guide](https://github.com/harryisfish/mosh/blob/rust-v0.1.3/docs/rust-usage.md#diagnose-a-stalled-session).

Connection retry messages also show the local detach shortcut, `Ctrl-^ .`.

Install 0.1.3 on both endpoints to use diagnostics. The terminal protocol remains
v2. Existing sessions continue running their original binaries; new connections
use the updated installation.
