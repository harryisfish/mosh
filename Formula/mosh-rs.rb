class MoshRs < Formula
  desc "Resumable remote terminal with local input prediction"
  homepage "https://github.com/harryisfish/mosh"
  version "0.1.2"
  license "GPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.2/mosh-rs-0.1.2-aarch64-apple-darwin.tar.gz"
      sha256 "f5780fe6972c85112b4f54ba15f14ad10203ce9bc9d61bbf927f018a04adcc83"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.2/mosh-rs-0.1.2-x86_64-apple-darwin.tar.gz"
      sha256 "ccf585d9fca517f9918af11f99ef064bf5814b5559107f528ee05f02e7251c6a"
    end
  end

  on_linux do
    depends_on "openssh"

    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.2/mosh-rs-0.1.2-aarch64-unknown-linux-musl.tar.gz"
      sha256 "f040c48580d9b590fa4f0bfb8f3bf26c204b696b8e975921a906884bc1a23222"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.2/mosh-rs-0.1.2-x86_64-unknown-linux-musl.tar.gz"
      sha256 "425a2f06c37436972fef5655bb195913962fdb8fec1ad2089726b6a484062160"
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
