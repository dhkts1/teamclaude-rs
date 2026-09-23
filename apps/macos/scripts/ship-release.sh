#!/usr/bin/env bash
# ship-release.sh — cut, land and publish a TcrBar release in one command.
#
#   apps/macos/scripts/ship-release.sh <X.Y.Z> [--next <X.Y.Z>] [--install] [--dry-run]
#
# Run from the Mac that holds the signing keys, in a checkout on `main`.
#
# WHY THIS EXISTS. A release was five commands run by hand in a fixed order, and
# only one of them has a reason to be separate. Signing stays on this Mac on
# purpose (docs/RELEASING.md § "Why there are no signing secrets in GitHub"), so
# this script runs locally too. What it removes is the hand-sequencing of the
# rest, which has broken releases before: an appcast entry left uncommitted for
# three days blocked the next release (v0.2.45 -> v0.2.46), and a feed nobody
# published leaves a release that no install is ever offered.
#
# THE STEPS, each one refusing rather than guessing:
#
#   1. Refuse unless the checkout is on `main`, has no tracked changes, equals
#      `origin/main`, Cargo.toml says X.Y.Z, the tag vX.Y.Z exists nowhere yet,
#      and the `ci` and `macos` checks PASSED on that exact commit. The last one
#      is the gate a hand release skips most easily: a PR merged while `ci` was
#      still running is otherwise released unverified.
#   2. Tag vX.Y.Z as a LIGHTWEIGHT tag, like every shipped tag (`tag.gpgsign`
#      would otherwise demand a message and fail), push that one ref by explicit
#      refspec, and read it back from the remote. Creation and push are separate
#      commands on purpose: chained, a failed `git tag` once let a push go out
#      that published a stale tag.
#   3. `release-local.sh vX.Y.Z`: build, sign, notarize, DMG, appcast entry,
#      upload. It needs TCRBAR_OP_ITEM (see that script); this one checks it is
#      set BEFORE the tag is pushed, not after.
#   4. With --install only: `install.sh` from the tagged commit. It RESTARTS the
#      local proxy (quitting TcrBar stops the proxy it supervises), which costs
#      every live session its warm prompt cache, so it is never the default.
#   5. One PR carrying the appcast entry AND Cargo.toml + Cargo.lock moved to the
#      next version. One PR, not two: an appcast-only PR skips `ci` and `macos`,
#      and a bump in the same commit is what the pre-commit version gate wants
#      after a release (scripts/check-release-version.sh).
#   6. Wait for the four required checks (ci, audit, macos, plan) BY NAME on the
#      PR's head commit, then squash-merge. By name, because a workflow that has
#      not been queued yet has no check run, and "every check I can see passed"
#      is then true of a PR that was never tested.
#   7. Back on `main`: `publish-appcast-feed.sh`, then read the feed back through
#      the GitHub API. Not the raw URL, whose CDN lags a push by minutes and reads
#      as a failed publish.
#
# --dry-run runs step 1 and prints the plan; it changes nothing.
#
# NOTHING HERE PRINTS A SECRET OR NAMES A PERSON, same rule as release-local.sh:
# this file is world-readable.
#
# Environment:
#   TCRBAR_OP_ITEM   REQUIRED (by release-local.sh). op://<vault>/<item>.
#   RELEASE_REPO     owner/name (default dhkts1/teamclaude-rs, as elsewhere).
#   SHIP_CHECK_WAIT_MINUTES  how long to wait for checks (default 60).
set -uo pipefail

die() { printf 'ship-release: error: %s\n' "$*" >&2; exit 1; }
say() { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }

repo="${RELEASE_REPO:-dhkts1/teamclaude-rs}"
wait_minutes="${SHIP_CHECK_WAIT_MINUTES:-60}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$script_dir" rev-parse --show-toplevel)" || die "not inside a git checkout"

version="" next="" install=0 dry_run=0
while [ $# -gt 0 ]; do
  case "$1" in
    --next) next="${2:-}"; shift 2 ;;
    --install) install=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) sed -n '2,5p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) die "unknown flag: $1" ;;
    *) [ -z "$version" ] || die "one version only (got '$version' and '$1')"; version="$1"; shift ;;
  esac
done

semver='^[0-9]+\.[0-9]+\.[0-9]+$'
[[ "$version" =~ $semver ]] || die "usage: ship-release.sh <X.Y.Z> [--next X.Y.Z] [--install] [--dry-run]"
if [ -z "$next" ]; then
  IFS=. read -r major minor patch <<<"$version"
  next="$major.$minor.$((patch + 1))"
fi
[[ "$next" =~ $semver ]] || die "--next must be X.Y.Z (got '$next')"
[ "$next" != "$version" ] || die "--next must differ from the version being released"
tag="v$version"

