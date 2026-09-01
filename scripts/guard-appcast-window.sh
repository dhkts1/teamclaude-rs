#!/bin/sh
# Close the 404 window documented in docs/RELEASING.md § "Known gap".
#
# A tag push makes cargo-dist publish a GitHub Release with the CLI tarballs and
# NO appcast.xml. That release becomes `latest`, and the SUFeedURL compiled into
# every installed TcrBar — releases/latest/download/appcast.xml — 404s until the
# DMG lands minutes later. Marking the tag prerelease keeps `latest` pointing at
# the previous good release; release-tcrbar.sh stage 9 clears the flag once the
# assets are up.
#
# The in-repo guard (quarantine_if_assetless) does not fire here: it tests for a
# release with NO assets, and a cargo-dist release has thirteen. This tests the
# predicate that actually matters — no appcast.xml asset.
#
# Watched firing, both directions, before this was trusted — a guard that has
# only ever reported "safe" proves nothing:
#
#   printf '%s' '{"isPrerelease":false,"assets":[{"name":"tcr.tar.xz"},{"name":"sha256.sum"}]}' \
#     | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if any(a["name"]=="appcast.xml" for a in d.get("assets",[])) else "no")'
#   # -> no   (window open; guard marks prerelease. quarantine_if_assetless
#   #          stands down here, because assets ARE present — that is the gap.)
#
#   printf '%s' '{"isPrerelease":false,"assets":[{"name":"appcast.xml"}]}' | (same)
#   # -> yes  (safe; guard stands down)
set -eu

TAG="${1:?usage: guard-appcast-window.sh vX.Y.Z}"
REPO=dhkts1/teamclaude-rs
DEADLINE=$(( $(date +%s) + 1800 ))

while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    if assets=$(gh release view "$TAG" --repo "$REPO" --json assets,isPrerelease 2>/dev/null); then
        has_appcast=$(printf '%s' "$assets" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if any(a["name"]=="appcast.xml" for a in d.get("assets",[])) else "no")')
        is_pre=$(printf '%s' "$assets" | python3 -c 'import json,sys; print(json.load(sys.stdin)["isPrerelease"])')
        if [ "$has_appcast" = "yes" ]; then
            echo "SAFE: $TAG carries appcast.xml — window never opened, or stage 9 closed it"
            exit 0
        fi
        if [ "$is_pre" = "True" ]; then
            echo "HELD: $TAG is prerelease with no appcast yet — latest still points at the previous release"
        else
            echo "WINDOW OPEN: $TAG is latest with no appcast.xml — marking prerelease"
            gh release edit "$TAG" --repo "$REPO" --prerelease
            echo "MARKED: $TAG now prerelease"
        fi
    fi
    sleep 15
done

echo "GAVE UP after 30m with no appcast.xml on $TAG — check the feed by hand:"
echo "  curl -sL https://github.com/$REPO/releases/latest/download/appcast.xml | grep -oE 'TcrBar-[0-9.]+\\.dmg' | head -2"
exit 1
