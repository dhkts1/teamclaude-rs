#!/usr/bin/env bash
# Refuse a release that is already known to fail. Runs in about a second and
# asserts, before anything is built, signed, notarized or pushed, the five
# conditions that have actually broken this project's releases.
#
#   apps/macos/scripts/release-preflight.sh v0.2.5
#
# It is safe to run standalone and by design it CHANGES NOTHING: it creates no
# tag, uploads nothing, touches no running process and writes no file. It reads
# Cargo.toml, the appcast, the git index, `origin` and the GitHub Releases API,
# and then either exits 0 or names what is wrong.
#
# WHY THIS EXISTS. Four consecutive releases of this project broke the Sparkle
# update feed, and `docs/plans/swarm-retrospectives.md` prescribed this script
# three times before anyone wrote it. The specific incidents, in the order the
# checks below are numbered:
#
#   1. Two version numbers for one release. `release-tcrbar.sh` already refuses
#      this at stage 0, but by then a release is underway; a disagreement is
#      cheaper to find before the operator has walked away from the machine.
#
#   2. A tag that already exists. v0.2.4 (2026-08-10) was re-tagged onto a
#      commit that did NOT carry the fix, because a `commit && tag` chain was
#      never checked and the commit half had been blocked by a hook. A local
#      `git tag -l` cannot see that: the fact that matters is whether `origin`
#      has the tag, because pushing the tag is what starts the release. This
#      check asks the remote and treats "could not ask" as a failure, not a
#      pass — a gate that cannot reach its evidence has not checked anything.
#
#   3. An appcast that already carries this version. `release-tcrbar.sh` stage 8
#      writes the <item> even under `--dry-run`, so a rehearsal poisons the real
#      run — which then dies AFTER notarizing (2026-08-10 morning postmortem,
#      "I rehearsed with --dry-run without knowing the rehearsal mutates the
#      tree"). The expensive work is already spent by the time stage 8 refuses.
#
#   4. A GitHub Release for this tag that exists and has no appcast.xml on it.
#      This is the one that caused the outage. `releases/latest/download/
#      appcast.xml` is the URL Sparkle fetches; the tag-triggered cargo-dist
#      workflow creates the Release the moment the tag is pushed and it becomes
#      `latest` immediately, while the signed DMG and the appcast are uploaded
#      by the local script MINUTES later. Every user's update check in that
#      window fails with "An error occurred in retrieving update information",
#      and GitHub's CDN caches the 404 against the exact URL Sparkle requests.
#      Measured on v0.2.4 (2026-08-10): Release created 20:40:42Z, appcast
#      uploaded 20:52:19Z — an 11m 37s outage. The same failure was ~2 minutes
#      that morning and had been recorded the day before that. If this check
#      fires, the feed is dark RIGHT NOW and the fix is to upload the appcast
#      to that release (or mark it prerelease so `latest` falls back), not to
#      start another release.
#
#   5. An uncommitted apps/macos/appcast.xml. The v0.2.3 entry was written by
#      stage 8, never committed, and the published feed sat on 0.2.1 for a day
#      with a signed 0.2.3 DMG already on the release. A dirty appcast at the
#      START of a release means the PREVIOUS one is still half-finished.
#
# The rule those five encode, stated once: push a version tag ONLY in the
# operation that can immediately upload its assets, and never start a release on
# top of an unfinished one.
#
# NOTHING HERE PRINTS A SECRET OR A PATH, same rule as its neighbours and for
# the same reason: this repository is public. The certificate probe reports that
# a "Developer ID Application" identity is present and never the holder's name,
# email or team id, and every path printed is relative to the repository root.
#
# Environment:
#   RELEASE_REPO   owner/name for the GitHub Release. Defaults to whatever
#                  `gh repo view` reports for this checkout.
set -euo pipefail

