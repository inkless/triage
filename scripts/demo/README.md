# triage demo

Generate a polished triage demo for the README **without exposing any real work**.
Everything is synthetic: a throwaway sandbox `$HOME` full of fake sessions +
transcripts, joined to a small tmux session of idle panes. The real `~/.claude`
is never read.

The README uses a static screenshot (`assets/demo.png`) — quickest path, no extra
tools. An animated asciinema recording is an optional upgrade (see below).

## Prerequisites

```sh
cargo install --path .   # ensure `triage` on PATH is current
# tmux is required (the seed spawns a `triage-demo` session of idle panes)
```

## Recipe (screenshot — the default)

```sh
# 1. Seed the synthetic environment (sandbox $HOME + tmux `triage-demo` panes).
scripts/demo/seed.sh

# 2. Sanity-check the fixtures render correctly — leak-free, non-interactive.
HOME=/tmp/triage-demo triage --probe
#   → 7 sessions, states: block / work / done / idle / long / stale / fresh

# 3. Launch the TUI and screenshot your terminal. TMUX= is unset so triage
#    launches its own TUI instead of attaching to your real triage; it still
#    sees the demo panes because `tmux list-panes -a` is server-wide.
env HOME=/tmp/triage-demo TMUX= triage
#   …take a screenshot (⌘⇧4 on macOS), save it to assets/demo.png, then `q`.

# 4. Tear down (kills the tmux session, deletes the sandbox $HOME).
scripts/demo/teardown.sh
```

## Optional: animated recording (asciinema)

```sh
brew install asciinema agg                          # agg converts .cast → .gif
scripts/demo/seed.sh
asciinema rec /tmp/triage-demo.cast -c "env HOME=/tmp/triage-demo TMUX= triage"
agg /tmp/triage-demo.cast assets/demo.gif           # then point README at the .gif
scripts/demo/teardown.sh
```

## Suggested key sequence (while recording)

Keep it ~15–20s. The rows are pre-sorted by attention priority, so the story
tells itself top-to-bottom:

1. Pause a beat on the full table — `block` at top, `stale` at bottom.
2. `j`/`k` (or arrows) down a couple rows to show the selection moving.
3. Land on `tetris-tui` (blocked) — the detail pane shows the pending `Edit`
   approval and Claude's reasoning.
4. `?` to flash the keybinding help overlay, then `?`/`esc` to close.
5. `A` to show the AUTO-mode indicator flip in the header (then `A` off).
6. `q` to quit.

Avoid `⏎ jump` / `r reply` on camera — jumping focuses a demo `sleep` pane and
breaks the illusion.

## Re-recording after a UI change

Just re-run the recipe — the seed regenerates timestamps relative to *now*, so
freshness (`AGE` column, idle/stale classification) is always correct at record
time. No fixture is hand-dated.

## Fully-automated alternative (VHS)

asciinema is a live capture (you drive the keys). If you want a byte-for-byte
reproducible recording, [VHS](https://github.com/charmbracelet/vhs) scripts the
keystrokes in a `.tape` file. The data setup is identical — seed first, then
point a `.tape` at `env HOME=/tmp/triage-demo TMUX= triage` and script the same
sequence above. Not committed here yet; asciinema is the default.

## How it stays leak-free

- `seed.sh` writes only under the sandbox `$HOME` (default `/tmp/triage-demo`)
  and a dedicated `triage-demo` tmux session. It reads nothing from your real
  environment.
- triage resolves sessions, transcripts, `.alive`, and config all under `$HOME`,
  so the sandbox fully isolates it from `~/.claude`.
- Pane pids come from idle `sleep` panes — `pid_alive` only checks liveness, not
  that the process is `claude`, so no real Claude session is involved.
