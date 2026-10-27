# Publish a Rust release

## Prepare the release

Work on a separate branch. Update the version in `rust/Cargo.toml` and the
`mosh-rs` entry in `Cargo.lock`. Add `docs/releases/rust-v<VERSION>.md` and update
installation examples in both READMEs and `docs/rust-usage.md`.

Open a PR and check the Rust release workflow. It builds these targets:

| OS | CPU | Targets |
| --- | --- | --- |
| Linux | x86_64 | `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` |
| Linux | arm64 | `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl` |
| macOS | Apple Silicon | `aarch64-apple-darwin` |
| macOS | Intel | `x86_64-apple-darwin` |

Each target produces an archive containing both binaries. The musl jobs also
produce `.deb` and `.rpm` packages with nFPM 2.48.0, using `packaging/nfpm.yaml`.
They verify static linking and install the packages in distribution containers.
The archive checks run `scripts/verify-rust-cli.py --network` on the extracted
binaries.

## Publish the tag

After merging the PR, tag its commit. For version 0.1.2:

```sh
git tag rust-v0.1.2 <merged-commit>
git push origin rust-v0.1.2
```

The tag must match the Cargo package version. The tag workflow builds and
verifies all six targets, checks the combined `SHA256SUMS`, and publishes six
archives, two `.deb` packages, and two `.rpm` packages with the release notes.
Manual workflow dispatch builds packages without publishing a release.

## Update Homebrew

Update the formula from the published release, on a new branch. Do not use
checksums from a PR build or guess checksums before publication.

```sh
gh release download rust-v0.1.2 --repo harryisfish/mosh \
	--pattern SHA256SUMS --dir target/homebrew-0.1.2
python3 scripts/update-homebrew-formula.py 0.1.2 target/homebrew-0.1.2/SHA256SUMS
```

The generator pins four archive URLs and their SHA-256 checksums. It uses musl
archives for Linux when the manifest contains them, and GNU archives for older
releases.

Review `Formula/mosh-rs.rb`, install it through the tap, and run both binaries
with `--version`. Run `brew test harryisfish/mosh/mosh-rs`, then submit the
formula update as a PR. Publishing a release does not write to the default
branch.

The public installation commands are:

```sh
brew tap harryisfish/mosh https://github.com/harryisfish/mosh
brew install harryisfish/mosh/mosh-rs
```
