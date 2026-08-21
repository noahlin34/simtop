# simtop Homebrew formula.
#
# This formula builds the tagged source release. Homebrew installs Rust as a
# build dependency; running simtop requires macOS 15 or newer and Xcode 16.
class Simtop < Formula
  desc "High-performance iOS Simulator management TUI and automation CLI"
  homepage "https://github.com/noahlin34/simtop"
  url "https://github.com/noahlin34/simtop/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "3ba85858f644f3e1b603c36f729a65e1b9f7a594ea567b3a2c026abcbf61dbc2"
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
