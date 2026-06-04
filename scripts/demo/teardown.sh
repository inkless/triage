#!/usr/bin/env bash
#
# teardown.sh — remove the triage demo environment created by seed.sh.
# Kills the demo tmux session and deletes the sandbox $HOME. Safe to run
# repeatedly; touches nothing outside the demo sandbox.
set -euo pipefail

DEMO_HOME="${TRIAGE_DEMO_HOME:-/tmp/triage-demo}"
TMUX_SESSION="triage-demo"

tmux kill-session -t "$TMUX_SESSION" 2>/dev/null && echo "killed tmux session $TMUX_SESSION" || echo "no tmux session $TMUX_SESSION"
rm -rf "$DEMO_HOME" && echo "removed $DEMO_HOME"
