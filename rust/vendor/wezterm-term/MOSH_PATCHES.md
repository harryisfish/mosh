# Local changes

The application must call `Terminal::advance_bytes_checked` with its bounded
collector checks. The original upstream convenience API is retained for source
compatibility and is not the application's entry point.

- Remove the unbounded background channel and buffered writer. Every encoded
  byte reaches the application's synchronous collector. After each action and
  final text flush, the adapter checks the collector's sticky error. Errors
  poison the session model; an encoder is never retried after partial mutation.
- Add semantic key press/repeat/release, associated text and alternate codes.
  Kitty modifier bits retain hyper, meta and lock states. Keypad function codes
  retain their identity; legacy keypad encoding uses the core's application
  keypad mode. Media and modifier-only events without legacy terminal encodings
  produce no legacy bytes. F25-F35 likewise require the Kitty protocol.
- Bound incomplete parser strings, Kitty chunks, decompression, image decoding,
  unplaced images and cell placements. Kitty queries use the same validation and
  decoder as uploads, without creating image-number mappings. File and shared
  memory sources are rejected before loading. The upstream blob swap-out path
  is not used.
- Decode static Kitty RGB/RGBA/PNG, SIXEL and iTerm inline PNG/JPEG. Reject animated
  Kitty commands, APNG and other encoded formats. GIF, WebP and AVIF are outside
  this profile even when a particular file contains only one frame.
- Key private image deduplication by hash and dimensions. Track live images with
  weak references, including images retained by inactive screens or unplaced
  Kitty uploads. Export the live keys so the adapter can retain immutable RGBA
  Arcs while the core still references the source and remove obsolete copies.
- Bound user variables, Unicode mode stacks and accumulated titles. Join
  graphemes across PTY reads, with a 64-byte bound per grapheme.