# The latest run of each named check on <sha>, as "<name> <status> <conclusion>".
# A check with no run yet prints "<name> missing -".
check_states() {
  local sha="$1"; shift
  local runs name
  runs="$(gh api --paginate "repos/$repo/commits/$sha/check-runs?per_page=100" \
            --jq '.check_runs[] | "\(.id)\t\(.name)\t\(.status)\t\(.conclusion // "-")"')" \
    || return 2
  for name in "$@"; do
    printf '%s\n' "$runs" \
      | awk -F'\t' -v n="$name" '$2 == n { if ($1 + 0 > best) { best = $1 + 0; line = $2 " " $3 " " $4 } }
                                  END { if (line == "") print n " missing -"; else print line }'
  done
}

# Wait until every named check on <sha> has completed. Exit 0 when all passed
# (success, or skipped/neutral, which GitHub's branch protection also accepts),
# 1 when any finished otherwise, 2 on timeout or an unreadable API.
wait_checks() {
  local sha="$1"; shift
  local deadline=$(( $(date +%s) + wait_minutes * 60 ))
  local states pending failed
  while :; do
    states="$(check_states "$sha" "$@")" || { note "could not read check runs for $sha"; return 2; }
    pending="$(printf '%s\n' "$states" | awk '$2 != "completed"')"
    failed="$(printf '%s\n' "$states" | awk '$2 == "completed" && $3 != "success" && $3 != "skipped" && $3 != "neutral"')"
    if [ -n "$failed" ]; then
      printf '%s\n' "$states" | sed 's/^/    /'
      return 1
    fi
    if [ -z "$pending" ]; then
      printf '%s\n' "$states" | sed 's/^/    /'
      return 0
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      note "gave up after ${wait_minutes}m with checks still pending:"
      printf '%s\n' "$pending" | sed 's/^/      /'
      return 2
    fi
    sleep 30
  done
}

cargo_version() { awk -F'"' '/^version = "/ { print $2; exit }' "$root/Cargo.toml"; }

# ---------------------------------------------------------------------------
say "step 1/7  preconditions for $tag (next: $next)"
command -v gh >/dev/null || die "gh is not installed"
gh auth status >/dev/null 2>&1 || die "gh is not signed in"
[ -n "${TCRBAR_OP_ITEM:-}" ] || die "TCRBAR_OP_ITEM is not set; release-local.sh needs it (see its header)"
[ "$(git -C "$root" branch --show-current)" = main ] || die "the checkout is not on main"
[ -z "$(git -C "$root" status --porcelain --untracked-files=no)" ] \
  || die "the checkout has tracked changes; land or stash them first"
git -C "$root" fetch -q origin || die "git fetch failed"
git -C "$root" merge -q --ff-only origin/main || die "main cannot fast-forward to origin/main"
head="$(git -C "$root" rev-parse HEAD)"
[ "$head" = "$(git -C "$root" rev-parse origin/main)" ] || die "main is not origin/main"
[ "$(cargo_version)" = "$version" ] || die "Cargo.toml says $(cargo_version), not $version"
git -C "$root" rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists locally"
git -C "$root" ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1 \
  && die "tag $tag already exists on origin"
note "main is $head, Cargo.toml is $version, $tag is new"
note "waiting for ci and macos on that commit…"
wait_checks "$head" ci macos
rc=$?
[ "$rc" -eq 0 ] || die "ci/macos did not pass on $head (see above); not releasing unverified code"

if [ "$dry_run" = 1 ]; then
  say "dry run: every precondition holds; nothing was changed"
  note "would tag $tag at $head, run release-local.sh $tag,$([ "$install" = 1 ] && printf ' install it,')"
  note "open 'chore: appcast entry for $version and bump to $next', merge it when green,"
  note "and publish the feed."
  exit 0
fi

# ---------------------------------------------------------------------------
say "step 2/7  tag $tag"
git -C "$root" -c tag.gpgsign=false tag "$tag" || die "git tag failed"
[ "$(git -C "$root" cat-file -t "$tag")" = commit ] || die "$tag is not a lightweight tag"
git -C "$root" push origin "refs/tags/$tag:refs/tags/$tag" || die "pushing $tag failed"
remote_sha="$(git -C "$root" ls-remote origin "refs/tags/$tag" | awk '{ print $1 }')"
[ "$remote_sha" = "$head" ] || die "origin's $tag is '$remote_sha', not $head"
note "origin has $tag at $head"

# ---------------------------------------------------------------------------
say "step 3/7  release-local.sh $tag"
"$script_dir/release-local.sh" "$tag" || die "release-local.sh failed; the tag is pushed, see docs/RELEASING.md"
grep -q "<sparkle:shortVersionString>$version</sparkle:shortVersionString>" "$root/apps/macos/appcast.xml" \
  || die "the appcast has no $version entry after the release"

# ---------------------------------------------------------------------------
if [ "$install" = 1 ]; then
  say "step 4/7  install $tag here (restarts the local proxy)"
  bash "$script_dir/install.sh" || die "install.sh failed; the release itself is published"
else
  say "step 4/7  install: skipped (pass --install to put it live on this Mac)"
fi

# ---------------------------------------------------------------------------
branch="chore/appcast-$version-bump-$next"
title="chore: appcast entry for $version and bump to $next"
say "step 5/7  PR: $title"
git -C "$root" switch -q -c "$branch" || die "could not create $branch"
V="$version" N="$next" perl -0pi -e 's/^version = "\Q$ENV{V}\E"/version = "$ENV{N}"/m' "$root/Cargo.toml"
V="$version" N="$next" perl -0pi -e 's/(name = "teamclaude-rs"\nversion = ")\Q$ENV{V}\E"/$1$ENV{N}"/' "$root/Cargo.lock"
[ "$(cargo_version)" = "$next" ] || die "Cargo.toml was not moved to $next"
grep -A1 '^name = "teamclaude-rs"$' "$root/Cargo.lock" | grep -q "^version = \"$next\"$" \
  || die "Cargo.lock was not moved to $next"
cargo metadata --manifest-path "$root/Cargo.toml" --format-version 1 --locked --no-deps >/dev/null \
  || die "Cargo.lock does not match Cargo.toml after the bump"
git -C "$root" add apps/macos/appcast.xml Cargo.toml Cargo.lock
git -C "$root" commit -q -m "$title" \
  -m "release: the feed entry the release wrote, and the next unreleased version in Cargo.toml and Cargo.lock." \
  || die "commit failed"
git -C "$root" push -q -u origin "$branch" || die "push of $branch failed"
body="🍄 The appcast entry the $version release wrote, and \`Cargo.toml\` plus \`Cargo.lock\` moved to $next unreleased. Opened by \`apps/macos/scripts/ship-release.sh\`."
pr_url="$(gh pr create --repo "$repo" --base main --head "$branch" --title "$title" --body "$body")" \
  || die "gh pr create failed"
pr="${pr_url##*/}"
note "$pr_url"

# ---------------------------------------------------------------------------
say "step 6/7  wait for ci, audit, macos and plan on #$pr, then merge"
merged=0
for attempt in 1 2; do
  pr_head="$(gh pr view "$pr" --repo "$repo" --json headRefOid --jq .headRefOid)" || die "cannot read #$pr"
  wait_checks "$pr_head" ci audit macos plan
  rc=$?
  [ "$rc" -eq 0 ] || die "#$pr's required checks did not pass (see above); left open"
  if gh pr merge "$pr" --repo "$repo" --squash; then
    merged=1
    break
  fi
  # `main` is protected with strict=true: a PR that fell BEHIND must be updated
  # and re-checked before it can land. Once, then stop and say so.
  [ "$attempt" = 1 ] || break
  note "merge refused; updating the branch to main and waiting again"
  gh pr update-branch "$pr" --repo "$repo" || break
  sleep 10
done
[ "$merged" = 1 ] || die "#$pr did not merge; it is open for a human"
[ "$(gh pr view "$pr" --repo "$repo" --json state --jq .state)" = MERGED ] || die "#$pr does not read MERGED"

# ---------------------------------------------------------------------------
say "step 7/7  publish the feed from main"
git -C "$root" switch -q main || die "could not switch back to main"
git -C "$root" fetch -q origin || die "git fetch failed"
git -C "$root" merge -q --ff-only origin/main || die "main cannot fast-forward to origin/main"
git -C "$root" branch -q -D "$branch" 2>/dev/null || true
grep -q "<sparkle:shortVersionString>$version</sparkle:shortVersionString>" "$root/apps/macos/appcast.xml" \
  || die "main's appcast has no $version entry after the merge"
"$script_dir/publish-appcast-feed.sh" || die "publish-appcast-feed.sh failed"
feed="$(gh api -H 'Accept: application/vnd.github.raw' "repos/$repo/contents/appcast.xml?ref=gh-pages")" \
  || die "cannot read the published feed back"
printf '%s\n' "$feed" | grep -q "<sparkle:shortVersionString>$version</sparkle:shortVersionString>" \
  || die "the published feed does not carry $version"
say "released $tag: GitHub release, merged appcast + bump to $next (#$pr), feed serves $version"
