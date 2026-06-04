#!/usr/bin/env bash
#
# seed.sh — build a fully synthetic, leak-free triage demo environment.
#
# Every triage data path keys off $HOME (sessions, transcripts, .alive, config),
# so we point triage at a throwaway sandbox $HOME stuffed with fake sessions and
# transcripts. tmux panes are listed server-wide, so a small `triage-demo` tmux
# session of idle `sleep` panes supplies real, live pids for the pane-join —
# `pid_alive` only checks the pid is alive, not that it's actually `claude`.
#
# Nothing real is ever read: the real ~/.claude is untouched, and all content
# below is author-controlled toy text.
#
# Usage:
#   scripts/demo/seed.sh            # seed, then print the record command
#   TRIAGE_DEMO_HOME=/tmp/x seed.sh # override sandbox location
#
# Teardown: scripts/demo/teardown.sh
set -euo pipefail

DEMO_HOME="${TRIAGE_DEMO_HOME:-/tmp/triage-demo}"
TMUX_SESSION="triage-demo"
SESSIONS_DIR="$DEMO_HOME/.claude/sessions"
PROJECTS_DIR="$DEMO_HOME/.claude/projects"

NOW="$(date +%s)"

# epoch (seconds-ago) → RFC3339 UTC "2026-06-03T20:00:00.000Z" (triage's parser
# is strict: 'T' at [10], trailing 'Z', UTC). BSD `date -r EPOCH` (macOS).
ts() { date -u -r "$((NOW - $1))" +%Y-%m-%dT%H:%M:%S.000Z; }
# seconds-ago → epoch-millis (sessions JSON updatedAt/startedAt are ms).
ms() { echo "$(((NOW - $1) * 1000))"; }
# cwd → encode_cwd: '/' and '.' become '-'.
encode_cwd() { echo "$1" | tr './' '-'; }

# ---- teardown any prior demo so re-seeding is idempotent --------------------
tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true
rm -rf "$DEMO_HOME"
mkdir -p "$SESSIONS_DIR" "$PROJECTS_DIR"