- Give orphan zero-width Mn/Me marks a one-column NBSP carrier after the existing
  previous-cell join fails, if NBSP plus the entire original cluster is still
  one grapheme. This follows [Mosh 1.4.0's fallback representation](https://github.com/mobile-shell/mosh/blob/bc73a26316ede2a79259d859f8ee309b32412420/src/terminal/terminalframebuffer.h#L191).
  The carrier counts toward the same 64-byte bound.
- Return Kitty success replies for explicit image IDs as well as image numbers.
- Intersect rectangular checksum queries with the viewport and sum modulo 65536.
  Count physical columns, including blank cells and wide-character continuations.
- Stop cursor and tab loops at their fixed point. Reverse-wrap cursor movement
  skips complete cycles after executing one cycle. Insert/delete operations use
  the remaining margin width, and Kitty keyboard pops truncate the stack.

# Budgets and error behavior

The adapter retains 128 scrollback rows. At the maximum 512×256 viewport,
primary/alternate/scrollback cell counts and each grapheme are bounded. Exported
text plus hyperlink strings are limited to 4 MiB per snapshot. A counting postcard
writer checks projected cell encoding against the frame budget before cells are
retained in the snapshot; it never allocates a giant intermediate frame buffer.
These content limits apply even when the requested window size itself is valid.

The orphan carrier is part of `cell.text`, so snapshots and copied text include
the synthetic U+00A0. Original mark bytes and order remain unchanged; NFC is not
enabled. Default-ignorable marks cannot trigger a carrier. The exclusions are
U+034F, U+17B4-U+17B5, U+180B-U+180D, U+180F, U+FE00-U+FE0F and U+E0100-U+E01EF,
from [Unicode 16's Default_Ignorable_Code_Point table](https://www.unicode.org/Public/16.0.0/ucd/DerivedCoreProperties.txt)
intersected with the locked Mn/Me categories. Recheck them when the category or
width dependencies change. Format-only clusters and Mc keep their existing
behavior. White_Space keeps its existing one-column rule. Trailing variation
selectors or CGJ remain with a supported mark or an existing base.

No orphan prefix is retained across flushes. A CGJ prefix is preserved when it
arrives with an orphan mark, but is elided if it arrives alone first. Those forms
can therefore differ in text and at the 64-byte limit. U+0600 Prepend mixed with
an orphan mark still follows the old elision boundary when NBSP would form two
graphemes. This is not complete support for every zero-width Unicode cluster.

A decoded image is at most 16 MiB RGBA and 16384 pixels in either dimension.
Core live decoded images are at most 64 MiB, with at most 4096 image objects,
4096 Kitty chunks and 4096 placements. Image-cell associations are capped at
262144. The wire model and renderer share the same 4096-layer per-cell and
262144-tile per-screen limits, in addition to the frame byte budget.
Kitty accumulated encoded chunks are at most 24 MiB; decoded compressed or direct
payloads are at most 16 MiB. Image readers receive a 32 MiB logical
decoder budget before constructing the decoder, including dimension probes.
Before either PNG dimension probing or decoding, the shared input boundary
retains all critical chunks and tRNS, and discards unused ancillary metadata.
This prevents ICC and compressed text metadata from reaching the decoder.
APNG is rejected before filtering; unknown critical chunks reach the decoder.
The filter borrows metadata-free input and otherwise copies at most its encoded
length. Pixel expansion, transparency and 16-bit conversion remain with image/png.
ICC color management is outside this static profile. Discarded metadata and its
CRC are ignored; retained chunks follow the decoder's existing tolerance rules.
The decoder budget is still a dependency limit, not a total allocation or RSS cap.
OSC/APC strings are at most 24 MiB; SIXEL/DCS parser input is at most 256 KiB.
One REP action writes at most 32768 cells. Larger counts return an error.

Core lines may retain columns outside the current viewport after resize or image
placement. The adapter exports only visible columns without trimming core storage.
A wide glyph crossing the right edge exports a space until both columns fit.
Hidden image tiles remain available when the window grows again.

The adapter's exported image cache is a separate maximum of 64 MiB. Its immutable
RGBA Arcs are shared by display roots and transfers; they are not copied every
snapshot. This cache is distinct from the core's Vec-backed RGBA storage.
Parser/decode scratch space, screen cells, retained roots and protocol queues
have separate budgets. The 64 MiB number is not a total process RSS guarantee.

The reply collector accepts at most 256 KiB per call, never blocks, and either
accepts the full write or records a sticky error. The session actor is the only
real PTY writer and owns short writes and input commits. None of the core's
writer methods acknowledge execution by the remote application.

Resource violations and malformed image commands return errors to the adapter;
the caller must terminate that poisoned model. Kitty queries with invalid image
data can return protocol errors. Ordinary effect bursts do not poison the model:
the adapter coalesces bells and notifications, keeps the latest clipboard write
per selection, and immediately refuses excess clipboard reads. Query responses,
refusals and timeouts still use the actor's ordered PTY writer.

Synchronized output suppresses publication for at most 150 ms. The actor must
call `TerminalModel::publish_allowed` on its timer even without new PTY output.
Repeated enable commands cannot extend that deadline; disable re-arms it.

# Verification

From the Mosh workspace run `cargo run -p mosh-rs --example verify_core`.
The example exercises the actual modified core, not a mocked parser. It checks
attributes, split graphemes, Kitty keyboard phases/alternate codes/text, keypad,
mouse, paste, effects, query responses, synchronized-output expiry, static
images, size-aware resource identity, Arc reuse, deletion, unplaced-image quota,
chunk count, forbidden data sources, parser bounds and sticky collector errors.
Orphan carrier cases also pass through `state::frame` and `Display::install`,
including the client's separate single-grapheme validation. It does not verify
the network protocol, a real PTY or native terminal visuals. Other-platform and
comprehensive performance acceptance remain outside this verification.

Integration fixes: shell markers are observed after core actions, fresh-line-only
events are omitted, and `N` exports the resulting prompt boundary. Unicode
width changes across reads update pending-wrap state; resize carries pending-wrap
into the new viewport. Display metadata drops control characters and truncates at
UTF-8 boundaries. Integer image tiles cover fractional sampling intervals using
floor/ceil bounds; non-integral scales may overlap source pixels. Legacy shifted
symbols use the supplied shifted identity. All image dimension probes apply the
static format allowlist and decoder limits before constructing a decoder.
