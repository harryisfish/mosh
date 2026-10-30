class MoshRs < Formula
  desc "Resumable remote terminal with local input prediction"
  homepage "https://github.com/harryisfish/mosh"
  version "0.1.3"
  license "GPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.3/mosh-rs-0.1.3-aarch64-apple-darwin.tar.gz"
      sha256 "6663fc0a2bb5e7c93d8c4265307f61f84d38871abc21680d7039d957185f11c8"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.3/mosh-rs-0.1.3-x86_64-apple-darwin.tar.gz"
      sha256 "eab47132f5e0744ee6bf5ac746312c0479275940520a4decb798a4278ed5b11e"
    end
  end

  on_linux do
    depends_on "openssh"

    on_arm do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.3/mosh-rs-0.1.3-aarch64-unknown-linux-musl.tar.gz"
      sha256 "2fcf338329f064f36fb5e87ebeed9e445b6b8b4a0b8aa79b89744599f8ccce44"
    end
    on_intel do
      url "https://github.com/harryisfish/mosh/releases/download/rust-v0.1.3/mosh-rs-0.1.3-x86_64-unknown-linux-musl.tar.gz"
      sha256 "72634cc92b110d74d94a027ee9e0b45d276a56a38ece662936bb4d8914d2f4dd"
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
