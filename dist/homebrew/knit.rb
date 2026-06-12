class Knit < Formula
  desc "Local-first CLI for coordinating cross-repo feature bundles"
  homepage "https://github.com/marc-merino/knit"
  license "Apache-2.0"
  version "0.1.0-alpha.1"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/marc-merino/knit/releases/download/v#{version}/knit-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    else
      url "https://github.com/marc-merino/knit/releases/download/v#{version}/knit-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/marc-merino/knit/releases/download/v#{version}/knit-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    else
      url "https://github.com/marc-merino/knit/releases/download/v#{version}/knit-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  def install
    bin.install "knit"
  end

  test do
    assert_match "knit", shell_output("#{bin}/knit --version")
  end
end
