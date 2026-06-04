# Homebrew formula for triage. Lives in the tap repo `inkless/homebrew-triage`
# as Formula/triage.rb (this copy is the canonical template; the release
# workflow's bump step keeps the tap's url/version/sha256 in sync each release).
#
# Builds from the published crates.io source crate (`triage-tui`), so build.rs
# compiles the Swift notify helper on the user's own mac — no Developer ID
# signing / notarization needed, and click-to-jump notifications work out of
# the box. The helper is installed under prefix/scripts/triage-notify/ to match
# triage's runtime probe (`<exe_dir>/../scripts/triage-notify/triage-notify.app`).
#
# The crate is named `triage-tui` (the name `triage` was taken on crates.io);
# the installed binary is `triage` via the crate's [[bin]] section.
class Triage < Formula
  desc "TUI to monitor parallel Claude Code and Codex CLI sessions across tmux panes"
  homepage "https://github.com/inkless/triage"
  url "https://static.crates.io/crates/triage-tui/triage-tui-0.1.0.crate"
  version "0.1.0"
  sha256 "ac0779d23c649f89e1cae51dc92f03c83f9bf267645bcc1c0f56b74aafbf9522"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/inkless/triage.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos
  depends_on "tmux"

  def install
    system "cargo", "install", *std_cargo_args
    system "bash", "scripts/triage-notify/build.sh", buildpath/"dist"
    (prefix/"scripts/triage-notify").install buildpath/"dist/triage-notify.app"
  end

  test do
    assert_match "triage", shell_output("#{bin}/triage --help 2>&1")
  end
end
