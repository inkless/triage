# Homebrew formula for triage. Lives in the tap repo `inkless/homebrew-triage`
# (this copy is the canonical template; the release workflow's bump step keeps
# the tap's copy's `url`/`sha256` in sync).
#
# Builds from source so build.rs compiles the Swift notify helper on the user's
# own mac — no Developer ID signing / notarization needed, and click-to-jump
# notifications work out of the box. The helper is installed under
# prefix/scripts/triage-notify/ to match triage's runtime probe
# (`<exe_dir>/../scripts/triage-notify/triage-notify.app`).
class Triage < Formula
  desc "TUI to monitor parallel Claude Code and Codex CLI sessions across tmux panes"
  homepage "https://github.com/inkless/triage"
  url "https://github.com/inkless/triage/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_TARBALL_SHA256" # the bump action fills this on each release
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/inkless/triage.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos
  depends_on "tmux"

  def install
    # Crate is `triage-tui`; the [[bin]] is `triage`. std_cargo_args installs
    # the binary into bin/.
    system "cargo", "install", *std_cargo_args
    # Build + install the notify helper where triage's runtime probe finds it.
    system "bash", "scripts/triage-notify/build.sh", buildpath/"dist"
    (prefix/"scripts/triage-notify").install buildpath/"dist/triage-notify.app"
  end

  test do
    assert_match "triage", shell_output("#{bin}/triage --help 2>&1")
  end
end