usage() { sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
esac

tag="${1:-}"
if [ -z "$tag" ]; then
  echo "usage: $(basename "$0") vX.Y.Z" >&2
  echo "  asserts the release is safe to start; changes nothing" >&2
  exit 2
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkg_dir="$(dirname "$here")"
repo_root="$(cd "$pkg_dir/../.." && pwd)"

# Relative, always. An absolute path here would publish someone's home
# directory into a world-readable log, and the repo's own disclosure gate
# blocks it at commit time.
readonly appcast_rel="apps/macos/appcast.xml"
appcast_path="$repo_root/$appcast_rel"

failures=0
ok()   { printf '    ok   %s\n' "$1"; }
info() { printf '    ---  %s\n' "$1"; }
check() { printf '\n==> check %s\n' "$1"; }
fail() {
  failures=$((failures + 1))
  printf '    FAIL %s\n' "$1" >&2
  shift
  for line in "$@"; do printf '         %s\n' "$line" >&2; done
}

# Every check runs even after one fails. An operator who is about to spend
# twenty minutes on a notarized build should be told everything that is wrong
# in one pass, not made to fix them one exit code at a time.

# ---------------------------------------------------------------------------
# 1 — the tag and Cargo.toml agree
# ---------------------------------------------------------------------------
# Character-for-character the sed in release-tcrbar.sh:352 and .githooks/
# pre-commit. Three copies is a real cost; a preflight that reads a DIFFERENT
# number than the script it guards is worse than no preflight.
check "1/5  the tag matches the version in Cargo.toml"
# Two versions, deliberately. `version` is what the manifest claims; `release_version`
# is what the TAG would publish, and every later check keys on the tag — because
# a preflight whose other checks silently re-aim at a different number the moment
# this one fails would answer a question nobody asked.
release_version="${tag#v}"
version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([0-9][^"]*\)".*/\1/p' "$repo_root/Cargo.toml" | head -1)"
if [ -z "$version" ]; then
  fail "could not read a version from Cargo.toml." \
       "Nothing below can be checked without it."
  version=""
elif [ "${tag#v}" != "$version" ]; then
  fail "tag $tag disagrees with Cargo.toml version $version." \
       "One release must not carry two version numbers." \
       "Bump the version in Cargo.toml, or pass v$version."
else
  ok "$tag  ==  Cargo.toml $version"
fi

# ---------------------------------------------------------------------------
# 2 — the tag does not exist, locally OR on origin
# ---------------------------------------------------------------------------
check "2/5  the tag does not name a different commit"
# The guard is against a tag that names OTHER code, not against the tag
# existing: the documented order pushes vX.Y.Z first, because that push is what
# starts the release, and the local script then uploads onto the Release the
# push created. A tag that already points at HEAD is therefore the expected
# state here. What must still refuse is a tag pointing somewhere else — that is
# the v0.2.4 incident, where a tag was re-pointed onto a commit that did not
# carry the fix and the release shipped code the tag did not name.
head_sha="$(git -C "$repo_root" rev-parse HEAD)"
if git -C "$repo_root" rev-parse -q --verify "refs/tags/$tag" >/dev/null 2>&1; then
  local_tag_sha="$(git -C "$repo_root" rev-parse "$tag^{commit}")"
  if [ "$local_tag_sha" = "$head_sha" ]; then
    ok "local tag $tag points at HEAD"
  else
    fail "$tag exists as a LOCAL tag naming a DIFFERENT commit than HEAD." \
         "Releasing would publish assets for code the tag does not name." \
         "Inspect it first:  git show $tag --stat" \
         "tag: ${local_tag_sha:0:12}  HEAD: ${head_sha:0:12}"
  fi
else
  ok "no local tag $tag"
fi

# `git ls-remote`, never a local ref and never FETCH_HEAD: local refs are a
# cache of the remote as of the last fetch, and the tag push is what triggers
# the release, so the remote is the only authority. --exit-code makes the three
# outcomes distinguishable — 0 found, 2 no such ref, anything else an error we
# must NOT read as "absent".
#
# Two refs are asked for, not one: for an ANNOTATED tag (`git tag -a`, what
# this project uses), `refs/tags/$tag` names the tag OBJECT, not the commit —
# ls-remote returns that wrapper's own sha, which never equals a commit sha and
# so always disagreed with $head_sha even when the tag was correct. The peeled
# form `refs/tags/$tag^{}` is what ls-remote reports as the commit the tag
# points at; a LIGHTWEIGHT tag has no such entry because it IS the commit ref,
# so the plain form is used only when the peeled one is absent — never compare
# a wrapper sha to a commit sha.
set +e
remote_out="$(git -C "$repo_root" ls-remote --exit-code --tags origin "refs/tags/$tag" "refs/tags/$tag^{}" 2>&1)"
remote_rc=$?
set -e
case "$remote_rc" in
  0)
    remote_peeled_sha=""
    remote_plain_sha=""
    while IFS=$'\t' read -r sha ref; do
      case "$ref" in
        "refs/tags/$tag^{}") remote_peeled_sha="$sha" ;;
        "refs/tags/$tag") remote_plain_sha="$sha" ;;
      esac
    done <<<"$remote_out"
    remote_sha="${remote_peeled_sha:-$remote_plain_sha}"
    if [ -z "$remote_sha" ]; then
      fail "ls-remote reported $tag but neither the peeled commit nor the" \
           "plain ref could be parsed out of its output." \
           "Not treated as absent: an unread answer is not a pass." \
           "git said: ${remote_out:-<no output>}"
    elif [ "$remote_sha" = "$head_sha" ]; then
      ok "tag $tag on origin points at HEAD (commit ${remote_sha:0:12}) — the tag push is what starts the release"
    else
      fail "$tag exists on origin naming a DIFFERENT commit than HEAD." \
           "A release started now would upload assets for code the published" \
           "tag does not name, and moving a published tag is the v0.2.4" \
           "incident. Reconcile the tag before releasing." \
           "origin (commit): ${remote_sha:0:12}  HEAD: ${head_sha:0:12}"
    fi ;;
  2)
    ok "no tag $tag on origin" ;;
  *)
    fail "could not ask origin whether $tag exists (git ls-remote exit $remote_rc)." \
         "This check is not skippable: a local-only tag check cannot prove a" \
         "remote fact, and a release started on an unproven assumption is how" \
         "a tag gets pushed twice. Fix connectivity and re-run." \
         "git said: ${remote_out:-<no output>}" ;;
