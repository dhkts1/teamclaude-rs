#!/usr/bin/env bash
# Assemble TcrBar.app by hand — no Xcode project, no .xcodeproj to keep in sync.
#
# Output: apps/macos/build/TcrBar.app, signed with the best certificate present on
# the machine (see the signing ladder below). Notarization, stapling, DMGs and
# Sparkle are deliberately out of scope: this is a local operator tool, not a
# distributed product.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkg_dir="$(dirname "$here")"
repo_root="$(cd "$pkg_dir/../.." && pwd)"

app_name="TcrBar"
bundle_id="com.github.dhkts1.tcrbar"
build_dir="$pkg_dir/build"
app_dir="$build_dir/$app_name.app"
macos_dir="$app_dir/Contents/MacOS"

# Build stamp. A missing or unreadable .git must never fail the build.
#
# `--untracked-files=no` matches what `build.rs` uses for TCR_BUILD_DIRTY, and
# for the same reason: an untracked file cannot reach a build unless some
# TRACKED file starts referring to it. Counting untracked files made this stamp
# call a clean tracked tree "dirty" whenever scratch scripts were lying around,
# which is both wrong and exactly the kind of stamp nobody believes twice.
git_sha="unknown"
build_number="0"
if git -C "$repo_root" rev-parse --git-dir >/dev/null 2>&1; then
  git_sha="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  build_number="$(git -C "$repo_root" rev-list --count HEAD 2>/dev/null || echo 0)"
  if [ -n "$(git -C "$repo_root" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    git_sha="$git_sha-dirty"
  fi
fi

# The version is DERIVED, never written down here.
#
# It used to be a literal `0.1.0` in this script AND a literal `0.1.0` in
# Cargo.toml -- two copies of one fact, kept in step by memory alone. And it
# never moved, so every build of every commit claimed the same version.
#
# MAJOR.MINOR is read from Cargo.toml (the one place a human sets it); PATCH is
# the commit count, which rises with every commit and therefore with every push.
# That makes "bump the version before pushing" impossible to forget, because
# there is nothing to bump.
#
# Deliberately NOT a pre-push hook: git resolves the refs to push before
# pre-push runs, so a hook that edits a version file cannot get that edit into
# the push it is running for. It would either leave the tree dirty with the bump
# excluded, or commit a bump that ships one push late -- forever publishing N
# while the tree reads N+1.
base_version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([0-9]\{1,\}\.[0-9]\{1,\}\)\..*"/\1/p' "$repo_root/Cargo.toml" 2>/dev/null | head -1)"
if [ -z "$base_version" ]; then
  echo "WARNING: could not read version from Cargo.toml — falling back to 0.0" >&2
  base_version="0.0"
fi
short_version="$base_version.$build_number"

echo "==> swift build -c release --product $app_name"
swift build --package-path "$pkg_dir" -c release --product "$app_name"
binary="$(swift build --package-path "$pkg_dir" -c release --product "$app_name" --show-bin-path)/$app_name"

echo "==> assembling $app_dir"
rm -rf "$app_dir"
mkdir -p "$macos_dir"
cp "$binary" "$macos_dir/$app_name"

# Bundle the `tcr` server binary alongside the app.
#
# The app and the server ship as ONE artifact so they cannot drift: before this,
# the two were built separately and TcrBar resolved `tcr` from `PATH`, which
# drifted twice in a single day. `TcrTool.resolve()` probes
# `Contents/MacOS/tcr` ahead of `PATH` for the same reason.
#
# The output path is READ OUT OF CARGO, never assumed to be `$repo_root/target`.
# `CARGO_TARGET_DIR` redirects it, and assuming otherwise is not a hypothetical:
# `.githooks/post-merge` announced `built <sha>` for a binary that had landed in
# `$CARGO_TARGET_DIR` while the `target/release/tcr` a symlink actually resolved
# to stayed at the old sha. A build that reports success from the wrong path is
# worse than a failed one.
echo "==> cargo build --release --bin tcr"
cargo build --manifest-path "$repo_root/Cargo.toml" --release --bin tcr
cargo_target_dir="$(cargo metadata --manifest-path "$repo_root/Cargo.toml" --no-deps --format-version 1 | jq -r .target_directory)"
tcr_binary="$cargo_target_dir/release/tcr"
if [ ! -x "$tcr_binary" ]; then
  echo "ERROR: cargo reported target_directory=$cargo_target_dir but $tcr_binary is not executable." >&2
  exit 1
fi
cp "$tcr_binary" "$macos_dir/tcr"
chmod +x "$macos_dir/tcr"

# Assert the COPY carries the sha we think we built.
#
# `build.rs` stamps `TCR_BUILD_SHA` into the binary, so the bundled file can be
# asked what it is rather than trusted. A bundle holding a stale `tcr` is worse
# than no bundle at all — the app would then confidently serve old code. The
# expected value is the bare short sha; `$git_sha` may carry a `-dirty` suffix
# that is not part of the stamp.
expected_sha="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ "$expected_sha" = "unknown" ]; then
  echo "    tcr: bundled (no git sha to verify against)"
elif grep -aq "$expected_sha" "$macos_dir/tcr"; then
  echo "    tcr: bundled from $tcr_binary (sha $expected_sha)"
else
  echo "ERROR: $macos_dir/tcr does not carry the expected build sha $expected_sha." >&2
  echo "ERROR: the bundled server would be a DIFFERENT build from this checkout." >&2
  exit 1
fi

# The icon is DRAWN BY THE APP, not committed as a binary asset.
#
# `AppIcon.swift` renders it from the same `Tok` values the panel uses, so the
# mark cannot drift from the palette the way a checked-in .icns silently does.
# That is why this runs after the binary is copied: the binary is the generator.
#
# Non-fatal on purpose. A missing icon costs a generic placeholder in Finder; it
# is not worth failing a build that is otherwise fine, and a hard failure here
# would block `swift build` working on a machine without `iconutil`.
iconset="$(mktemp -d)/AppIcon.iconset"
if "$macos_dir/$app_name" --render-icon "$iconset" >/dev/null 2>&1 \
    && command -v iconutil >/dev/null 2>&1 \
    && iconutil -c icns "$iconset" -o "$app_dir/Contents/Resources/AppIcon.icns" 2>/dev/null; then
  echo "    icon: generated"
else
  mkdir -p "$app_dir/Contents/Resources"
  if "$macos_dir/$app_name" --render-icon "$iconset" >/dev/null 2>&1 \
      && iconutil -c icns "$iconset" -o "$app_dir/Contents/Resources/AppIcon.icns" 2>/dev/null; then
    echo "    icon: generated"
  else
    echo "    WARNING: could not generate the app icon — Finder will show a placeholder." >&2
  fi
fi
rm -rf "$(dirname "$iconset")"

cat >"$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>$app_name</string>
	<key>CFBundleIdentifier</key>
	<string>$bundle_id</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIconName</key>
	<string>AppIcon</string>
	<key>CFBundleName</key>
	<string>$app_name</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$short_version</string>
	<key>CFBundleVersion</key>
	<string>$build_number</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>TcrGitSHA</key>
	<string>$git_sha</string>
</dict>
</plist>
PLIST

# Signing ladder — loudest-safe-first.
#
# The signature is not ceremony here: "Launch at login" registers the bundle with
# macOS by *code identity*. An ad-hoc signature has no certificate behind it, so
# its cdhash changes on every rebuild and a previously-registered login item ends
# up pointing at an identity that no longer matches. The same is true of any
# permission grant the app is ever given.
#
# So: prefer a real certificate. No identity name, team id or email is written
# down here — this repository is public. The label prefix is matched with
# `grep -F` (fixed string, no regex) and the full quoted identity is read out of
# the keychain at build time.
first_identity_matching() {
  # $1 is a certificate-class label prefix, e.g. "Developer ID Application".
  # Lines look like: `  1) <SHA1> "<Class>: <Name> (<TEAM>)"`
  # `|| true`: "no identity of this class" is a normal answer, and under
  # `set -o pipefail` grep's exit 1 would otherwise abort the whole build.
  security find-identity -v -p codesigning 2>/dev/null \
    | grep -F "\"$1: " \
    | head -n 1 \
    | sed -E 's/^[^"]*"(.*)"[[:space:]]*$/\1/' || true
}

sign_identity=""
sign_tier=""
for class in "Developer ID Application" "Apple Development"; do
  sign_identity="$(first_identity_matching "$class")"
  if [ -n "$sign_identity" ]; then
    sign_tier="$class"
    break
  fi
done

# Nested Mach-O binaries are signed BEFORE the bundle that contains them.
#
# The outer signature seals the bundle's contents, so signing `Contents/MacOS/tcr`
# afterwards would mutate a file the app's own seal covers and invalidate it.
# `codesign -v --deep --strict` on the finished bundle is what proves the order
# is right, and per the note above an invalid signature is not cosmetic: it
# silently breaks "Launch at login" and every permission grant.
if [ -n "$sign_identity" ]; then
  echo "==> codesign ($sign_tier)"
  codesign -s "$sign_identity" --force "$macos_dir/tcr"
  codesign -s "$sign_identity" --force "$app_dir"
else
  sign_tier="ad-hoc"
  echo "==> codesign (ad-hoc)"
  codesign -s - --force "$macos_dir/tcr"
  codesign -s - --force "$app_dir"
  {
    echo
    echo "WARNING: no codesigning certificate found — signed ad-hoc."
    echo "WARNING: an ad-hoc signature has no stable code identity, so its hash"
    echo "WARNING: changes on every rebuild. Consequences:"
    echo "WARNING:   - 'Launch at login' silently stops working after a rebuild,"
    echo "WARNING:     because the registered login item points at an identity"
    echo "WARNING:     that no longer matches this bundle."
    echo "WARNING:   - any future permission grant (Accessibility, Screen"
    echo "WARNING:     Recording, Automation) has to be re-granted each rebuild."
    echo "WARNING: Fix: install a 'Developer ID Application' or 'Apple"
    echo "WARNING: Development' certificate into the login keychain, then rebuild."
    echo
  } >&2
fi

echo "built $app_dir  version=$short_version build=$build_number sha=$git_sha signing=$sign_tier"
