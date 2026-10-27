# Rust terminal protocol

This is the implementation contract for the Rust client and server. The protocol
is not compatible with the original Mosh wire protocol.

The current Rust protocol is version 5, with ALPN `mosh-rs/5`. Both endpoints
must use this version. Earlier Rust workers cannot resume with a version 5 client.
Version 4 peers are rejected by bootstrap, grant, Attach, and ALPN validation.

## Ownership and authentication

The session actor owns one PTY and one terminal core. The network tasks never
write the PTY. A disconnected connection cannot cancel accepted PTY input.
Each connection has a strictly increasing attach attempt. The actor accepts
only a larger attempt, assigns a new epoch, and checks that epoch when executing
every input, resize, acknowledgement, detach, close, and probe message.

System OpenSSH authenticates a structured bootstrap request. Commands travel as
JSON argv, never shell interpolation. The client generates a session client
certificate. The server trusts only that certificate. The client trusts only
the server certificate returned over SSH. Both use normal rustls certificate
validation, the `mosh.local` server name, and QUIC without early application data.

For SSH sessions, only the client's explicit non-wildcard `--bind` IP or the
effective hostname from local `ssh -G` authorizes the UDP destination. The client
normalizes IPv4-mapped addresses before deciding whether a bind is a wildcard
and comparing it with the worker's grant. A concrete bind must match the grant's
exact IP. Without a concrete bind, the client requires a successful, bounded
SSH configuration query with one valid hostname before starting the worker,
then replaces the grant's address with that hostname. Query failure, timeout,
or a missing hostname ends bootstrap; neither the grant address nor the original
SSH target supplies a fallback. Reconnection resolves the same authorized
hostname again. Certificate validation remains required at that destination.
Local bootstrap trusts the executable explicitly selected by the caller,
including controlled local relays.

Default server listening uses one dual-stack UDP socket where it can own both
address families, explicitly disabling and verifying `IPV6_V6ONLY`. On macOS,
an IPv6 wildcard can bind over an existing IPv4 port even with V6ONLY disabled,
so the server always uses separate IPv6-only and native IPv4 listeners. Other
platforms also use separate listeners if single-socket dual-stack capability is
unavailable. Both listeners use the same port, certificate, session, and actor.
Only an unavailable address family permits a single-family fallback;
permissions and fixed-port conflicts fail startup. Split listeners each allow
16 pending incoming connections and 2 MiB of incoming buffers, preserving the
group's 32-connection and 4 MiB limits. The actor shares eight connection permits
across both families and starts only one PTY. Normal completion, errors, and
cancellation, including dropping an unpolled actor future, close the whole
listener group. Cross-family recovery between
separate QUIC endpoints reattaches to the same actor; it does not migrate one
QUIC connection between their independent routing tables.

An explicit bind keeps its selected interface and address family, with mapped
IPv4 normalized to native IPv4. `0.0.0.0` listens on IPv4 and `::` on IPv6 only;
omitting bind selects automatic dual-stack listening. Both automatic listeners
must bind the same OS-assigned or fixed port before the grant is published.

## UTF-8 locale and PTY startup

Since version 4, `BootstrapRequest` requires the `locale_candidates` field.
It contains at most eight locale names, each 1 to 128 ASCII bytes from
`[A-Za-z0-9_.@-]`. The decoder rejects invalid names and excess entries. The
client reads `LC_ALL`, `LC_CTYPE`, and `LANG` in that order, skips invalid or empty
values, and removes duplicates. The grant envelope retains version 3 framing.

The worker waits for the first authenticated, valid Attach before selecting a
locale or starting the command. It tries the client's candidates, the server's
`LC_ALL`, `LC_CTYPE`, and `LANG`, then `C.UTF-8`, `C.utf8`, `UTF-8`,
`en_US.UTF-8`, and `en_US.utf8`, skipping duplicate and invalid names. If these
candidates fail, it runs `locale -a` once and probes the installed names.
Discovery accepts at most 64 KiB and 1024 names and has a five-second deadline.
On Linux and macOS,
`newlocale(LC_CTYPE_MASK, ...)` and `nl_langinfo_l(CODESET, ...)` must confirm an
actual UTF-8 codeset. A locale name alone is insufficient. Each probe frees its
locale without changing process-global locale or environment state. Other
platforms return an explicit unsupported-platform error.

