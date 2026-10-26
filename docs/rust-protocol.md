# Rust terminal protocol

This is the implementation contract for the Rust client and server. The protocol
is not compatible with the original Mosh wire protocol.

The current Rust protocol is version 2, with ALPN `mosh-rs/2`. Both endpoints
must use this version. Version 1 workers cannot resume with a version 2 client.

## Ownership and authentication

The session actor owns one PTY and one terminal core. The network tasks never
write the PTY. A disconnected connection cannot cancel accepted PTY input.
Each connection has a strictly increasing attach attempt. The actor accepts
only a larger attempt, assigns a new epoch, and checks that epoch when executing
every input, resize, acknowledgement, detach, and close message.

System OpenSSH authenticates a structured bootstrap request. Commands travel as
JSON argv, never shell interpolation. The client generates a session client
certificate. The server trusts only that certificate. The client trusts only
the server certificate returned over SSH. Both use normal rustls certificate
validation, the `mosh.local` server name, and QUIC without early application data.

## Independent confirmation domains

`InputRecord.seq` is an ordered input event number. An event is encoded once.
The actor retains the exact encoding and short-write position until all its
bytes enter the PTY. Only then does it report `Committed`. Duplicate pending
events must have the same content. Retries never restart a command or a PTY.
A dead worker ends the session. No crash-recovery transaction is claimed.

Display revisions are separate. A frame is a checkpoint or a set of row
replacements relative to a confirmed revision. The sender retains bounded
roots, replaces unsent obsolete state, and cancels obsolete display streams.
The client checks epoch, revision, and base before installation. Once local
output starts, its sole writer completes and flushes the frame before confirming
it. Images have immutable content hashes and separate low-priority streams.
Control messages use a four-byte big-endian length followed by bounded postcard
encoding. The receiver rejects trailing bytes and checks resource-request and
effect collection limits during decoding. Enum variant order and struct field
order are part of the versioned wire format. SSH bootstrap remains newline JSON.
Frames use bounded postcard encoding and zstd compression. Image keys hash their
dimensions followed by their RGBA bytes. A cache miss explicitly requests the
missing resources. A base mismatch requests a checkpoint. Application exit has its own final acknowledgement.

Local suspension retains the client input queue, display revision, cached screen,
and effect acknowledgements. Foreground recovery redraws cached state without
replaying effects or shell markers. A long suspension uses the same reattachment
path as network loss. A background client leaves the local terminal to its shell
and can exit on TERM or HUP. Explicit suspension restores modes before stopping;
external SIGSTOP cannot provide that cleanup.

`echo_through` advances only after PTY output has been observed following input.
It is a prediction lifetime bound, not proof that a particular glyph was echoed.
`PredictionContext` carries mode generation and PTY echo eligibility separately
from input commits. Prediction is a reversible local overlay. It never sends
corrective input to the remote process. Mode changes and mismatches remove it.

## Modern terminal state

Rows contain graphemes and widths, RGB colors, underline kind and color, style
flags, hyperlink parameters and URL, and image placements. Cursor, title,
directory, shell markers, reverse video, and prediction context accompany every
frame. Server input encoding uses its current keyboard, mouse, focus, and paste
modes. Key press, repeat, release, alternate key codes, and associated text remain
distinct semantic fields. Paste is UTF-8 text, with nested paste markers removed.

Static Kitty direct-data, SIXEL, and inline image data are decoded under limits
before display projection. Files and shared memory are never forwarded or read.
Parser accumulation, decompression, decoding, unplaced uploads, live resources,
and client caches all have bounds. Rejected image commands cannot bypass the
bounds through a graphics query. Animated images are outside this profile.
Static PNG input retains critical chunks and transparency data; unused color
profiles, text, EXIF, and other ancillary metadata are discarded before decoding.
The terminal uses the decoder's existing RGBA conversion without ICC color
management. Discarded metadata is not validated, including its CRC; retained
chunks follow the decoder's existing validation and tolerance rules.

Effects have monotonic IDs and a display-revision barrier. Reconnection replays
only unapplied effects for the same client process. An overflow advances an
explicit floor; expired clipboard queries receive a refusal. Clipboard reads
require local opt-in. Results, refusals, and timeouts enter the actor's ordered
PTY writer. External effects are not transactional across a client crash.

## Resource limits

Windows are at most 512 columns by 256 rows. A control message is at most 512 KiB;
a decoded or compressed frame is at most 16 MiB. An image is at most 16 MiB RGBA.
Each cell contains at most 4096 image tiles, and a screen at most 262144 tiles.
The frame byte budget can require a smaller tile limit.
Each current screen, the core's decoded image store, and the adapter's exported
cache have separate 64 MiB limits. The actor retains at most 128 MiB of unique
resources across display roots plus two independent resource transfers. A client
cache holds at most 64 MiB, in addition to the installed screen and three bounded
stream readers. Its network queue has four entries. These limits do not describe
total process RSS. Parser and decoder budgets are in the core maintenance notes.
Pending client input is at most 256 KiB, with a bounded input decoder and captured
input batch. The effect log is at most 1 MiB and 256 entries. Saturation never
silently drops keyboard input. Control traffic
has a bounded queue and connection flow-control capacity reserved from large
display and image transfers. Current and in-flight image references stay pinned.
There is one display sender and at most two image senders. The connection has a
4 MiB receive window with at most three 1 MiB display/image stream windows, leaving
capacity for the single control stream. Stable newer state or a substantially
smaller replacement can cancel an obsolete display stream. Repeated stable-frame
cancellation is bounded so large checkpoints can still finish.

QUIC migration and application reattachment are separate mechanisms. Neither
Rust nor this protocol repairs an unavailable UDP path or OS network denial.
