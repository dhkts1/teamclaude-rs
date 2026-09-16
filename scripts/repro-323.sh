#!/usr/bin/env bash
# Repro for issue #323, at the reporter's terminal size (154x31).
# Runs examples/tui_garble_repro.rs in a detached tmux pane and captures the screen
# BEFORE and AFTER a background task panics. Control run: the same loop, no panic.
#
# Usage: scripts/repro-323.sh <panic|clean>
set -uo pipefail
mode=${1:-panic}
bin=${CARGO_TARGET_DIR:-target}/debug/examples/tui_garble_repro
[ -x "$bin" ] || { echo "build it first: cargo build --example tui_garble_repro"; exit 2; }

session="repro323-$mode-$$"
tmux kill-session -t "$session" 2>/dev/null
tmux new-session -d -s "$session" -x 154 -y 31 "$bin $mode"
sleep 0.4
echo "=== BEFORE (t=0.4s) ==="
tmux capture-pane -p -t "$session" | sed -n '1,12p'
sleep 1.2
echo "=== AFTER (t=1.6s, past the panic at 0.6s) ==="
tmux capture-pane -p -t "$session" | sed -n '1,20p'
sleep 1.5
echo "=== LATER (t=3.1s) ==="
tmux capture-pane -p -t "$session" | sed -n '1,20p'
tmux kill-session -t "$session" 2>/dev/null