esac

# ---------------------------------------------------------------------------
# 3 — the appcast has no item for this version
# ---------------------------------------------------------------------------
# Matched as an ELEMENT, `<sparkle:shortVersionString>X</...>`, which is the
# shape release-tcrbar.sh stage 8 actually writes and the shape every existing
# item in the feed has. Note that release-tcrbar.sh:452 greps for the ATTRIBUTE
# form `sparkle:shortVersionString="X"` instead, which appears nowhere in the
# file — so its own guard cannot fire, and this check is currently the only one
# standing between a poisoned tree and a post-notarization abort. Fixing that
# line is a separate change; do not delete this check when it lands.
check "3/5  the appcast has no item for this version"
if [ ! -f "$appcast_path" ]; then
  ok "$appcast_rel does not exist yet (release-tcrbar.sh will create it)"
elif grep -qF "sparkle:shortVersionString>$release_version<" "$appcast_path"; then
  fail "$appcast_rel already has an <item> for $release_version." \
       "release-tcrbar.sh stage 8 writes that entry EVEN UNDER --dry-run, so a" \
       "rehearsal leaves it behind and the real run then dies at stage 8 —" \
       "after notarizing, which is the expensive part." \
       "Either this version was already released, or a dry run poisoned the" \
       "tree. Check with:  git diff -- $appcast_rel"
else
  ok "no <item> for $release_version in $appcast_rel"
fi

# ---------------------------------------------------------------------------
# 4 — the update feed is not currently dark
# ---------------------------------------------------------------------------
check "4/5  no assetless GitHub Release is serving the update feed"
repo="${RELEASE_REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)}"
if [ -z "$repo" ]; then
  fail "could not determine the GitHub repository." \
       "Set RELEASE_REPO=owner/name, or sign in with: gh auth login"
