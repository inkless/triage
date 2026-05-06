#!/usr/bin/env bash
# Jump to the tmux pane running `triage`. If none exists, spawn one in a new
# window of the current tmux session. Intended to be bound to a tmux key:
#   bind-key -n M-t run-shell "/path/to/triage/scripts/triage-jump.sh"
set -eu

# Look for an existing pane whose foreground command is `triage`. We match on
# the pane_current_command (what tmux thinks is in the foreground), which is
# the name of the running binary.
target=$(tmux list-panes -aF '#{pane_current_command}|#{session_name}:#{window_index}.#{pane_index}' \
  | awk -F'|' '$1=="triage" {print $2; exit}')

if [ -n "$target" ]; then
  session=${target%%:*}
  tmux switch-client -t "$session"
  tmux select-pane -t "$target"
  exit 0
fi

# No live triage pane — spawn one in a new window of the current session.
tmux new-window -n triage 'triage'
