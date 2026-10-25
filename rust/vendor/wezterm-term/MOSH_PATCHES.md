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
- Return Kitty success replies for explicit image IDs as well as image numbers.

# Budgets and error behavior

The adapter retains 128 scrollback rows. At the maximum 512×256 viewport,
primary/alternate/scrollback cell counts and each grapheme are bounded. Exported
text plus hyperlink strings are limited to 4 MiB per snapshot. A counting postcard
writer checks projected cell encoding against the frame budget before cells are
retained in the snapshot; it never allocates a giant intermediate frame buffer.
These content limits apply even when the requested window size itself is valid.

A decoded image is at most 16 MiB RGBA and 16384 pixels in either dimension.
Core live decoded images are at most 64 MiB, with at most 4096 image objects,
4096 Kitty chunks and 4096 placements. Image-cell associations are capped at
262144. Kitty accumulated encoded chunks are at most 24 MiB; decoded compressed
or direct payloads are at most 16 MiB. The decoder allocation limit is 32 MiB.
OSC/APC strings are at most 24 MiB; SIXEL/DCS parser input is at most 256 KiB.

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
It does not verify the network protocol, a real PTY or a local terminal renderer.

Integration fixes: shell markers are observed after core actions, fresh-line-only
events are omitted, and `N` exports the resulting prompt boundary. Unicode
width changes across reads update pending-wrap state; resize carries pending-wrap
into the new viewport. Display metadata drops control characters and truncates at
UTF-8 boundaries. Integer image tiles cover fractional sampling intervals using
floor/ceil bounds; non-integral scales may overlap source pixels. Legacy shifted
symbols use the supplied shifted identity. All image dimension probes apply the
static format allowlist and decoder limits before constructing a decoder.
