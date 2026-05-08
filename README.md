# triage

A Rust TUI to **triage parallel Claude Code sessions across tmux panes** — sort by attention priority, optionally let a Sonnet auditor handle routine approvals so you don't babysit every prompt.

Reads files Claude Code already writes:

- `~/.claude/sessions/<pid>.json` — discovery + `idle`/`busy` status
- `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` — recap, prompts, tool calls, per-message usage
- `tmux list-panes -a` — joined via process-ancestor walk from the Claude PID

Different shape from [`agentop`](https://crates.io/crates/agentop) (process-centric, token/cost focused). `triage` is content-centric: the headline column is the recap, the detail pane shows what the agent is doing and why, and auto mode (off by default) routes safe tool approvals through an LLM auditor.

## Install

```bash
cargo install --path .
triage              # launch the TUI
triage --probe      # print the joined session table once (no TUI)
```

## Keys

```
General:
  ↑↓ / j k         move selection
  ⏎                jump to selected session's tmux pane
  space            toggle detail panel
  /  (then text)   filter (substring across name/cwd/headline/pane)
  r                request manual refresh
  q / Ctrl-C       quit

Approve / deny / mute:
  a                approve (selected session must be paused on a permission prompt)
  d                deny
  h                cycle approve mode: hook ↔ tmux
  A                toggle autonomous mode (off → on)
  m                mute / unmute selected session

Audit-log overlay (only when auto mode is on):
  H                open / close
  j k / ↑↓         scroll one line
  ^d / ^u          half-page
  gg               jump to top
  G                jump to bottom
  Esc              close
```

## Attention states

Default sort order, highest-attention first:

| State | Meaning |
| --- | --- |
| `error` | Last `stop_hook_summary` reported errors. |
| `block` | Paused on a permission prompt (or `status=busy` + no events for 90s). |
| `done` | `Stop` within last 3 min — awaiting next prompt. |
| `work` | `status=busy` and progressing. |
| `idle` | `Stop` >3 min and <30 min ago. |
| `long` | `Stop` >30 min ago. |
| `fresh` | No user prompts seen yet. |
| `stale` | No transcript activity >24h. |
| `?` | Indeterminate. |

## Detail pane

Toggle with `space`. Three zones:

- **Header** — `state · pane · model (1M) · uptime · approve mode`.
- **Body** — agent's latest text (Claude's reasoning, often the *why* before the next tool call), pending tool + full input, recap (`away_summary`), last user prompt.
- **Stats footer** — auditor decision (when auto mode is on, with cost + duration), session cost + tokens + context-window % (yellow ≥80%, red ≥95%), event timing.

## Auto mode

Toggle with `A`. Off by default; persists across restart.

When on, each refresh spawns `claude -p --model claude-sonnet-4-6 --tools "" --name triage-auditor` for any Blocked session with a captured tool_use. The auditor receives the session's recent recap + intent + tool + full tool_input and returns `APPROVE` / `DENY` / `WAIT` with a one-line reason.

- `APPROVE` / `DENY` route through the same machinery as manual `a`/`d` (hook decision file when available, tmux send-keys fallback).
- `WAIT` surfaces the reason in the detail pane and leaves the prompt for human review.

Decisions append to `~/.config/triage/auto-decisions.jsonl` (one JSON object per line, includes cost + duration). Press `H` for the audit-history overlay.

**Safety**: the prompt explicitly approves routine repo work (Read/Glob/Grep, builds, tests, git ops, `gh pr create/edit`, file edits in the repo) and denies destructive actions (`rm -rf`, force-push to main, dropping data, `sudo`, shared-infrastructure writes). It WAITs when the action itself is in a middle zone — unfamiliar API, unreadable Bash flags, paths outside the repo. Customize via `~/.config/triage/auditor-prompt.md` (or `$TRIAGE_AUDITOR_PROMPT_FILE`).

Per-call budget is `--max-budget-usd 1.00`. Typical Sonnet round-trip: 10–25s and \$0.02–0.05 per audit.

## Hook setup (optional)

`a`/`d` in `hook` mode and auto mode both deliver decisions through a PreToolUse hook. To install:

```bash
triage --install-hooks-hint
```

This prints a JSON snippet to merge into `~/.claude/settings.json`. The hook (`scripts/hooks/triage-preuse.sh`) is zero-cost when triage isn't running. With auto mode on, the hook waits up to 60s (vs the default 3s) for the auditor's verdict via a claim-file handshake.

Without the hook installed, `h` falls back to `tmux` mode which sends keystrokes to the pane — works regardless of managed-policy settings.

## Cost & context-window tracking

Detail pane shows approximate session cost (per-message `usage` × per-model rates, deduplicated by `message.id`) and context-window occupancy as `current / total (%)`.

Context-window detection precedence:

1. `TRIAGE_CONTEXT_WINDOW` env var (explicit override, e.g. `1000000`)
2. Session's own `model` carries `[1m]`
3. `~/.claude/settings.json` `"model"` field has `[1m]` (e.g. `"opus[1m]"`) — the deterministic global signal
4. Per-session peak input tokens >210k → 1M
5. Fleet-wide peak >210k → 1M (any sibling session's evidence)
6. Default 200k

Cost figures are approximate; cross-check against `/cost` for the canonical per-session total.

## Environment variables

| Variable | Purpose |
| --- | --- |
| `TRIAGE_CONTEXT_WINDOW` | Override context-window size. Bypasses detection. |
| `TRIAGE_AUDITOR_PROMPT_FILE` | Custom auditor system prompt path. |
| `TRIAGE_TERMINAL_BUNDLE` | macOS terminal bundle ID for notification sender (auto-detected for kitty / ghostty / iTerm2 / Alacritty / WezTerm). |

## Design notes

- **Discovery + tmux join.** Sessions JSON keyed by Claude PID. Tmux's `pane_pid` is the shell; walk the process tree upward (up to 8 hops) until an ancestor matches a `pane_pid`.
- **Transcript pairing.** The active pane's session gets the jsonl with the newest qualifying user-text; remaining sessions pair greedily by mtime. Survives `/clear`.
- **Mechanical extraction in the live path.** Recap is `away_summary` (Claude-generated, no LLM in triage). Auditor is opt-in and runs only on Blocked sessions.
- **Hook is optional.** Triage works without any `~/.claude/settings.json` edits — the hook is needed only for clean approve/deny + auto-mode decision delivery.

## Status

`v0.2-dev` — local single-machine, macOS-tested. Auto mode + per-session cost + context-window % + audit-log overlay shipped. Not yet on crates.io.

## Stack

`ratatui` 0.30 + `crossterm` 0.29 + `notify` 8.2 + `serde_json` + `libc`. Rust edition 2024.

## License

MIT OR Apache-2.0
