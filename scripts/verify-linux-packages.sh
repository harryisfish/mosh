#!/usr/bin/env bash
set -euo pipefail

release_target=${1:?Usage: verify-linux-packages.sh <musl-rust-target>}
repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"
case "$release_target" in
  x86_64-unknown-linux-musl) package_arch=amd64; rpm_arch=x86_64 ;;
  aarch64-unknown-linux-musl) package_arch=arm64; rpm_arch=aarch64 ;;
  *) printf 'Unsupported package verification target: %s\n' "$release_target" >&2; exit 1 ;;
esac
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' rust/Cargo.toml)
export ARCHIVE="mosh-rs-$version-$release_target.tar.gz"
export PACKAGE_DIR="mosh-rs-$version-$release_target"
export DEB_PACKAGE="mosh-rs_${version}-1_${package_arch}.deb"
export RPM_PACKAGE="mosh-rs-${version}-1.${rpm_arch}.rpm"
export EXPECTED_VERSION="mosh-rs $version"
docker_args=(--rm -v "$repo_root/target/dist:/packages:ro" -e ARCHIVE -e PACKAGE_DIR -e DEB_PACKAGE -e RPM_PACKAGE -e EXPECTED_VERSION)

docker run "${docker_args[@]}" alpine:3.22 sh -euxc '
  tar -xzf "/packages/$ARCHIVE" -C /tmp
  test "$(/tmp/$PACKAGE_DIR/bin/mosh-rs --version)" = "$EXPECTED_VERSION"
  test "$(/tmp/$PACKAGE_DIR/bin/mosh-rs-server --version)" = "$EXPECTED_VERSION"
'
for image in debian:bookworm-slim ubuntu:22.04; do
  docker run "${docker_args[@]}" "$image" sh -euxc '
    apt-get update
    apt-get install -y --no-install-recommends "/packages/$DEB_PACKAGE"
    test "$(mosh-rs --version)" = "$EXPECTED_VERSION"
    test "$(mosh-rs-server --version)" = "$EXPECTED_VERSION"
    test -f /usr/share/doc/mosh-rs/copyright
    test -f /usr/share/doc/mosh-rs/wezterm-copyright
    test -x /usr/bin/ssh
  '
done
docker run "${docker_args[@]}" fedora:44 sh -euxc '
  dnf install -y "/packages/$RPM_PACKAGE"
  test "$(mosh-rs --version)" = "$EXPECTED_VERSION"
  test "$(mosh-rs-server --version)" = "$EXPECTED_VERSION"
  test -f /usr/share/doc/mosh-rs/copyright
  test -f /usr/share/doc/mosh-rs/wezterm-copyright
  test -x /usr/bin/ssh
'
docker run "${docker_args[@]}" opensuse/leap:16.0 sh -euxc '
  zypper --non-interactive install --allow-unsigned-rpm --no-recommends "/packages/$RPM_PACKAGE"
  test "$(mosh-rs --version)" = "$EXPECTED_VERSION"
  test "$(mosh-rs-server --version)" = "$EXPECTED_VERSION"
  test -f /usr/share/doc/mosh-rs/copyright
  test -f /usr/share/doc/mosh-rs/wezterm-copyright
  test -x /usr/bin/ssh
'
