#!/usr/bin/env bash
# Claude Code PreToolUse hook for triage approve/deny.
#
# Install by adding to ~/.claude/settings.json (merge with existing hooks):
#
#   {
#     "hooks": {
#       "PreToolUse": [
#         {
#           "matcher": ".*",
#           "hooks": [
#             {
#               "type": "command",
#               "command": "/Users/guangda.zhang/workspace/triage/scripts/hooks/triage-preuse.sh"
#             }
#           ]
#         }
#       ]
#     }
#   }
#
# Behavior:
# - If triage is not running (~/.claude/triage/.alive missing or stale), exit 0
#   so Claude's normal permission flow takes over. Zero overhead.
# - Otherwise, write the hook stdin payload to ~/.claude/triage/pending/<uuid>.json
#   and block waiting for ~/.claude/triage/decisions/<uuid>.json.
# - On user approve/deny in triage, emit the decision JSON to stdout and exit 0.
# - On 5-minute timeout, exit 1 (hook error → Claude falls back to its own prompt).

DIR="${HOME}/.claude/triage"
PENDING_DIR="${DIR}/pending"
DECISIONS_DIR="${DIR}/decisions"
ALIVE_FILE="${DIR}/.alive"
TIMEOUT_SECS=300

# --- 1. Bail out if triage isn't running -------------------------------------

if [ ! -f "$ALIVE_FILE" ]; then
  exit 0
fi
alive_pid=$(cat "$ALIVE_FILE" 2>/dev/null || echo "")
if [ -z "$alive_pid" ] || ! kill -0 "$alive_pid" 2>/dev/null; then
  rm -f "$ALIVE_FILE" 2>/dev/null
  exit 0
fi

# --- 2. Write pending file ---------------------------------------------------

mkdir -p "$PENDING_DIR" "$DECISIONS_DIR" 2>/dev/null || exit 0

uuid="$(uuidgen 2>/dev/null)"
if [ -z "$uuid" ]; then
  exit 0
fi
pending_file="${PENDING_DIR}/${uuid}.json"
decision_file="${DECISIONS_DIR}/${uuid}.json"

# Pipe stdin straight to disk. Hook input is plain JSON so this preserves
# everything triage needs (session_id, tool_name, tool_input, cwd, …).
if ! cat > "$pending_file"; then
  exit 0
fi

# Always clean up the pending+decision files on exit (success, error, signal).
cleanup() {
  rm -f "$pending_file" "$decision_file" 2>/dev/null
}
trap cleanup EXIT INT TERM

# --- 3. Wait for decision ----------------------------------------------------

deadline=$(( $(date +%s) + TIMEOUT_SECS ))
while [ ! -f "$decision_file" ]; do
  if [ "$(date +%s)" -gt "$deadline" ]; then
    # Timed out — let Claude's normal permission flow take over.
    exit 1
  fi
  sleep 0.5
done

# --- 4. Emit decision to Claude ---------------------------------------------

cat "$decision_file"
exit 0
