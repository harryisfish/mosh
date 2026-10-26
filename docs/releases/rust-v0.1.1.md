# mosh-rs 0.1.1

This patch fixes two terminal compatibility problems in 0.1.0.

- Arrow keys and other function keys using Kitty press, repeat, or release
  events no longer terminate the client with an invalid input parameter error.
  Terminal replies such as DECRPM are ignored by the keyboard decoder.
- Default foreground and background colors follow your local terminal theme.
  Explicit black, ANSI palette colors, RGB colors, and application changes to
  default colors still work. SGR and OSC resets restore the correct defaults.

The existing verification examples and CLI checks cover these regressions,
including real PTY input, rendered default and explicit colors, and terminal
restoration after exit. The release workflow runs the CLI checks against each
extracted native package.

This release uses protocol v2. Install the paired client and server, then start
a new connection. Existing sessions keep running their original binaries.
Use `SHA256SUMS` to verify the downloaded archive.
