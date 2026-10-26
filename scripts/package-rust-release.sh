#!/usr/bin/env bash
set -euo pipefail

release_target=${1:?Usage: package-rust-release.sh <rust-target>}
repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' rust/Cargo.toml)
package="mosh-rs-$version-$release_target"
build_root=${CARGO_TARGET_DIR:-target}
output_dir="$repo_root/target/dist"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$package/bin" "$stage/$package/docs" \
  "$stage/$package/rust/vendor/wezterm-term" "$output_dir"
cp "$build_root/$release_target/release/mosh-rs" \
  "$build_root/$release_target/release/mosh-rs-server" "$stage/$package/bin/"
cp README.md README.zh-CN.md COPYING "$stage/$package/"
cp docs/rust-usage.md docs/rust-protocol.md docs/rust-macmini-verification.md \
  "$stage/$package/docs/"
cp rust/vendor/wezterm-term/LICENSE.md rust/vendor/wezterm-term/MOSH_PATCHES.md \
  "$stage/$package/rust/vendor/wezterm-term/"

tar -czf "$output_dir/$package.tar.gz" -C "$stage" "$package"
(
  cd "$output_dir"
  shasum -a 256 "$package.tar.gz" > "$package.tar.gz.sha256"
)
printf '%s\n' "$output_dir/$package.tar.gz"
