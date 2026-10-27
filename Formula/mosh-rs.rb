class MoshRs < Formula
  desc "Resumable remote terminal with local input prediction"
  homepage "https://github.com/harryisfish/mosh"
  version "0.1.1"
  license "GPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.1/mosh-rs-0.1.1-aarch64-apple-darwin.tar.gz"
      sha256 "dc8828c8585c5e1bedc39bebb4060fa8a36fdba1ad8ec0dd31c595df7b31c2ed"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.1/mosh-rs-0.1.1-x86_64-apple-darwin.tar.gz"
      sha256 "4b68d02100b21dc90d505ee2dfa878a6e1d9c6ea9b020748950d61a9cca0e983"
    end
  end

  on_linux do
    depends_on "openssh"

    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.1/mosh-rs-0.1.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "8e3432dee799b649857745d5f2a1a6581d7e7a32f60089eb4dec94075402c9e5"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.1/mosh-rs-0.1.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "835ea01369435e82f1ae2a0c95d4063709c583ebd5515c3101b348599c9728c0"
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
