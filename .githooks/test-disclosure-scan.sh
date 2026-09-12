#!/usr/bin/env bash
# .githooks/test-disclosure-scan.sh — .githooks/lib/disclosure-scan.sh and the
# push-time gate that reads it.
#
# Every case here is a fixture the gate MUST refuse, plus the controls that prove
# it can still pass. A gate whose fixtures only ever pass is a gate nobody has
# watched fail, and this one exists because a reminder that can be read past is
# not a gate (.githooks/pre-commit, 2026-08-08).
#
# Fixture commits name their paths explicitly rather than staging the whole tree.
# The scratch repo is the test's own and a blind stage would be harmless here, but
# the repo-wide rule against it is worth not teaching anyone to read past.
#
# Run: .githooks/test-disclosure-scan.sh      (exit 0 = every assertion held)
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$REPO/.githooks/lib/disclosure-scan.sh"
PREPUSH="$REPO/.githooks/pre-push"
FAILED=0
ok()   { echo "  ok: $1"; }
fail() { echo "FAIL: $1"; FAILED=1; }

[ -f "$LIB" ] || { fail "library missing: $LIB"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A scratch repo with its own denylist: the real one is local-only and its
# contents must never reach a test log.
SCRATCH="$WORK/repo"
mkdir -p "$SCRATCH/.githooks/lib"
cp "$LIB" "$SCRATCH/.githooks/lib/disclosure-scan.sh"
cp "$PREPUSH" "$SCRATCH/.githooks/pre-push"
printf 'acmecorp-private\n' > "$SCRATCH/.githooks/private-names"
git -C "$SCRATCH" init -q
git -C "$SCRATCH" config user.email alice@example.com
git -C "$SCRATCH" config user.name Alice
echo "start" > "$SCRATCH/file.txt"
git -C "$SCRATCH" add file.txt .githooks
git -C "$SCRATCH" commit -q --no-verify -m "init"

# A remote from the start, because the gate's range is "what no remote ref already
# carries". A repo with no remote at all has nothing to subtract, so every case
# below would scan the whole history and trip over an earlier case's fixtures.
# Reality always has a remote; the fixture should too.
SCRATCH_REMOTE="$WORK/remote.git"
git init -q --bare "$SCRATCH_REMOTE"
git -C "$SCRATCH" remote add origin "$SCRATCH_REMOTE"
git -C "$SCRATCH" push -q origin HEAD:refs/heads/main
git -C "$SCRATCH" fetch -q origin

commit_file() {  # <message>
  git -C "$SCRATCH" commit -q --no-verify -m "$1" file.txt
}
parent() { git -C "$SCRATCH" rev-parse HEAD~1; }
push_input() { printf 'refs/heads/main %s refs/heads/main %s\n' "$1" "$2"; }

# ── The text scan ────────────────────────────────────────────────────────────
cd "$SCRATCH" || exit 1
# shellcheck source=/dev/null
. "$SCRATCH/.githooks/lib/disclosure-scan.sh"

scan() { printf '%s\n' "$1" | tcr_disclosure_scan_text "the text" >/dev/null 2>&1; }

scan "a line naming AcmeCorp-Private in mixed case" \
  && fail "a private name passed the scan (case-insensitive match is broken)" \
  || ok "a private name is refused, case-insensitively"
# A gate's own test file is the one place where the thing the gate refuses must
# not appear literally: written out, these two fixtures ARE the disclosure, and
# this file could not be committed past the gate it tests (measured — the first
# version of it was refused, correctly). So the fixtures are ASSEMBLED here and
# exist only at runtime. Do not "tidy" them back into one string.
HOME_FIXTURE="/Users/""real""person/git/thing/file.rs"
MAIL_FIXTURE="some""one""@real""company.io"
scan "see $HOME_FIXTURE" \
  && fail "an absolute home path passed the scan" \
  || ok "an absolute home path is refused"
scan "mail me at $MAIL_FIXTURE" \
  && fail "a real-looking email passed the scan" \
  || ok "a real-looking email is refused"
scan "alice@example.com and /Users/alice/x and a plain sentence" \
  && ok "the synthetic allowlist still passes (example.com, /Users/alice)" \
  || fail "the allowlist broke: a fixture-safe line was refused"
scan "" \
  && ok "empty text is clean" \
  || fail "empty text was reported as a disclosure"

# ── The push gate: a commit MESSAGE, which no diff scan can read ─────────────
echo "a harmless line" >> "$SCRATCH/file.txt"
commit_file "chore: mention AcmeCorp-Private in the subject"
out="$(push_input "$(git -C "$SCRATCH" rev-parse HEAD)" "$(parent)" | "$SCRATCH/.githooks/pre-push" 2>&1)"
rc=$?
if [ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -q 'the message of'; then
  ok "a private name in a COMMIT MESSAGE is refused at push time"
else
  fail "a private name in a commit message reached the push unexamined (rc=$rc)"
fi

# ── The push gate: an added LINE ────────────────────────────────────────────
echo "config for AcmeCorp-Private" >> "$SCRATCH/file.txt"
commit_file "chore: a clean subject"
out="$(push_input "$(git -C "$SCRATCH" rev-parse HEAD)" "$(parent)" | "$SCRATCH/.githooks/pre-push" 2>&1)"
rc=$?
if [ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -q 'the changes in'; then
  ok "a private name in an added LINE is refused at push time"
else
  fail "a private name in an added line reached the push unexamined (rc=$rc)"
fi

# ── The control: a clean commit must PASS ───────────────────────────────────
# From a base that is actually on the remote. The two refusal fixtures above are
# still on this branch and are NOT pushed, so a range measured from here would
# legitimately include them: the gate would be right to refuse, and the control
# would be testing the wrong thing. Throw them away first.
git -C "$SCRATCH" reset -q --hard origin/main
echo "an entirely ordinary line" >> "$SCRATCH/file.txt"
commit_file "chore: an ordinary change"
GOOD_SHA="$(git -C "$SCRATCH" rev-parse HEAD)"
out="$(push_input "$GOOD_SHA" "$(parent)" | "$SCRATCH/.githooks/pre-push" 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  ok "a clean commit passes the push gate"
else
  fail "the push gate refused a clean commit — it would be turned off (out: $out)"
fi

# ── A deletion pushes nothing ───────────────────────────────────────────────
out="$(printf 'refs/heads/gone 0000000000000000000000000000000000000000 refs/heads/gone %s\n' "$GOOD_SHA" | "$SCRATCH/.githooks/pre-push" 2>&1)"
rc=$?
[ "$rc" -eq 0 ] && ok "a branch deletion is not scanned" || fail "a deletion was treated as content"

# ── The denylist file itself is not a disclosure ─────────────────────────────
printf 'another-private-name\n' >> "$SCRATCH/.githooks/private-names"
git -C "$SCRATCH" add .githooks/private-names
staged="$(cd "$SCRATCH" && tcr_disclosure_staged_added)"
if printf '%s\n' "$staged" | grep -q 'another-private-name'; then
  fail "the denylist's own lines are scanned — adding a name would block the commit"
else
  ok "the denylist file is excluded from the scan"
fi
git -C "$SCRATCH" reset -q

# ── An already-public commit arriving via a merge must NOT be rescanned ──────
# The regression that produced this case: merging `main` into a feature branch to
# bring it up to date makes main's commits new to THAT ref, so a plain
# `remote..local` range handed them to the scan even though every one of them was
# already pushed and world-readable. The hook refused its own author's push on
# 2026-09-12 over two addresses in a dependabot commit message already on `main`.
# A gate that refuses the ordinary act of updating a branch gets turned off.
# A commit on "main" whose MESSAGE would trip the scan, pushed and therefore public.
# The two sides touch DIFFERENT files on purpose: appending to one file from both
# makes the merge below conflict, and a conflicted merge leaves HEAD where it was,
# so the range under test is trivially clean and the case passes for the wrong
# reason. That is not hypothetical either — the first version of this case did
# exactly that and a mutant survived it.
echo "upstream work" > "$SCRATCH/upstream.txt"
git -C "$SCRATCH" add upstream.txt
git -C "$SCRATCH" commit -q --no-verify -m "chore: a message naming AcmeCorp-Private, already public"
PUBLIC_SHA="$(git -C "$SCRATCH" rev-parse HEAD)"
git -C "$SCRATCH" push -q origin HEAD:refs/heads/main
git -C "$SCRATCH" fetch -q origin

# A feature branch that merges it in, then pushes. Nothing of ITS own discloses.
git -C "$SCRATCH" checkout -q -b feature "$PUBLIC_SHA~1"
echo "my own clean work" > "$SCRATCH/mine.txt"
git -C "$SCRATCH" add mine.txt
git -C "$SCRATCH" commit -q --no-verify -m "chore: an ordinary change of my own"
FEATURE_BASE="$(git -C "$SCRATCH" rev-parse HEAD)"
git -C "$SCRATCH" merge -q --no-ff --no-verify -m "merge: origin/main" "$PUBLIC_SHA"
if [ "$(git -C "$SCRATCH" rev-parse HEAD)" = "$FEATURE_BASE" ]; then
  fail "the fixture's merge did not move HEAD, so this case would pass for the wrong reason"
fi
if ! git -C "$SCRATCH" merge-base --is-ancestor "$PUBLIC_SHA" HEAD; then
  fail "the fixture's merge did not bring the public commit in; the case proves nothing"
fi
out="$(printf 'refs/heads/feature %s refs/heads/feature %s\n' "$(git -C "$SCRATCH" rev-parse HEAD)" "$FEATURE_BASE" | "$SCRATCH/.githooks/pre-push" 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  ok "a commit already on a remote is not rescanned when merged in"
else
  fail "merging public history into a branch was refused (rc=$rc): $(printf '%s' "$out" | head -3)"
fi
git -C "$SCRATCH" checkout -q -

# ── The gate cannot silently degrade ────────────────────────────────────────
rm "$SCRATCH/.githooks/lib/disclosure-scan.sh"
out="$(push_input "$GOOD_SHA" "$(parent)" | "$SCRATCH/.githooks/pre-push" 2>&1)"
rc=$?
if [ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -q 'disclosure-scan.sh is missing'; then
  ok "a missing library refuses the push instead of reporting safety"
else
  fail "a missing library let the push through (rc=$rc)"
fi

# ── One spelling: the hooks must all read the library ──────────────────────
cd "$REPO" || exit 1
for hook in pre-commit pre-push; do
  if grep -Eq '^[[:space:]]*(\.|source) [^#]*lib/disclosure-scan\.sh' ".githooks/$hook"; then
    ok ".githooks/$hook sources the shared library"
  else
    fail ".githooks/$hook does not source the shared library"
  fi
done
if grep -q 'exec .*pre-commit' .githooks/pre-merge-commit 2>/dev/null; then
  ok ".githooks/pre-merge-commit runs pre-commit, so a merge is scanned"
else
  fail ".githooks/pre-merge-commit does not run pre-commit — merges stay blind"
fi
if grep -q "grep -E -m3 '/(Users|home)" .githooks/pre-commit; then
  fail "pre-commit carries its own copy of the home-path check — two spellings again"
else
  ok "pre-commit carries no second copy of the checks"
fi

if [ "$FAILED" -ne 0 ]; then
  echo "test-disclosure-scan: FAILED"
  exit 1
fi
echo "test-disclosure-scan: all assertions passed"
exit 0
