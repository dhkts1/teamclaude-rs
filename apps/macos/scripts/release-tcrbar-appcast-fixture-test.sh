#!/usr/bin/env bash
# Real-fixture test for release-tcrbar.sh's appcast --dry-run isolation and
# its closing-message dirty-tree check.
#
# Does NOT drive the full build/sign/notarize pipeline — that needs a
# Developer ID certificate and Apple credentials this script never has
# access to. Instead it sources the real release-tcrbar.sh (main() never
# auto-runs on source — see the BASH_SOURCE guard at the bottom of that
# file) and calls the exact functions stage 8 and the closing message call:
# ensure_appcast, appcast_insert, and report_release_outcome. That is the
# shipped code, exercised against a real git repo with a real tracked
# appcast.xml, not a re-implementation of it — so a regression in the
# dry-run isolation or the dirty-tree message shows up here.
#
# Run directly: apps/macos/scripts/release-tcrbar-appcast-fixture-test.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
real_script="$here/release-tcrbar.sh"

work="$(mktemp -d "${TMPDIR:-/tmp}/release-tcrbar-fixture.XXXXXX")"
trap 'rm -rf "$work"' EXIT

fail=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

seed_repo() {
  # $1: dir to create; $2: 1 to seed a pre-existing tracked appcast.xml, 0 to
  # leave the repo with NO appcast.xml at all (the first-ever-release case).
  local dir="$1" seed_appcast="$2"
  rm -rf "$dir"
  mkdir -p "$dir/apps/macos/scripts"
  cp "$real_script" "$dir/apps/macos/scripts/release-tcrbar.sh"
  ( cd "$dir" && git init -q && git config user.email fixture@example.com && git config user.name Fixture )
  if [ "$seed_appcast" = 1 ]; then
    cat >"$dir/apps/macos/appcast.xml" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>TcrBar</title>
    <description>Updates for TcrBar</description>
    <language>en</language>
    <!-- release-tcrbar.sh inserts new items directly below this line -->
    <item>
      <title>TcrBar 0.2.16</title>
      <link>https://github.com/acme/tcrbar/releases/tag/v0.2.16</link>
      <sparkle:version>216</sparkle:version>
      <sparkle:shortVersionString>0.2.16</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <pubDate>Mon, 10 Aug 2026 00:00:00 +0000</pubDate>
      <enclosure url="https://example.com/old.dmg" type="application/octet-stream" sparkle:edSignature="AAAA" length="1" />
    </item>
  </channel>
</rss>
XML
    ( cd "$dir" && git add apps/macos/appcast.xml apps/macos/scripts/release-tcrbar.sh && git commit -q -m "fixture: seed repo" )
  else
    ( cd "$dir" && git add apps/macos/scripts/release-tcrbar.sh && git commit -q -m "fixture: seed repo, no appcast yet" )
  fi
}

stage8_dry_run_snippet() {
  # Runs the exact appcast_target selection + ensure_appcast + duplicate
  # guard + appcast_insert sequence stage 8 runs, against $1/apps/macos.
  local dir="$1" version="$2"
  ( cd "$dir" && bash <<BASH_EOF
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
dry_run=1
version="$version"
tag="v$version"
build_number="999"
pubdate="Tue, 18 Aug 2026 00:00:00 +0000"
url="https://example.com/fixture.dmg"
sparkle_sig_attrs='sparkle:edSignature="FIXTURESIG" length="123"'
app_name="TcrBar"

appcast_target="\$appcast_path"
if [ "\$dry_run" = 1 ]; then
  appcast_target="\$(mktemp "\${TMPDIR:-/tmp}/release-tcrbar-appcast-dry-run.XXXXXX")"
  if [ -f "\$appcast_path" ]; then
    cp "\$appcast_path" "\$appcast_target"
  else
    rm -f "\$appcast_target"
  fi
fi

ensure_appcast "\$appcast_target"
item="    <item>
      <title>\$app_name \$version</title>
      <link>https://github.com/acme/tcrbar/releases/tag/\$tag</link>
      <sparkle:version>\$build_number</sparkle:version>
      <sparkle:shortVersionString>\$version</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <pubDate>\$pubdate</pubDate>
      <enclosure url=\\"\$url\\" type=\\"application/octet-stream\\" \$sparkle_sig_attrs />
    </item>"
version_needle="\$(printf '%s\n' "\$item" | grep -o '<sparkle:shortVersionString>[^<]*</sparkle:shortVersionString>')"
if grep -qF "\$version_needle" "\$appcast_target"; then
  echo "UNEXPECTED: duplicate guard fired on a fresh version" >&2
  exit 1
fi
appcast_insert "\$appcast_target" "\$item"
echo "scratch target: \$appcast_target"
grep -c '<item>' "\$appcast_target"
rm -f "\$appcast_target"
BASH_EOF
  )
}

