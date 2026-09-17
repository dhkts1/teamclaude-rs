#!/usr/bin/env bash
# Repro for issue #323, at the reporter's terminal size (154x31).
# Runs examples/tui_garble_repro.rs in a detached tmux pane and captures the screen
# BEFORE and AFTER a background task panics. Control run: the same loop, no panic.
#
# Also reproduces the SECOND report (a build with the panic fix, no panic involved):
# something writes over the alternate screen, ratatui's diff can never repair it, and
# `desync-fixed` shows `r` doing what only a restart used to.
#
# Usage: scripts/repro-323.sh <panic|clean|fixed|desync|desync-fixed>
set -uo pipefail
mode=${1:-panic}
bin=${CARGO_TARGET_DIR:-target}/debug/examples/tui_garble_repro
[ -x "$bin" ] || { echo "build it first: cargo build --example tui_garble_repro"; exit 2; }

session="repro323-$mode-$$"
tmux kill-session -t "$session" 2>/dev/null
# The reporter's second terminal size for the desync modes, their first for the rest.
if [[ $mode == desync* ]]; then cols=190; rows=37; else cols=154; rows=31; fi
tmux new-session -d -s "$session" -x "$cols" -y "$rows" "$bin $mode"
sleep 0.4
echo "=== BEFORE (t=0.4s) ==="
tmux capture-pane -p -t "$session" | sed -n '1,12p'
sleep 1.2
echo "=== AFTER (t=1.6s, past the damage at 0.6s) ==="
tmux capture-pane -p -t "$session" | sed -n '1,20p'
if [[ $mode == desync* ]]; then
  tmux send-keys -t "$session" r
  sleep 0.6
  echo "=== AFTER PRESSING r (t=2.2s) ==="
  tmux capture-pane -p -t "$session" | sed -n '1,20p'
else
  sleep 1.5
  echo "=== LATER (t=3.1s) ==="
  tmux capture-pane -p -t "$session" | sed -n '1,20p'
fi
tmux kill-session -t "$session" 2>/dev/null
