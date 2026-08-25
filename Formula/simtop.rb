# simtop Homebrew formula.
#
# This formula builds the tagged source release. Homebrew installs Rust as a
# build dependency; running simtop requires macOS 15 or newer and Xcode 16.
class Simtop < Formula
  desc "High-performance iOS Simulator management TUI and automation CLI"
  homepage "https://github.com/noahlin34/simtop"
  url "https://github.com/noahlin34/simtop/archive/refs/tags/v0.1.1.tar.gz"
  sha256 "76877cb4db0cdf7713e76223211945831abd0c73b9c2db1dd51ad1de6bbbfda6"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      simtop requires macOS 15 or newer with Xcode 16 (CoreSimulator).
    EOS
  end

  test do
    assert_match "simtop", shell_output("#{bin}/simtop --version")
  end
end
