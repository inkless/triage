# Packaging & release

triage ships through three channels. The crate is named **`triage-tui`** on
crates.io (the name `triage` was taken); the installed **binary stays `triage`**
via the `[[bin]]` section in `Cargo.toml`.

## How a release happens

1. Land conventional-commit PRs on `main` (`feat:`, `fix:`, …).
2. [Release Please](https://github.com/googleapis/release-please) keeps an open
   "Release PR" that bumps `Cargo.toml` + `CHANGELOG.md`. Merging it tags
   `vX.Y.Z` and cuts a GitHub Release.
3. That triggers the `publish` job in `.github/workflows/release.yml`, which:
   - builds a **universal macOS binary** + the notify `.app`, tars them, and
     attaches `triage-macos-universal.tar.gz` to the Release;
   - **publishes `triage-tui` to crates.io** (if a token/Trusted Publishing is set up);
   - **bumps the Homebrew tap** formula (if the tap token is set up).

## One-time setup (required before the first real release)

These are **human prerequisites** — the workflow steps that need them are
guarded (`if: env.… != ''`) so the pipeline stays green until they exist:

- **crates.io** — either configure [Trusted Publishing](https://crates.io/docs/trusted-publishing)
  for the `triage-tui` crate (no secret needed; uses the workflow's `id-token`),
  **or** add a `CARGO_REGISTRY_TOKEN` repo secret. The crate name is reserved by
  the first successful publish — do an initial manual `cargo publish` if you want
  to claim `triage-tui` before automating.
- **Homebrew tap** — create the repo `inkless/homebrew-triage`, drop
  `packaging/homebrew/triage.rb` in it as `Formula/triage.rb`, and add a
  `HOMEBREW_TAP_TOKEN` repo secret (a PAT with `repo` scope on the tap). Then
  users install with `brew install inkless/triage/triage`.

## Validated vs. needs-validation

- ✅ **crates.io packaging** — `cargo publish --dry-run` passes. build.rs writes
  the `.app` into `OUT_DIR` (not the source tree), so verification succeeds, and
  it still stages the helper to `~/.config/triage` so `cargo install triage-tui`
  produces a working binary with notifications.
- ⚠️ **Homebrew** — the formula builds from source and installs the helper under
  `prefix/scripts/triage-notify/` to match triage's runtime probe. This path
  hasn't been exercised by a real `brew install` yet — validate on the first tap
  release (`brew install --build-from-source`, then confirm a notification fires).

## Known gotchas ("what goes wrong")

- **Gatekeeper on prebuilt binaries** — the universal tarball is **not** Developer
  ID-signed or notarized, so a downloaded binary is quarantined. Users must
  `xattr -dr com.apple.quarantine triage triage-notify.app` (or right-click →
  Open) once. The Homebrew/`cargo install` paths build from source and avoid this.
- **`brew` build sandbox `$HOME`** — build.rs stages the helper to `$HOME` during
  build, but brew's build `$HOME` is a sandbox, not the user's. That's why the
  formula additionally installs the `.app` into the prefix (where the runtime
  probe finds it) rather than relying on the `$HOME` stage.
- **CI runs on `macos-latest`** — triage is macOS-targeted (swiftc in build.rs,
  tmux/ps joins). Linux CI would skip the helper and risk platform-specific test
  drift, so we test on the real target. macOS minutes cost more — acceptable for
  a solo project.
- **crates.io name ≠ tool name** — the crate is `triage-tui`; never reference
  `triage` as the crate name in install docs. `cargo install triage-tui` →
  `triage` binary.
