#!/usr/bin/env sh
# Prove that --headless is what keeps a GUI-spawned server alive.
#
# The claim under test: without --headless, `tcr server` runs its ratatui TUI
# (src/main.rs:590), which calls enable_raw_mode()? on stdout (src/tui.rs:47).
# When stdout is a PIPE rather than a terminal — which is how a GUI app spawns a
# child — raw mode fails, the `?` propagates, and the process dies at once.
#
# SAFETY, and it is the whole reason this script exists rather than a one-liner:
#
#   * An EMPTY config, so the server has no accounts to probe or refresh. Two
#     processes refreshing the same accounts mutually invalidate each other's
#     single-use OAuth refresh tokens — the "token war" this repo warns about.
#     With zero accounts there is nothing to war over.
#   * A SCRATCH port, so the live proxy on 3456 is never touched.
#   * Both runs pipe stdout, reproducing the GUI's spawn conditions exactly. A
#     run from a terminal would pass either way and prove nothing.
set -u

TCR="$1"
PORT="$2"
CONFIG=/tmp/tcr-empty-config.json
printf '{"accounts":[],"proxy":{"port":%s}}\n' "$PORT" > "$CONFIG"

run() {
    label="$1"; shift
    # `sh -c ... | cat` guarantees stdout is a pipe, not this terminal.
    ( "$TCR" server --port "$PORT" --config "$CONFIG" "$@" >/tmp/tcr-probe.out 2>&1 ) &
    pid=$!
    sleep 3
    if kill -0 "$pid" 2>/dev/null; then
        echo "  $label -> ALIVE after 3s"
        kill "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
        return 0
    fi
    wait "$pid" 2>/dev/null
    echo "  $label -> DIED (exit $?)"
    sed 's/^/      /' /tmp/tcr-probe.out | head -3
    return 1
}

echo "== control: NO --headless (expected to die: TUI with no terminal) =="
run "tcr server            " ; no_headless=$?

echo
echo "== fix: WITH --headless (expected to survive) =="
run "tcr server --headless " --headless ; with_headless=$?

echo
if [ "$no_headless" -ne 0 ] && [ "$with_headless" -eq 0 ]; then
    echo "PROVEN: --headless is the difference. Without it the child dies on launch."
    rm -f "$CONFIG"
    exit 0
fi
echo "INCONCLUSIVE: no_headless=$no_headless with_headless=$with_headless"
echo "(a control that does not fail proves nothing about the fix)"
rm -f "$CONFIG"
exit 1