The child removes inherited `LC_ALL` and sets `LC_CTYPE` and `LANG` to the
selected locale. Other explicit `LC_*` values remain inherited. On Linux and
macOS, the worker reads the slave PTY's termios, ORs `IUTF8` into `c_iflag`, and
applies the attributes with `TCSANOW`, preserving all other flags and control
characters. The IUTF8 step is a no-op on other platforms.

If no candidate is usable, the worker reports `no usable UTF-8 locale found`
and asks the user to generate or install a server UTF-8 locale and
set `LC_ALL`, `LC_CTYPE`, or `LANG`. The startup error reaches the client before
the worker exits. The command does not start, the client treats the error as
fatal, and the worker cannot return to its prepared state. Reattachment uses
the running PTY and does not select a new locale.
Missing locale utilities, failed discovery, excess output, and discovery
timeouts have separate startup errors. A configured usable locale avoids the
discovery process.

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
order are part of the versioned wire format. SSH bootstrap requests remain newline JSON.
Frames use bounded postcard encoding and zstd compression. Image keys hash their
dimensions followed by their RGBA bytes. A cache miss explicitly requests the
missing resources. A base mismatch requests a checkpoint. Application exit has its own final acknowledgement.

## Actor application confirmation

Version 5 appends `Probe { epoch, nonce }` and `ProbeReply { epoch, nonce }` to
the control enums. The actor checks both the connection epoch and the message
epoch before scheduling a reply. The transport reader never replies itself.
The reply does not change PTY input, `Committed`, `echo_through`, display revision,
or effects. A QUIC ACK only confirms transport receipt.

The client keeps one unfinished nonce per active attachment. It queues the next
probe no earlier than two seconds after a timely reply. A full local output
queue does not create a flight, and another attempt waits two seconds. A flight
remains until its matching reply retires it or the link ends. A reply processed
six seconds after its probe was queued only retires that flight. Unknown,
duplicate, obsolete-epoch, or pre-resume replies cannot confirm recovery.
Recovery requires a new, timely nonce.

The actor retains one pending reply and the highest nonce seen. Repeated or
decreasing nonces do not generate another reply. A successful reply queue entry
waits at least two seconds before another. A full output queue retains the
pending reply and retries no more than once per second. Required commit and
exit messages run first. A closed queue disconnects the attachment.

The client records traffic receipt immediately after decoding, before waiting
for its UI event channel. Valid traffic can update last contact, but only a
timely, matching reply updates last reply. Accepted input timestamps stay in the
existing pending-input ledger. Only `Committed` or a validated `Attached` releases
those entries. Successfully installed frames separately record `echo_through`
and revision. Received resources and uninstalled frames cannot advance them.

Six seconds without an application reply shows an unconfirmed status, even if
screen traffic continues. Accepted inputs waiting six seconds show their count
and age independently. Continuing status updates occur at most once per second.
Recovery appears for two seconds. Healthy idle sessions produce no periodic TTY
redraw. The status cannot identify a failed network direction or prove that the
remote command consumed input.

Foreground recovery and SIGCONT restart the observation window. Existing flights
remain bounded and lose eligibility to confirm health. Their replies only retire
them before a fresh nonce can confirm recovery. Events decoded before the new
window cannot become new contact. Input waiting time uses the later of its
acceptance time and the new observation start. Closing retains its committed,
final display, and effects barriers. Once Close is queued in the current epoch,
the client stops sending new probes.

## Bootstrap grant framing

The SSH and local bootstrap stdout streams carry exactly one grant envelope.
Its first line is the ASCII record separator byte `0x1e`, followed by
`MOSH-RS-GRANT` and a newline. A single newline-terminated JSON `Grant` follows.
The last line is `0x1e` followed by `MOSH-RS-GRANT-END` and a newline.
The JSON body, including its newline, is at most 512 KiB. The detached worker
uses the same envelope for its parent bootstrap process.

The client copies at most 64 KiB of stdout diagnostics outside the envelope to
stderr. It keeps SSH stderr separate so authentication prompts remain visible.
It rejects missing, incomplete, duplicate, oversized, and unframed grants.
Grant bodies and JSON decoding details never become diagnostic output. The
client drains stdout through EOF and requires a successful bootstrap exit.
The parent bootstrap process reads only through the worker's complete envelope,
because the worker stays alive for the session. Request writes and grant reads
run concurrently. Bootstrap failure or timeout kills and reaps the local child.

Version 3 changes this required bootstrap framing. Its JSON fields and postcard
messages otherwise retain version 2's shape. Update both endpoints together.

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
