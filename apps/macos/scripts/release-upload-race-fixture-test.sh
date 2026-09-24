#!/usr/bin/env bash
# Fixture test for release-tcrbar.sh's upload_release_assets: the race with
# .github/workflows/appcast-guard.yml that stopped v1.1.17 (2026-09-24).
#
# Sources the real release-tcrbar.sh (main() never auto-runs on source) and
# calls the shipped function against a stub `gh` on PATH. The stub keeps the
# release's assets in a directory and can replay the race: on the first upload
# it attaches the guard's copy of the appcast (the committed, one-version-old
# file) and answers HTTP 422, exactly as the live run did, although the upload
# passed --clobber.
#
# Run directly: apps/macos/scripts/release-upload-race-fixture-test.sh
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
real_script="$here/release-tcrbar.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/release-upload-race.XXXXXX")"
trap 'rm -rf "$work"' EXIT

fail=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

mkdir -p "$work/bin"
cat >"$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
# Stub gh: `release upload` and `release download` against $STUB_STATE/assets.
state="$STUB_STATE"
case "$1 $2" in
  "release upload")
    n=$(( $(cat "$state/uploads" 2>/dev/null || echo 0) + 1 ))
    echo "$n" >"$state/uploads"
    shift 3
    files=()
    while [ $# -gt 0 ]; do
      case "$1" in
        --clobber) shift ;;
        --repo) shift 2 ;;
        *) files+=("$1"); shift ;;
      esac
    done
    if [ "$n" -le "${STUB_FAIL_UPLOADS:-0}" ]; then
      # The guard attaches its copy inside gh's --clobber gap.
      cp "$STUB_GUARD_APPCAST" "$state/assets/appcast.xml"
      echo "HTTP 422: Validation Failed: ReleaseAsset.name already exists" >&2
      exit 1
    fi
    # An upload that reports success and changes nothing: the case only a read-back catches.
    [ "${STUB_UPLOAD_NOOP:-0}" = 1 ] && exit 0
    for f in "${files[@]}"; do cp "$f" "$state/assets/$(basename "$f")"; done
    ;;
  "release download")
    shift 3
    dir="" pattern=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --dir) dir="$2"; shift 2 ;;
        --pattern) pattern="$2"; shift 2 ;;
        --repo) shift 2 ;;
        *) shift ;;
      esac
    done
    [ -f "$state/assets/$pattern" ] || { echo "no asset $pattern" >&2; exit 1; }
    cp "$state/assets/$pattern" "$dir/$pattern"
    ;;
  *)
    echo "stub gh: unexpected: $*" >&2
    exit 2
    ;;
esac
STUB
chmod +x "$work/bin/gh"

# Named appcast.xml like the real one: the upload names the asset after the file.
mkdir -p "$work/new"
printf '<rss><item>1.1.17</item><item>1.1.16</item></rss>\n' >"$work/new/appcast.xml"
printf '<rss><item>1.1.16</item></rss>\n' >"$work/guard-appcast.xml"
printf 'dmg bytes\n' >"$work/TcrBar-9.9.9.dmg"

# $1 case name; remaining: env assignments for the stub. Prints the function's exit
# status and leaves its combined output in $work/<case>.out and assets in $work/<case>.
run_case() {
  local name="$1"; shift
  local state="$work/$name"
  mkdir -p "$state/assets"
  [ "${SEED_GUARD:-0}" = 1 ] && cp "$work/guard-appcast.xml" "$state/assets/appcast.xml"
  set +e
  (
    export PATH="$work/bin:$PATH" STUB_STATE="$state" STUB_GUARD_APPCAST="$work/guard-appcast.xml"
    # Each remaining argument is a NAME=value for the stub.
    for assignment in "$@"; do
      declare -x "$assignment"
    done
    # shellcheck source=SCRIPTDIR/release-tcrbar.sh
    source "$real_script"
    upload_release_assets v9.9.9 acme/tcrbar "$work/TcrBar-9.9.9.dmg" "$work/new/appcast.xml"
  ) >"$work/$name.out" 2>&1
  local rc=$?
  set -e
  echo "$rc"
}

# 1. The v1.1.17 race: the first upload loses to the guard, the retry replaces its copy.
rc=$(run_case race STUB_FAIL_UPLOADS=1)
if [ "$rc" = 0 ] && cmp -s "$work/race/assets/appcast.xml" "$work/new/appcast.xml" \
  && [ "$(cat "$work/race/uploads")" = 2 ]; then
  pass "a 422 from the guard's race is retried once and the release ends up with this release's appcast"
else
  fail "race: rc=$rc uploads=$(cat "$work/race/uploads" 2>/dev/null) output: $(cat "$work/race.out")"
fi

# 2. No race: one upload, verified.
rc=$(run_case clean)
if [ "$rc" = 0 ] && [ "$(cat "$work/clean/uploads")" = 1 ] \
  && grep -q 'read back and compared' "$work/clean.out"; then
  pass "without a race it uploads once and still reads the appcast back"
else
  fail "clean: rc=$rc output: $(cat "$work/clean.out")"
fi

# 3. The upload reports success but the release still serves the guard's copy: must refuse.
rc=$(SEED_GUARD=1 run_case stale STUB_UPLOAD_NOOP=1)
if [ "$rc" != 0 ] && grep -q 'not the one this release wrote' "$work/stale.out"; then
  pass "a release left serving another appcast is refused, not reported as uploaded"
else
  fail "stale: rc=$rc output: $(cat "$work/stale.out")"
fi

# 4. Both attempts fail: a clean refusal, not an endless retry.
rc=$(run_case twice STUB_FAIL_UPLOADS=2)
if [ "$rc" != 0 ] && grep -q 'gh release upload failed' "$work/twice.out" \
  && [ "$(cat "$work/twice/uploads")" = 2 ]; then
  pass "two failed uploads stop with the upload error after exactly one retry"
else
  fail "twice: rc=$rc uploads=$(cat "$work/twice/uploads" 2>/dev/null) output: $(cat "$work/twice.out")"
fi

exit "$fail"
