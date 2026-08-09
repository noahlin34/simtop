# simtop Homebrew formula template.
#
# Not released yet: this file is a template that is structurally complete but
# deliberately refuses to load until a release maintainer fills in the release
# coordinates below. It must not be presented as installable.
#
# Before the first tagged release, replace the remaining values in the
# class body:
#   RELEASE_VERSION   the release tag without the leading "v"
#                     (e.g. "0.1.0" for tag "v0.1.0")
#   RELEASE_SHA256    the SHA-256 of the release source archive
#                     (`shasum -a 256 <archive>` after downloading it)
#
# Until both are replaced, loading this formula raises a release-maintainer
# error immediately: nothing is downloaded, built, installed, or verified,
# so no placeholder can ever be mistaken for real release metadata or used
# to install the wrong archive.
#
# homepage is omitted on purpose: the project has not published one, and a
# fabricated value would be worse than an absent one (brew audit will flag
# it at publish time, when the real value is known).
class Simtop < Formula
  desc "High-performance iOS Simulator management TUI and automation CLI"
  license "MIT"

  # Release coordinates (template placeholders — see notes above).
  RELEASE_OWNER = "noahlin34"
  RELEASE_VERSION = "REPLACE_WITH_VERSION"
  RELEASE_SHA256 = "REPLACE_WITH_SHA256"

  raise <<~EOS if [RELEASE_OWNER, RELEASE_VERSION, RELEASE_SHA256].any? { |value| value.include?("REPLACE_WITH_") }
    simtop is not released yet, so this formula is not usable.

    A release maintainer must first replace the template placeholders in
    Formula/simtop.rb:
      RELEASE_VERSION   -> the release tag without the leading "v"
      RELEASE_SHA256    -> SHA-256 of the release source archive

    Until then the formula refuses to load on purpose, so the placeholders can
    never be used to install or verify anything.
  EOS

  url "https://github.com/#{RELEASE_OWNER}/simtop/archive/refs/tags/v#{RELEASE_VERSION}.tar.gz"
  sha256 RELEASE_SHA256

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
