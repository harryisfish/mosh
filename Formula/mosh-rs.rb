class MoshRs < Formula
  desc "Resumable remote terminal with local input prediction"
  homepage "https://github.com/harryisfish/mosh"
  version "0.1.6"
  license "GPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.6/mosh-rs-0.1.6-aarch64-apple-darwin.tar.gz"
      sha256 "cec1f4efbf2a22db788b77eaa0db0cad8b96aabc5c97485c9f98e2da96a2b89a"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.6/mosh-rs-0.1.6-x86_64-apple-darwin.tar.gz"
      sha256 "e5e74fc4bebf7ab40133bcc5f0a80139d20176fd15a1298ba2a6ee07d7b06f5f"
    end
  end

  on_linux do
    depends_on "openssh"

    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.6/mosh-rs-0.1.6-aarch64-unknown-linux-musl.tar.gz"
      sha256 "a0c26b1dc666beb5b9ece16429f714874ce46d5036ac761cf91e8b7819c9daab"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.6/mosh-rs-0.1.6-x86_64-unknown-linux-musl.tar.gz"
      sha256 "dff64855a1178e0ad934910d89449a881d536c68ca73ae9d0fdfbc810ce921d1"
    end
  end

  def install
    bin.install "bin/mosh-rs", "bin/mosh-rs-server"
    doc.install(*Dir["README*.md", "COPYING", "AUTHORS", "THANKS"])
    doc.install "docs/rust-usage.md", "docs/rust-protocol.md"
    pkgshare.install "rust/vendor/wezterm-term/LICENSE.md" => "wezterm-LICENSE.md"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mosh-rs --version")
    assert_match version.to_s, shell_output("#{bin}/mosh-rs-server --version")
  end
end
