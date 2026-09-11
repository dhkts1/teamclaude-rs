#!/usr/bin/env bash
# Publish apps/macos/appcast.xml to refs/heads/gh-pages as a single committed
# file, so SUFeedURL (build-tcrbar.sh) is a FIXED location that stops
# depending on which GitHub Release happens to be `latest`.
#
# WHY. `/releases/latest/download/appcast.xml` follows whichever release is
# newest, including a CLI-only cargo-dist release that carries no appcast.xml.
# That broke every installed copy's updater five times, most recently
# 2026-08-27 (v0.2.27) — see docs/RELEASING.md § "The update feed" and the
# history in .github/workflows/appcast-guard.yml's header. This script is the
# fix: the feed now lives at a URL only THIS script ever writes, and the
# tag-triggered cargo-dist workflow (.github/workflows/release.yml) never
# touches the gh-pages branch, so it cannot become the feed by accident.
#
# HOW. Pure git plumbing (hash-object / mktree / commit-tree), never a
# checkout. This never touches this worktree's index, working tree or HEAD —
# the exact thing CLAUDE.md's "You are not alone" rule asks every state-
# mutating git op here to rule out first. It reads one tracked file
# (apps/macos/appcast.xml) and writes one ref (refs/heads/gh-pages); nothing
# a sibling session has uncommitted can collide with either.
#
# NOT called by release-tcrbar.sh. That script's own header states it
# performs no git writes, on purpose, because this checkout routinely holds
# sibling sessions' uncommitted work — wiring this in automatically would make
# that statement false for a script whose whole design rests on it being true.
# Run this by hand, once per release, right after release-tcrbar.sh stage 9
# and once apps/macos/appcast.xml's new <item> is committed to main (see
# docs/RELEASING.md's "Cutting a release" steps).
#
# Usage:
#   publish-appcast-feed.sh [--dry-run] [--branch gh-pages] [--repo owner/name]
#
# Environment:
#   RELEASE_REPO   owner/name (default: dhkts1/teamclaude-rs, same default
#                  build-tcrbar.sh's feed_url is hardcoded to)
#
# Exit status: 0 when the feed already serves these exact bytes (nothing to
# push) or the push succeeded. Non-zero on anything else, including a push
# rejected because refs/heads/gh-pages is protected or push access is
# missing — this script never falls back to force-pushing around that.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkg_dir="$(dirname "$here")"
repo_root="$(cd "$pkg_dir/../.." && pwd)"
appcast_path="$pkg_dir/appcast.xml"

die() { printf 'ERROR: %s\n' "$@" >&2; exit 1; }
note() { printf '    %s\n' "$1"; }
usage() { sed -n '2,33p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

branch="gh-pages"
dry_run=0
repo="${RELEASE_REPO:-dhkts1/teamclaude-rs}"

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --branch)  branch="${2:-}"; shift ;;
    --repo)    repo="${2:-}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *)         die "unknown argument: $1" ;;
  esac
  shift
done

[ -n "$branch" ] || die "--branch given an empty value."
[ -n "$repo" ] || die "could not determine owner/repo; pass --repo or set RELEASE_REPO=owner/name."
[ -f "$appcast_path" ] || die "no appcast at $appcast_path — nothing to publish." \
  "release-tcrbar.sh stage 8 writes it; run that first."

feed_url="https://raw.githubusercontent.com/$repo/$branch/appcast.xml"

blob_sha="$(git -C "$repo_root" hash-object -w -- "$appcast_path")" \
  || die "git hash-object failed on $appcast_path."

# Fetch the branch's current tip, if it exists, so the new commit is a child
# of it rather than a fresh root every time — cheap history, and it is what
# lets a later `git log origin/gh-pages` show which release changed the feed.
# A branch that does not exist yet (the very first run) fails this fetch;
# swallowed, because "no parent" is the correct state for that case, not an
# error.
git -C "$repo_root" fetch --quiet origin "refs/heads/$branch" 2>/dev/null || true
parent_sha="$(git -C "$repo_root" rev-parse --verify -q FETCH_HEAD 2>/dev/null || true)"

if [ -n "$parent_sha" ]; then
  existing_blob="$(git -C "$repo_root" ls-tree "$parent_sha" -- appcast.xml 2>/dev/null | awk '{print $3}')"
  if [ "$existing_blob" = "$blob_sha" ]; then
    note "refs/heads/$branch already serves this exact appcast.xml — nothing to push."
    note "feed URL: $feed_url"
    exit 0
  fi
fi

tree_sha="$(printf '100644 blob %s\tappcast.xml\n' "$blob_sha" \
  | git -C "$repo_root" mktree)" || die "git mktree failed."

commit_args=(-m "chore: publish appcast.xml to the gh-pages feed")
if [ -n "$parent_sha" ]; then
  commit_args+=(-p "$parent_sha")
fi
commit_sha="$(git -C "$repo_root" commit-tree "$tree_sha" "${commit_args[@]}")" \
  || die "git commit-tree failed."

if [ "$dry_run" = 1 ]; then
  note "dry run — would push $commit_sha to refs/heads/$branch on $repo"
  note "(commit object was created locally; nothing was pushed)"
  note "feed URL once pushed: $feed_url"
  exit 0
fi

git -C "$repo_root" push origin "$commit_sha:refs/heads/$branch" \
  || die "push of $commit_sha to $repo:$branch failed." \
         "Check push access, and that refs/heads/$branch carries no branch" \
         "protection this operator's token cannot satisfy."

note "published $appcast_path -> refs/heads/$branch ($commit_sha)"
note "feed URL: $feed_url"