# ---- spawn one idle pane per row; capture pane_pid by window index ----------
# Each window runs `sleep 2147483647` (idle, harmless). The window's pane_pid is
# a live pid we plant into the matching session JSON.
ROWS=(tetris-tui rust-parser dns-explainer api-docs photo-organizer web-scraper haiku-bot)
tmux new-session -d -s "$TMUX_SESSION" -n "${ROWS[0]}" 'exec sleep 2147483647'
for ((i = 1; i < ${#ROWS[@]}; i++)); do
  tmux new-window -t "$TMUX_SESSION" -n "${ROWS[i]}" 'exec sleep 2147483647'
done

# window_index → pane_pid (indexed array; window indices are contiguous 0..N-1,
# so a plain array works on bash 3.2 — no associative arrays needed).
PANE_PID=()
while IFS=' ' read -r widx pid; do
  PANE_PID[$widx]=$pid
done < <(tmux list-panes -s -t "$TMUX_SESSION" -F '#{window_index} #{pane_pid}')

# ---- fixture emitters -------------------------------------------------------
# emit_session <sessionId> <cwd> <status> <pid> <updated_secs_ago> [waitingFor] [model]
emit_session() {
  local sid="$1" cwd="$2" status="$3" pid="$4" upd="$5" waiting="${6:-}" model="${7:-}"
  # Friendly row label: strip the demo- prefix and -NNNN suffix from the id.
  local disp="${sid#demo-}"; disp="${disp%-[0-9][0-9][0-9][0-9]}"
  local waiting_field="" version_field=""
  [ -n "$waiting" ] && waiting_field=",\"waitingFor\":\"$waiting\""
  # carry a [1m] model tag through the session version so the (1M) display path
  # has a deterministic signal even before transcript usage is parsed.
  [ -n "$model" ] && version_field=",\"version\":\"$model\""
  cat >"$SESSIONS_DIR/$sid.json" <<EOF
{"pid":$pid,"sessionId":"$sid","cwd":"$cwd","name":"$disp","status":"$status","startedAt":$(ms 7200),"updatedAt":$(ms "$upd")$waiting_field$version_field}
EOF
}

# transcript_path <sessionId> <cwd>
transcript_path() {
  local dir="$PROJECTS_DIR/$(encode_cwd "$2")"
  mkdir -p "$dir"
  echo "$dir/$1.jsonl"
}

# ---- row 0: tetris-tui — BLOCKED (status=waiting + pending Edit tool_use) ----
sid="demo-tetris-tui-0001"; cwd="/Users/dev/projects/tetris-tui"
emit_session "$sid" "$cwd" waiting "${PANE_PID[0]}" 60 "permission"
f="$(transcript_path "$sid" "$cwd")"
{
  echo "{\"type\":\"user\",\"timestamp\":\"$(ts 300)\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"add a line-clear flash animation to the board\"}]}}"
  echo "{\"type\":\"assistant\",\"timestamp\":\"$(ts 60)\",\"message\":{\"id\":\"msg_tetris_1\",\"model\":\"claude-sonnet-4-6\",\"content\":[{\"type\":\"text\",\"text\":\"I will edit the board renderer to flash cleared rows before collapsing them.\"},{\"type\":\"tool_use\",\"id\":\"toolu_tetris_1\",\"name\":\"Edit\",\"input\":{\"file_path\":\"src/board.rs\",\"old_string\":\"fn clear_rows\",\"new_string\":\"fn clear_rows_with_flash\"}}],\"usage\":{\"input_tokens\":1800,\"output_tokens\":260,\"cache_read_input_tokens\":54000}}}"
  echo "{\"type\":\"system\",\"subtype\":\"away_summary\",\"timestamp\":\"$(ts 50)\",\"content\":\"Adding a line-clear flash animation to the Tetris board renderer\"}"
} >"$f"

# ---- row 1: rust-parser — WORKING (status=busy, recent activity) ------------
sid="demo-rust-parser-0001"; cwd="/Users/dev/projects/rust-parser"
emit_session "$sid" "$cwd" busy "${PANE_PID[1]}" 15
f="$(transcript_path "$sid" "$cwd")"
{
  echo "{\"type\":\"user\",\"timestamp\":\"$(ts 200)\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"make the tokenizer handle nested block comments\"}]}}"
  echo "{\"type\":\"assistant\",\"timestamp\":\"$(ts 15)\",\"message\":{\"id\":\"msg_parser_1\",\"model\":\"claude-sonnet-4-6\",\"content\":[{\"type\":\"text\",\"text\":\"Running the test suite to check the nested-comment tokenizer.\"}],\"usage\":{\"input_tokens\":900,\"output_tokens\":140,\"cache_read_input_tokens\":31000}}}"
  echo "{\"type\":\"system\",\"subtype\":\"away_summary\",\"timestamp\":\"$(ts 16)\",\"content\":\"Teaching the tokenizer to handle nested block comments\"}"
} >"$f"

# ---- row 2: dns-explainer — DONE (JustFinished, idle, <3m) ------------------
sid="demo-dns-explainer-0001"; cwd="/Users/dev/code/dns-notes"
emit_session "$sid" "$cwd" idle "${PANE_PID[2]}" 70
f="$(transcript_path "$sid" "$cwd")"
{
  echo "{\"type\":\"user\",\"timestamp\":\"$(ts 240)\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"explain how recursive DNS resolution works\"}]}}"
  echo "{\"type\":\"assistant\",\"timestamp\":\"$(ts 72)\",\"message\":{\"id\":\"msg_dns_1\",\"model\":\"claude-sonnet-4-6\",\"content\":[{\"type\":\"text\",\"text\":\"Done. I wrote up the full recursive resolution path from stub resolver to root.\"}],\"usage\":{\"input_tokens\":1100,\"output_tokens\":540,\"cache_read_input_tokens\":22000}}}"
  echo "{\"type\":\"system\",\"subtype\":\"away_summary\",\"timestamp\":\"$(ts 70)\",\"content\":\"Explained recursive DNS resolution end to end, stub resolver through root\"}"
} >"$f"

# ---- row 3: api-docs — IDLE (IdleShort, idle, ~6m) --------------------------
sid="demo-api-docs-0001"; cwd="/Users/dev/projects/api-docs"
emit_session "$sid" "$cwd" idle "${PANE_PID[3]}" 360
f="$(transcript_path "$sid" "$cwd")"
{
  echo "{\"type\":\"user\",\"timestamp\":\"$(ts 900)\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"document the payments endpoints in OpenAPI\"}]}}"
  echo "{\"type\":\"assistant\",\"timestamp\":\"$(ts 362)\",\"message\":{\"id\":\"msg_apidocs_1\",\"model\":\"claude-sonnet-4-6\",\"content\":[{\"type\":\"text\",\"text\":\"Documented the v2 payments endpoints with request and response schemas.\"}],\"usage\":{\"input_tokens\":1500,\"output_tokens\":700,\"cache_read_input_tokens\":40000}}}"
  echo "{\"type\":\"system\",\"subtype\":\"away_summary\",\"timestamp\":\"$(ts 360)\",\"content\":\"Documented the v2 payments endpoints in OpenAPI\"}"
} >"$f"

# ---- row 4: photo-organizer — LONG (IdleLong, idle, ~45m) -------------------
sid="demo-photo-organizer-0001"; cwd="/Users/dev/projects/photo-organizer"
emit_session "$sid" "$cwd" idle "${PANE_PID[4]}" 2700
f="$(transcript_path "$sid" "$cwd")"
{
  echo "{\"type\":\"user\",\"timestamp\":\"$(ts 3600)\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"rename photos by EXIF capture date\"}]}}"
  echo "{\"type\":\"assistant\",\"timestamp\":\"$(ts 2702)\",\"message\":{\"id\":\"msg_photo_1\",\"model\":\"claude-opus-4-8[1m]\",\"content\":[{\"type\":\"text\",\"text\":\"Renamed 1,208 photos to their EXIF capture date and grouped them by month.\"}],\"usage\":{\"input_tokens\":2400,\"output_tokens\":480,\"cache_read_input_tokens\":215000}}}"
  echo "{\"type\":\"system\",\"subtype\":\"away_summary\",\"timestamp\":\"$(ts 2700)\",\"content\":\"Renamed 1,200 photos by EXIF capture date and grouped them by month\"}"
} >"$f"

# ---- row 5: web-scraper — STALE (status=busy but last event ~26h ago) -------
# Demonstrates the stale override: sessions JSON still says busy, but triage
# sinks it because no real activity in >24h.
sid="demo-web-scraper-0001"; cwd="/Users/dev/old/web-scraper"
emit_session "$sid" "$cwd" busy "${PANE_PID[5]}" 93600
f="$(transcript_path "$sid" "$cwd")"
{
  echo "{\"type\":\"user\",\"timestamp\":\"$(ts 96000)\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"scrape the product listings into a CSV\"}]}}"
  echo "{\"type\":\"assistant\",\"timestamp\":\"$(ts 93602)\",\"message\":{\"id\":\"msg_scraper_1\",\"model\":\"claude-sonnet-4-6\",\"content\":[{\"type\":\"text\",\"text\":\"Scraping the catalog pages into products.csv.\"}],\"usage\":{\"input_tokens\":800,\"output_tokens\":120,\"cache_read_input_tokens\":12000}}}"
  echo "{\"type\":\"system\",\"subtype\":\"away_summary\",\"timestamp\":\"$(ts 93600)\",\"content\":\"Scraping product listings into a CSV\"}"
} >"$f"

# ---- row 6: haiku-bot — FRESH (no user-text, no away_summary) ---------------
# Fresh requires user_prompt_count==0 AND headline==None: a non-empty transcript
# with only a non-prompt system event.
sid="demo-haiku-bot-0001"; cwd="/Users/dev/scratch/haiku"
emit_session "$sid" "$cwd" idle "${PANE_PID[6]}" 10
f="$(transcript_path "$sid" "$cwd")"
echo "{\"type\":\"system\",\"subtype\":\"init\",\"timestamp\":\"$(ts 10)\",\"content\":\"session started\"}" >"$f"

# ---- done -------------------------------------------------------------------
cat <<EOF

triage demo seeded.
  sandbox HOME : $DEMO_HOME
  tmux session : $TMUX_SESSION (${#ROWS[@]} idle panes)

Verify (non-interactive, leak-free):
  HOME=$DEMO_HOME triage --probe

Record (see scripts/demo/README.md for the full recipe):
  asciinema rec /tmp/triage-demo.cast -c "env HOME=$DEMO_HOME TMUX= triage"

Tear down when done:
  scripts/demo/teardown.sh
EOF
