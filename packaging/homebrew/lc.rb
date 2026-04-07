# Homebrew formula for the Lootcoin CLI wallet (lc).
#
# This file lives in the lootcoin repo for reference. To publish it, copy it
# into a tap repo (github.com/<you>/homebrew-lootcoin) so users can install
# with:
#
#   brew tap mcrepeau/lootcoin
#   brew install lc
#
# After each release:
#   1. Update `version` below.
#   2. Replace the sha256 values with those from the release's checksums.txt
#      (printed in the "Publish release" job summary on GitHub Actions).
#   3. Commit and push the tap repo.

class Lc < Formula
  desc "CLI wallet for Lootcoin"
  homepage "https://github.com/mcrepeau/lootcoin"
  version "3.2.0"
  license "AGPL-3.0-only"

  on_macos do
    on_arm do
      url "https://github.com/mcrepeau/lootcoin/releases/download/v#{version}/lc-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_CHECKSUM_FROM_RELEASE"
    end
    on_intel do
      url "https://github.com/mcrepeau/lootcoin/releases/download/v#{version}/lc-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_CHECKSUM_FROM_RELEASE"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/mcrepeau/lootcoin/releases/download/v#{version}/lc-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_CHECKSUM_FROM_RELEASE"
    end
    on_intel do
      url "https://github.com/mcrepeau/lootcoin/releases/download/v#{version}/lc-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_CHECKSUM_FROM_RELEASE"
    end
  end

  def install
    bin.install "lc"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/lc --version")
  end
end
