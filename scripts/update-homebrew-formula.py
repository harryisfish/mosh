#!/usr/bin/env python3
"""Render the Homebrew formula from a published release checksum manifest."""

import argparse
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="Published version, such as 0.1.2")
    parser.add_argument("checksums", type=Path, help="The release SHA256SUMS file")
    parser.add_argument("--output", type=Path, default=Path("Formula/mosh-rs.rb"))
    args = parser.parse_args()
    version = args.version.removeprefix("rust-v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        parser.error("version must be a stable release such as 0.1.2")
    checksums = {}
    for line in args.checksums.read_text().splitlines():
        parts = line.split()
        if len(parts) != 2 or not re.fullmatch(r"[0-9a-fA-F]{64}", parts[0]):
            parser.error("invalid SHA256SUMS entry")
        name = parts[1].removeprefix("*")
        if name in checksums:
            parser.error(f"duplicate checksum for {name}")
        checksums[name] = parts[0].lower()

    def archive(target):
        return f"mosh-rs-{version}-{target}.tar.gz"

    targets = {
        "macos": {"arm": "aarch64-apple-darwin", "intel": "x86_64-apple-darwin"},
        "linux": {},
    }
    for cpu, arch in (("arm", "aarch64"), ("intel", "x86_64")):
        musl = f"{arch}-unknown-linux-musl"
        targets["linux"][cpu] = musl if archive(musl) in checksums else f"{arch}-unknown-linux-gnu"
    lines = [
        "class MoshRs < Formula",
        '  desc "Resumable remote terminal with local input prediction"',
        '  homepage "https://github.com/harryisfish/mosh"',
        f'  version "{version}"',
        '  license "GPL-3.0-or-later"',
        "",
    ]
    for os_name, cpus in targets.items():
        lines.append(f"  on_{os_name} do")
        if os_name == "linux":
            lines.extend(['    depends_on "openssh"', ""])
        for cpu, target in cpus.items():
            name = archive(target)
            if name not in checksums:
                parser.error(f"missing checksum for {name}")
            url = f"https://github.com/harryisfish/mosh/releases/download/rust-v{version}/{name}"
            lines.extend([
                f"    on_{cpu} do",
                f'      url "{url}"',
                f'      sha256 "{checksums[name]}"',
                "    end",
            ])
        lines.extend(["  end", ""])
    lines.extend([
        "  def install",
        '    bin.install "bin/mosh-rs", "bin/mosh-rs-server"',
        '    doc.install(*Dir["README*.md", "COPYING", "AUTHORS", "THANKS"])',
        '    doc.install "docs/rust-usage.md", "docs/rust-protocol.md"',
        '    pkgshare.install "rust/vendor/wezterm-term/LICENSE.md" => "wezterm-LICENSE.md"',
        "  end",
        "",
        "  test do",
        '    assert_match version.to_s, shell_output("#{bin}/mosh-rs --version")',
        '    assert_match version.to_s, shell_output("#{bin}/mosh-rs-server --version")',
        "  end",
        "end",
    ])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
