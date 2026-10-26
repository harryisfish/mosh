#!/usr/bin/env bash
set -euo pipefail

readonly RELEASE_BASE="https://github.com/harryisfish/mosh/releases"

die() {
	printf 'mosh-rs install: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<'EOF'
Install mosh-rs and mosh-rs-server from a GitHub release.

Usage:
  install.sh [--version VERSION] [--install-dir DIRECTORY]

Options:
  --version VERSION       Install a release such as 0.1.1. Defaults to latest.
  --install-dir DIRECTORY Install into DIRECTORY. Defaults to $HOME/.local/bin.
  -h, --help              Show this help.

Environment:
  MOSH_RS_VERSION         Same value as --version.
  MOSH_RS_INSTALL_DIR     Same value as --install-dir.
EOF
}

require_command() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

download() {
	local url=$1
	local output=$2
	curl -fsSL --retry 3 --proto '=https' --tlsv1.2 "$url" -o "$output"
}

resolve_target() {
	local os=$1
	local arch=$2
	case "$os:$arch" in
		Linux:x86_64|Linux:amd64)
			printf '%s\n' 'x86_64-unknown-linux-gnu'
			;;
		Linux:aarch64|Linux:arm64)
			printf '%s\n' 'aarch64-unknown-linux-gnu'
			;;
		Darwin:x86_64|Darwin:amd64)
			printf '%s\n' 'x86_64-apple-darwin'
			;;
		Darwin:arm64|Darwin:aarch64)
			printf '%s\n' 'aarch64-apple-darwin'
			;;
		*)
			die "unsupported platform: ${os} ${arch}"
			;;
	esac
}

normalize_version() {
	local version=$1
	version=${version#rust-v}
	[[ "$version" =~ ^[0-9]+(\.[0-9]+){2}([.-][0-9A-Za-z.-]+)?$ ]] \
		|| die "invalid release version: $1"
	printf '%s\n' "$version"
}

resolve_latest_version() {
	local latest_url
	latest_url=$(curl -fsSL --retry 3 --proto '=https' --tlsv1.2 \
		-o /dev/null -w '%{url_effective}' "$RELEASE_BASE/latest")
	latest_url=${latest_url%/}
	case "$latest_url" in
		"$RELEASE_BASE/tag/rust-v"*)
			normalize_version "${latest_url##*/}"
			;;
		*)
			die "cannot resolve the latest rust release from $latest_url"
			;;
	esac
}

version=${MOSH_RS_VERSION:-latest}
install_dir=${MOSH_RS_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}

while (($# > 0)); do
	case "$1" in
		--version)
			(($# >= 2)) || die "--version needs a value"
			version=$2
			shift 2
			;;
		--install-dir)
			(($# >= 2)) || die "--install-dir needs a value"
			install_dir=$2
			shift 2
			;;
		-h|--help)
			usage
			exit 0
			;;
		*)
			die "unknown option: $1"
			;;
	esac
done

for command_name in curl install mktemp tar uname awk; do
	require_command "$command_name"
done

if command -v sha256sum >/dev/null 2>&1; then
	hash_command=sha256sum
elif command -v shasum >/dev/null 2>&1; then
	hash_command=shasum
else
	die 'required command not found: sha256sum or shasum'
fi

[[ -n "$install_dir" ]] || die 'install directory cannot be empty'
case "$install_dir" in
	-*) die 'install directory cannot start with -' ;;
esac

target=$(resolve_target "$(uname -s)" "$(uname -m)")
if [[ "$version" == latest ]]; then
	version=$(resolve_latest_version)
else
	version=$(normalize_version "$version")
fi
tag="rust-v$version"
release_url="$RELEASE_BASE/download/$tag"
archive="mosh-rs-$version-$target.tar.gz"

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

archive_path="$tmpdir/$archive"
checksums_path="$tmpdir/SHA256SUMS"
download "$release_url/$archive" "$archive_path" \
	|| die "failed to download $archive"
download "$release_url/SHA256SUMS" "$checksums_path" \
	|| die "failed to download SHA256SUMS for $tag"

expected=$(awk -v name="$archive" '$2 == name || $2 == ("*" name) { print $1; exit }' "$checksums_path")
[[ "$expected" =~ ^[[:xdigit:]]{64}$ ]] \
	|| die "checksum for $archive is missing from SHA256SUMS"

if [[ "$hash_command" == sha256sum ]]; then
	actual=$(sha256sum "$archive_path" | awk '{print $1}')
else
	actual=$(shasum -a 256 "$archive_path" | awk '{print $1}')
fi
[[ "$actual" == "$expected" ]] || die "checksum mismatch for $archive"

tar -xzf "$archive_path" -C "$tmpdir" \
	|| die "failed to extract $archive"
package_dir="$tmpdir/mosh-rs-$version-$target"
client="$package_dir/bin/mosh-rs"
server="$package_dir/bin/mosh-rs-server"
[[ -x "$client" && -x "$server" ]] \
	|| die "release archive does not contain executable mosh-rs binaries"

mkdir -p "$install_dir" || die "cannot create install directory: $install_dir"
install -m 755 "$client" "$install_dir/mosh-rs" \
	|| die "cannot install mosh-rs into $install_dir"
install -m 755 "$server" "$install_dir/mosh-rs-server" \
	|| die "cannot install mosh-rs-server into $install_dir"

printf 'Installed mosh-rs %s for %s in %s\n' "$version" "$target" "$install_dir"
case ":${PATH:-}:" in
	*":$install_dir:"*) ;;
	*) printf 'Add %s to PATH before running mosh-rs.\n' "$install_dir" ;;
esac
