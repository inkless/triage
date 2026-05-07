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
# - Otherwise, capture the hook stdin payload. If the session is in
#   `permission_mode:"auto"`, exit 0 immediately: auto mode was never going to
#   ask the user, and blocking here would stall every tool call for no value.
# - For non-auto sessions, write the payload to
#   ~/.claude/triage/pending/<uuid>.json and briefly poll for a triage
#   decision. If one arrives, emit it to stdout and exit 0.
# - On timeout, exit 0 silently so Claude proceeds with its native permission
#   flow (auto-approve, terminal prompt, etc.). The timeout must stay short
#   because PreToolUse fires for every tool call, not just genuine prompts.

DIR="${HOME}/.claude/triage"
PENDING_DIR="${DIR}/pending"
DECISIONS_DIR="${DIR}/decisions"
ALIVE_FILE="${DIR}/.alive"
TIMEOUT_SECS=3

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
pending_tmp="${PENDING_DIR}/.${uuid}.tmp"

# Pipe stdin to a temp file first so we can cheaply inspect the payload before
# deciding whether to expose it to triage.
if ! cat > "$pending_tmp"; then
  exit 0
fi

# Auto mode was never waiting on the user; don't manufacture a pending
# approval for it. This avoids both false Blocked rows and per-tool stalls.
if grep -Eq '"permission_mode"[[:space:]]*:[[:space:]]*"auto"' "$pending_tmp"; then
  rm -f "$pending_tmp" 2>/dev/null
  exit 0
fi

if ! mv "$pending_tmp" "$pending_file"; then
  rm -f "$pending_tmp" 2>/dev/null
  exit 0
fi

# Always clean up temp/pending/decision files on exit (success, error, signal).
cleanup() {
  rm -f "$pending_tmp" "$pending_file" "$decision_file" 2>/dev/null
}
trap cleanup EXIT INT TERM

# --- 3. Wait for decision ----------------------------------------------------

deadline=$(( $(date +%s) + TIMEOUT_SECS ))
while [ ! -f "$decision_file" ]; do
  if [ "$(date +%s)" -gt "$deadline" ]; then
    # Timed out — let Claude's normal permission flow take over.
    exit 0
  fi
  sleep 0.5
done

# --- 4. Emit decision to Claude ---------------------------------------------

cat "$decision_file"
exit 0