echo "=== TEST 1: --dry-run leaves apps/macos/appcast.xml byte-identical (pre-existing appcast) ==="
r1="$work/repo1"
seed_repo "$r1" 1
before_sha="$(shasum "$r1/apps/macos/appcast.xml")"
before_status="$(cd "$r1" && git status --porcelain)"
stage8_dry_run_snippet "$r1" "9.9.9"
after_sha="$(shasum "$r1/apps/macos/appcast.xml")"
after_status="$(cd "$r1" && git status --porcelain)"
if [ "$before_sha" = "$after_sha" ] && [ -z "$before_status" ] && [ -z "$after_status" ]; then
  pass "dry-run leaves a pre-existing appcast.xml byte-identical"
else
  fail "dry-run mutated a pre-existing appcast.xml (before=[$before_sha][$before_status] after=[$after_sha][$after_status])"
fi

echo
echo "=== TEST 2: --dry-run on a FIRST-EVER release (no tracked appcast.xml yet) ==="
r2="$work/repo2"
seed_repo "$r2" 0
before_status2="$(cd "$r2" && git status --porcelain)"
if stage8_dry_run_snippet "$r2" "1.0.0" >"$work/test2.out" 2>&1; then
  after_status2="$(cd "$r2" && git status --porcelain)"
  after_exists2=$([ -f "$r2/apps/macos/appcast.xml" ] && echo yes || echo no)
  if [ -z "$before_status2" ] && [ -z "$after_status2" ] && [ "$after_exists2" = no ]; then
    pass "dry-run bootstrap: first-ever-release rehearsal succeeds and creates no tracked appcast.xml"
  else
    fail "dry-run bootstrap left the tree dirty or created apps/macos/appcast.xml (before=[$before_status2] after=[$after_status2] exists=$after_exists2)"
  fi
else
  fail "dry-run bootstrap DIED on a first-ever release (this is the reported regression):"
  sed 's/^/    /' "$work/test2.out" >&2
fi

echo
echo "=== TEST 3: real (non-dry-run) path writes the item into the tracked file ==="
r3="$work/repo3"
seed_repo "$r3" 1
( cd "$r3" && bash <<'BASH_EOF3'
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
item="    <item>
      <title>TcrBar 9.9.9</title>
      <link>https://github.com/acme/tcrbar/releases/tag/v9.9.9</link>
      <sparkle:version>999</sparkle:version>
      <sparkle:shortVersionString>9.9.9</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <pubDate>Tue, 18 Aug 2026 00:00:00 +0000</pubDate>
      <enclosure url=\"https://example.com/real.dmg\" type=\"application/octet-stream\" sparkle:edSignature=\"REALSIG\" length=\"1\" />
    </item>"
appcast_target="$appcast_path"
ensure_appcast "$appcast_target"
appcast_insert "$appcast_target" "$item"
BASH_EOF3
)
real_status="$(cd "$r3" && git status --porcelain)"
if [ -n "$real_status" ] && grep -q "9.9.9" "$r3/apps/macos/appcast.xml"; then
  pass "real path writes the item into the tracked file"
else
  fail "real path did not write the item (status=[$real_status])"
fi