else
  # The REST endpoint rather than `gh release view`, because the two outcomes
  # have to be told apart: a 404 means no release yet (fine, the normal case)
  # while an auth or network error means we do not know (never fine). Anything
  # that is not a clean 404 and not a clean success is a failure here.
  #
  # `is_draft`/`is_prerelease` come out of the SAME call: `releases/latest`
  # skips both a draft and a prerelease and falls back to the next eligible
  # release, so an assetless release that is EITHER is not serving the feed —
  # it is exactly the state check 4's own remedy tells an operator to reach
  # for (`gh release edit $tag --prerelease`). Failing on that state refuses
  # the very remedy it recommends.
  set +e
  api_out="$(gh api "repos/$repo/releases/tags/$tag" --jq '[([.assets[].name] | join(" ")), (.draft|tostring), (.prerelease|tostring)] | join("\t")' 2>&1)"
  api_rc=$?
  set -e
  if [ "$api_rc" = 0 ]; then
    IFS=$'\t' read -r assets is_draft is_prerelease <<<"$api_out"
    case " $assets " in
      *" appcast.xml "*)
        ok "release $tag exists and already carries appcast.xml" ;;
      *)
        if [ "$is_draft" = "true" ] || [ "$is_prerelease" = "true" ]; then
          ok "release $tag exists, is assetless, but is $( [ "$is_draft" = "true" ] && printf draft || printf prerelease )." \
             "releases/latest/download/appcast.xml falls back past it, so it is" \
             "not serving the feed. It still needs finishing before it can go" \
             "live: assets currently on it: ${assets:-<none>}"
        else
          fail "release $tag ALREADY EXISTS and has no appcast.xml asset." \
               "The update feed is dark RIGHT NOW: releases/latest/download/appcast.xml" \
               "is the URL Sparkle fetches, and it resolves to this release." \
               "Every installed copy's update check is failing with" \
               "\"An error occurred in retrieving update information\", and GitHub's" \
               "CDN caches that 404 against the exact URL Sparkle requests." \
               "Measured on v0.2.4, 2026-08-10: 11m 37s of outage this way." \
               "" \
               "Do NOT start another release. Finish this one, or step out of the" \
               "way of the feed:" \
               "  gh release edit $tag --prerelease --repo $repo   # latest falls back" \
               "  gh release upload $tag <dmg> $appcast_rel --clobber --repo $repo" \
               "  gh release edit $tag --prerelease=false --latest --repo $repo" \
               "" \
               "assets currently on it: ${assets:-<none>}"
        fi ;;
    esac
  elif printf '%s' "$api_out" | grep -q '404'; then
    ok "no GitHub Release for $tag yet — nothing is serving an empty feed"
  else
    fail "could not ask GitHub about release $tag." \
         "Not treated as absent: this is the check that stands between a user" \
         "and a broken update feed, and it must not pass on an unread answer." \
         "gh said: ${api_out:-<no output>}"
  fi
fi

# ---------------------------------------------------------------------------
# 5 — the previous release finished
# ---------------------------------------------------------------------------
check "5/5  $appcast_rel has no uncommitted changes"
dirty="$(git -C "$repo_root" status --porcelain -- "$appcast_rel")"
if [ -n "$dirty" ]; then
  fail "$appcast_rel is uncommitted in the working tree." \
       "A previous release wrote its <item> and nobody committed it. That is" \
       "not cosmetic: it is how v0.2.3 shipped a signed DMG while the published" \
       "feed sat on 0.2.1 for a day. Commit it before starting another release." \
       "git status says: $dirty"
else
  ok "$appcast_rel is clean"
fi

# ---------------------------------------------------------------------------
# Information — not failures
# ---------------------------------------------------------------------------
# Neither of these is required to run this script, and neither is checked as a
# gate: a preflight for a `--dry-run` rehearsal on a machine with no credentials
# must still pass. They are printed because a release that dies three minutes
# into a notarize for want of a certificate is a bad way to learn.
printf '\n==> credentials (information only)\n'

# Only the certificate CLASS is ever printed. `security find-identity` reports
# the identity as `Developer ID Application: Some Human (TEAMID)`, and the part
# after the first `: ` is a name and a team id — see the same rule at the top of
# release-tcrbar.sh.
if security find-identity -v -p codesigning 2>/dev/null \
     | grep -qF '"Developer ID Application: '; then
  info "Developer ID Application certificate: present in the login keychain"
else
  info "Developer ID Application certificate: NOT FOUND — a real release will fail"
  info "  check with:  security find-identity -v -p codesigning"
fi

if command -v op >/dev/null 2>&1; then
  info "1Password CLI (op): on PATH"
else
  info "1Password CLI (op): NOT on PATH — release-local.sh will refuse"
fi

# ---------------------------------------------------------------------------
if [ "$failures" -gt 0 ]; then
  printf '\npreflight %s: %s check(s) FAILED — not safe to release\n' "$tag" "$failures" >&2
  exit 1
fi
printf '\npreflight %s: all checks passed\n' "$tag"
