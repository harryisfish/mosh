#!/usr/bin/env bash
set -euo pipefail

release_target=${1:?Usage: package-native-release.sh <musl-rust-target>}
repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"

case "$release_target" in
  x86_64-unknown-linux-musl) package_arch=amd64; rpm_arch=x86_64 ;;
  aarch64-unknown-linux-musl) package_arch=arm64; rpm_arch=aarch64 ;;
  *) printf 'Native packages require a supported musl target: %s\n' "$release_target" >&2; exit 1 ;;
esac
command -v nfpm >/dev/null 2>&1 || { printf 'nFPM 2.48.0 is required\n' >&2; exit 1; }

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' rust/Cargo.toml)
build_root=${CARGO_TARGET_DIR:-target}
output_dir="$repo_root/target/dist"
mkdir -p "$output_dir"
export MOSH_RS_PACKAGE_VERSION="$version"
export MOSH_RS_PACKAGE_ARCH="$package_arch"
export MOSH_RS_BIN_DIR="$(cd "$build_root/$release_target/release" && pwd)"
for binary in mosh-rs mosh-rs-server; do
  test -x "$MOSH_RS_BIN_DIR/$binary"
done

deb="mosh-rs_${version}-1_${package_arch}.deb"
rpm="mosh-rs-${version}-1.${rpm_arch}.rpm"
nfpm package --config packaging/nfpm.yaml --packager deb --target "$output_dir/$deb" >&2
nfpm package --config packaging/nfpm.yaml --packager rpm --target "$output_dir/$rpm" >&2
(
  cd "$output_dir"
  shasum -a 256 "$deb" > "$deb.sha256"
  shasum -a 256 "$rpm" > "$rpm.sha256"
)
printf '%s\n' "$output_dir/$deb" "$output_dir/$rpm"
