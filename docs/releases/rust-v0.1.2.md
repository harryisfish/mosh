# mosh-rs 0.1.2

This release adds installation options for Linux and Homebrew.

- Static musl archives for Linux x86_64 and arm64, including Alpine.
  The installer selects the musl archive on musl hosts.
- `.deb` packages for Debian and Ubuntu, and `.rpm` packages for Fedora and
  openSUSE, on both CPU architectures. These packages contain the static musl
  binaries and declare the OpenSSH client dependency.
- A Homebrew formula in this repository. Use `brew tap harryisfish/mosh
  https://github.com/harryisfish/mosh`, then `brew install harryisfish/mosh/mosh-rs`.
  The formula pins a published release and is updated separately after publication.

The release retains GNU Linux and macOS archives. All archives and native
packages contain both `mosh-rs` and `mosh-rs-server`. Verify downloads with the
attached `SHA256SUMS` before installing them. There is no project apt or RPM
repository.

The release workflow checks that musl binaries have no dynamic loader, runs the
CLI and network checks on each archive, and installs Linux packages in Alpine,
Debian, Ubuntu, Fedora, and openSUSE containers.

This release uses protocol v2. Install the same release on the client and server.
Existing sessions continue running their original binaries.
