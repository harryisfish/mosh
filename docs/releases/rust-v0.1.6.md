# mosh-rs 0.1.6

Pasted local PNG, JPEG, GIF, and WebP files are uploaded before their remote
paths reach the application. `Ctrl-V` also pastes clipboard images. macOS reads
PNG or converts TIFF to PNG and prefers original Finder files. Linux uses
`wl-paste` or `xclip` when the corresponding tool and local display are available.
Use `--no-image-paste` to retain the original path and key input behavior.

Typing after an image paste waits until the remote files are ready. After a
network interruption, the client retries the original image bytes. Uploaded
files survive reconnection and are removed when the server session ends.

Opt-in `--diagnostics` captures now survive session exit in persistent endpoint
state directories. Retention keeps the latest 32 captures per role. Server
captures include a same-session stderr sidecar for disconnect investigation.

Install 0.1.6 on both endpoints before starting a new session. This release uses
protocol v8 and ALPN `mosh-rs/8`. The new client cannot resume older-protocol
workers. Keep existing older-protocol sessions on their current clients until
those sessions end.