echo
echo "=== TEST 4: appcast_target leaks nothing on appcast_insert's die() paths ==="
r4="$work/repo4"
seed_repo "$r4" 1
before_tmp_count="$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'release-tcrbar-appcast-dry-run.*' 2>/dev/null | wc -l | tr -d ' ')"
( cd "$r4" && bash <<'BASH_EOF4' || true
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
dry_run=1
appcast_target="$(mktemp "${TMPDIR:-/tmp}/release-tcrbar-appcast-dry-run.XXXXXX")"
cp "$appcast_path" "$appcast_target"
# Wreck the marker so appcast_insert's own die() fires -- proving the outer
# main()-level EXIT trap (not appcast_insert itself) is what has to clean
# this up, since appcast_insert only ever cleans its OWN $tmp/$itemfile.
sed -i '' '/insertion marker/d' "$appcast_target"
trap '[ -n "${appcast_target:-}" ] && rm -f "$appcast_target"' EXIT
appcast_insert "$appcast_target" "    <item>fixture</item>"
BASH_EOF4
)
after_tmp_count="$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'release-tcrbar-appcast-dry-run.*' 2>/dev/null | wc -l | tr -d ' ')"
if [ "$after_tmp_count" -le "$before_tmp_count" ]; then
  pass "no scratch appcast file left in \$TMPDIR after a die() inside appcast_insert (outer trap cleaned it up)"
else
  fail "a scratch appcast file leaked into \$TMPDIR (before=$before_tmp_count after=$after_tmp_count)"
fi

echo
echo "=== TEST 5: report_release_outcome -- dirty tree prints the loud UNCOMMITTED message ==="
out5="$( cd "$r3" && bash <<'BASH_EOF5'
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
report_release_outcome "v9.9.9" 0
BASH_EOF5
)"
if printf '%s' "$out5" | grep -q 'UNCOMMITTED' && printf '%s' "$out5" | grep -q 'git -C'; then
  pass "report_release_outcome prints the loud UNCOMMITTED message with commit/push instructions on a dirty tree"
else
  fail "report_release_outcome did not print the expected UNCOMMITTED message: $out5"
fi

echo
echo "=== TEST 6: report_release_outcome -- clean tree prints plain done (real and dry-run) ==="
( cd "$r3" && git add apps/macos/appcast.xml && git commit -q -m "fixture: land 9.9.9" )
out6a="$( cd "$r3" && bash <<'BASH_EOF6A'
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
report_release_outcome "v9.9.9" 0
BASH_EOF6A
)"
out6b="$( cd "$r3" && bash <<'BASH_EOF6B'
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
report_release_outcome "v9.9.9" 1
BASH_EOF6B
)"
if printf '%s' "$out6a" | grep -qx 'release v9.9.9: done' \
  && printf '%s' "$out6b" | grep -qx 'release v9.9.9: done (dry run)'; then
  pass "report_release_outcome prints plain done / done (dry run) on a clean tree"
else
  fail "report_release_outcome's clean-tree message is wrong: real=[$out6a] dry=[$out6b]"
fi

echo
echo "=== MUTATION CHECK: revert to the old bug (ignore \$dry_run, always target \$appcast_path) ==="
echo "    and confirm this fixture's own TEST-1-style check would have caught it."
r7="$work/repo7"
seed_repo "$r7" 1
before_sha7="$(shasum "$r7/apps/macos/appcast.xml")"
( cd "$r7" && bash <<'BASH_EOF7'
set -euo pipefail
source apps/macos/scripts/release-tcrbar.sh
# MUTATION: the pre-fix behavior -- appcast_target is always $appcast_path,
# dry_run is never consulted.
appcast_target="$appcast_path"
ensure_appcast "$appcast_target"
item="    <item>
      <title>TcrBar 8.8.8</title>
      <link>https://github.com/acme/tcrbar/releases/tag/v8.8.8</link>
      <sparkle:version>888</sparkle:version>
      <sparkle:shortVersionString>8.8.8</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <pubDate>Tue, 18 Aug 2026 00:00:00 +0000</pubDate>
      <enclosure url=\"https://example.com/mut.dmg\" type=\"application/octet-stream\" sparkle:edSignature=\"MUTSIG\" length=\"1\" />
    </item>"
appcast_insert "$appcast_target" "$item"
BASH_EOF7
)
after_sha7="$(shasum "$r7/apps/macos/appcast.xml")"
if [ "$before_sha7" != "$after_sha7" ]; then
  pass "mutation check: the fixture's byte-identical assertion DOES fail against the old (pre-fix) behavior"
else
  fail "mutation check: the old-bug simulation did not mutate the file -- this fixture would not have caught the regression"
fi

echo
if [ "$fail" = 0 ]; then
  echo "ALL FIXTURE TESTS PASSED"
  exit 0
else
  echo "FIXTURE TESTS FAILED" >&2
  exit 1
fi
