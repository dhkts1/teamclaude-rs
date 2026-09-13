#!/usr/bin/env bash
# Break the peer verification three ways and confirm each break fails the test
# that exists to catch it.
#
# A green suite proves nothing about what it guards. Each break below names the
# test that MUST go red and the tests that must stay green; a break that leaves
# everything green is a hole, and the script says so rather than exiting 0.
#
# The file is restored by a trap on EXIT, so an interrupt mid-run does not leave
# a sabotaged peer.rs in the tree.
#
# `mktemp -t NAME.XXXXXX`, with the X's: GNU mktemp rejects a -t template
# without them, while BSD mktemp accepts the bare name. The bare form looks
# correct on macOS and fails wherever GNU coreutils comes first on PATH.
#
# Plain `cargo` on purpose. If your machine wraps rustc with sccache and that
# server has wedged, every build here hangs with no output and no rustc
# process; clear the wrapper for this run
# (`RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= scripts/watch-peer-tests-fail.sh`)
# rather than teaching this script about one machine's cache.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1
TARGET="crates/tcr-fdpass/src/peer.rs"
BACKUP="$(mktemp -t peer-rs-backup.XXXXXX)"
cp "$TARGET" "$BACKUP"

restore() {
  cp "$BACKUP" "$TARGET"
  rm -f "$BACKUP"
  # mtime must move or cargo reuses the sabotaged build. `cp` gives it a fresh
  # one, but say so out loud: a stale binary re-running is how a restored file
  # still fails.
  touch "$TARGET"
}
trap restore EXIT

fails=0

# run_break <label> <must-fail-test> <sed-program>
run_break() {
  local label="$1" must_fail="$2" prog="$3" log out
  log="$(mktemp -t peer-break.XXXXXX)"
  cp "$BACKUP" "$TARGET"
  python3 - "$TARGET" "$prog" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
old, new = sys.argv[2].split("::=")
s = p.read_text()
if old not in s:
    sys.exit(f"BREAK NOT APPLIED: {old!r} not found in {p}")
p.write_text(s.replace(old, new, 1))
PY
  if [ $? -ne 0 ]; then
    echo "BREAK-${label}: BLOCKED — the sabotage did not apply; the test below would pass for the wrong reason."
    fails=$((fails + 1))
    return
  fi
  touch "$TARGET"
  cargo test -p tcr-fdpass >"$log" 2>&1
  out="$(grep -E '^test peer::tests::' "$log")"

  if grep -qE "^test ${must_fail} \.\.\. FAILED" <<<"$out"; then
    echo "BREAK-${label}: OK — ${must_fail} went red."
  else
    echo "BREAK-${label}: HOLE — ${must_fail} did NOT fail. Full peer results:"
    sed 's/^/    /' <<<"$out"
    fails=$((fails + 1))
  fi
  rm -f "$log"
}

echo "=== control: unbroken tree must be green ==="
cp "$BACKUP" "$TARGET"; touch "$TARGET"
# Redirected to a file, never piped. `... | grep -q` exits on the first match,
# SIGPIPEs cargo, and `pipefail` then reports the whole pipeline as failed: a
# green suite reads as red and every break below becomes meaningless.
control_log="$(mktemp -t peer-control.XXXXXX)"
cargo test -p tcr-fdpass >"$control_log" 2>&1
if grep -q '^test result: ok' "$control_log"; then
  echo "CONTROL: OK — the unbroken tree passes, so a red below is the break."
  rm -f "$control_log"
else
  echo "CONTROL: BLOCKED — the tree is already red; nothing below means anything."
  tail -20 "$control_log" | sed 's/^/    /'
  rm -f "$control_log"
  exit 1
fi

# 1. Ignore the verdict of check_validity. Only the refusal test guards this.
run_break "ignore-verdict" \
  "peer::tests::an_adhoc_signed_peer_is_refused_by_a_developer_id_requirement" \
  'code.check_validity(Flags::NONE, &req).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer does not satisfy the code requirement: {e}"),
        )
    })::=code.check_validity(Flags::NONE, &req).ok();
    Ok(())'

# 2. Zero the audit token. The positive control must catch this, not just the
#    refusal test: a peer that cannot be resolved at all must not read as a peer
#    that was resolved and refused.
run_break "zero-token" \
  "peer::tests::a_peer_audit_token_resolves_to_a_code_object" \
  'let bytes = token_bytes(token);::=let bytes = [0u8; AUDIT_TOKEN_BYTES];'

# 3. Byte-swap the token reinterpretation. This is the break nothing above
#    code_for_token can see, which is why the byte-order test exists.
run_break "swap-token-bytes" \
  "peer::tests::the_token_bytes_are_the_token_in_order" \
  'unsafe { std::mem::transmute::<AuditToken, [u8; AUDIT_TOKEN_BYTES]>(*token) }::=let mut b = unsafe { std::mem::transmute::<AuditToken, [u8; AUDIT_TOKEN_BYTES]>(*token) };
    b.reverse();
    b'

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL BREAKS CAUGHT."
else
  echo "${fails} BREAK(S) NOT CAUGHT — see above."
fi
exit "$fails"
