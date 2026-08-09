# TcrBar

A macOS menu-bar front end for `tcr`. It shows the live fleet — one row per
account, with its quota bar, `quotaState` pill and reset time — can take a single
account in or out of rotation, and can start the proxy as a supervised child
process.

It contains no Rust and changes none: everything it knows comes from shelling out
to `tcr status --json`.

## Why it shells out instead of calling the proxy

`GET /_tcr/status` exists, but it requires the operator's proxy API key and has no
loopback exemption. Using it would mean a GUI process holding that secret. `tcr
status --json` authenticates itself, so TcrBar never reads `~/.config/teamclaude.json`,
never handles a token and never sends an `Authorization` header.

## The one safety property

`tcr`'s port singleton can take the port over: a starting server may kill a proxy
already holding it, which wipes the session→account pin map and costs every live
session a full cold prompt-cache prefix. That kill is now behind an explicit
`--replace`; the default is to stand down and exit without binding.

TcrBar therefore always spawns `tcr server --no-replace` on the routine path, and
reaches for `--replace` only from "Take over port…", behind a confirmation. (The
flag is redundant against a current `tcr`, where standing down is the default, and
load-bearing against an older one, where omitting it meant takeover.) It only ever
signals a child **it** spawned. A server that was already running is displayed and left
alone — there is no code path that can terminate it. A spawn that declines because
an incumbent holds the port is reported as "already running", which is a success.

## Enabling and disabling an account

Each row has an Enable/Disable button. It shells out to `tcr enable <name>` /
`tcr disable <name>` and nothing else — TcrBar never writes
`~/.config/teamclaude.json`, because that file holds credentials and `tcr` owns it.

`tcr`'s account query is a *case-insensitive substring* match, so even passing the
exact configured name is not a guarantee of a unique hit. The exit code and stderr
are therefore captured and the CLI's own words are rendered in the row on failure;
a toggle that did not happen never looks like one that did. On success the panel
re-polls rather than flipping its own copy of `disabled`, so what you see is
always what `tcr status` reports.

That is a statement about the *config*. Whether a proxy that is already running
picks the change up without a restart is not something this app verifies, in
either direction, and it does not claim to.

Disabling is reversible, so there is no confirmation dialog.

## What the menu-bar glyph means

Fleet **capacity**, not the worst account:

- any account ready → ok
- none ready, at least one `near` → amber
- none ready, none near → red

A rotating pool is *supposed* to contain spent accounts — that is the mechanism
working. A worst-account-wins glyph sat at its most alarming setting permanently
and therefore meant nothing.

## Build and run

```sh
cd apps/macos
swift build          # debug build
swift test           # unit tests
bash scripts/build-tcrbar.sh
open build/TcrBar.app
```

`scripts/build-tcrbar.sh` produces a `build/TcrBar.app` with
`LSUIElement` set (menu-bar only, no Dock icon) and stamps `CFBundleVersion` from
the commit count plus a `TcrGitSHA` key from the short SHA, suffixed `-dirty` on a
dirty tree. Developer ID signing, notarization and DMG packaging are out of scope.

## The bundled `tcr`

`build-tcrbar.sh` builds the Rust `tcr` binary and copies it into
`TcrBar.app/Contents/MacOS/tcr`, so the app and the server it drives are one
artifact and cannot drift. They previously drifted twice in a single day.

Two details are load-bearing:

- The build reads cargo's output directory from
  `cargo metadata --no-deps --format-version 1`, never assuming `target/release`.
  `CARGO_TARGET_DIR` redirects it, and a build that lands elsewhere while
  reporting success is exactly how the drift happened.
- The copy is then checked for the `TCR_BUILD_SHA` that `build.rs` stamps, and
  the build fails if it does not match `HEAD`. A bundle holding a stale `tcr` is
  worse than no bundle, because the app would confidently serve old code.

`Contents/MacOS/tcr` is codesigned **before** the bundle around it. Signing a
nested Mach-O afterwards mutates a file the outer signature seals, which
invalidates it — and per the note in the script an invalid signature silently
breaks "Launch at login" and every permission grant. `codesign -v --deep
--strict build/TcrBar.app` is the gate that proves the order.

### Optionally pointing the CLI at the bundle

Once the bundle is verified runnable, `~/.local/bin/tcr` can be repointed at
`/Applications/TcrBar.app/Contents/MacOS/tcr` so the CLI and the app share one
binary and only one thing needs updating.

Do this by hand, and verify first — `Contents/MacOS/tcr --version` must run from
the installed bundle. That symlink is what every shell `tcr` invocation resolves
through, **including live `tcr run` sessions**: aim it at a missing, unsigned or
quarantined binary and every `tcr` command on the machine breaks at once. The
build script deliberately does not touch it.

## Finding the `tcr` binary

An app launched from Finder inherits a minimal `PATH`, so TcrBar probes, in
order: the `TCR_BIN` environment variable, the `TcrExecutablePath` defaults key,
the `tcr` bundled next to its own executable, then `PATH`, then the usual
install directories (`~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`,
`/usr/local/bin`).

The bundled binary sits between the overrides and `PATH` on purpose: an operator
who names a path means it, but a `tcr` shipped inside this bundle must beat
whatever happens to be on `PATH`, or bundling buys nothing. Override explicitly
with either:

```sh
defaults write com.github.dhkts1.tcrbar TcrExecutablePath /path/to/tcr
TCR_BIN=/path/to/tcr open build/TcrBar.app     # env override, shell launches
```

If nothing is found the panel says so and names how many locations it searched —
the bundle path included — it never shows an empty list.

## A hidden menu-bar icon does not quit the app

The whole UI is one `MenuBarExtra`, so the app declares exactly one scene. When
Control Center hides the status item, SwiftUI tears that scene down — and a
SwiftUI `App` left with zero scenes terminates itself. AppKit's default answer to
`applicationShouldTerminate` is *yes*, so the app agreed: every launch exited 0
within seconds, with no output and no crash report, which reads as a broken build
rather than a quit. It cost a full day of no menu-bar app on 2026-08-09.

`AppDelegate.applicationShouldTerminate` now REFUSES any termination nobody
asked for, and `TerminationPolicy` holds the list of things that legitimately
ask: the panel's Quit button, a logout or shutdown, Sparkle relaunching into a
new version, and any quit request that arrives as an Apple event (`osascript`,
the Dock, Cmd-Q). The status-item teardown matches none of them. The item is also
bound through `MenuBarExtra(isInserted:)`, so a hide becomes observable state
rather than a scene that vanishes.

If the icon is hidden and the panel is therefore unclickable, `tcr ui` brings it
back: that runs `open -b <bundle id>`, which reaches
`applicationShouldHandleReopen` and re-inserts the item. The hidden state is
never persisted, so relaunching also restores it.

Both halves were measured separately rather than assumed. Against a reproduction
that kills the unfixed app instantly, a build carrying only the delegate survived
60s, and so did a build carrying only the `isInserted:` binding — each prevents
the death on its own. The redundancy is kept deliberately: they fail for
different reasons, since the binding relies on SwiftUI keeping a
declared-but-absent scene alive, which is undocumented, while the delegate relies
only on AppKit asking before it quits.

To reproduce the failure, run the executable directly instead of through `open`:

```sh
apps/macos/build/TcrBar.app/Contents/MacOS/TcrBar; echo "exit=$?"
```

A build without the fix exits 0 within a second with no output — the reported
signature exactly. A fixed build stays up. Launching this way denies the process
a status item for the same reason a hidden icon does, so it reaches the same
teardown without involving Control Center.

`TcrHideMenuBarItemForTesting` is the other reproduction, driving the teardown
from inside a normally launched app:

```sh
defaults write com.github.dhkts1.tcrbar.dev TcrHideMenuBarItemForTesting -bool true
```

Neither goes through Control Center itself, because that cannot be scripted: it
owns the visibility in memory and ignores `defaults write
com.apple.controlcenter "NSStatusItem Visible Item-0"` entirely — measured, the
app mirrored `1` straight back over a `0` written seconds earlier.

## Reading the panel honestly

Three states look similar and are not:

- **`tcr` not found** — the binary is missing, nothing was polled.
- **poll failed (non-zero exit)** — usually no server is running.
- **`source: offline`** — `tcr` answered without a server, so the quota bars are
  real (they come from a live probe) but every serving counter is structurally
  zero. The panel labels this; a structural zero is never rendered as a
  measurement, and a `null` cache-hit ratio shows as `n/a`, not `0%`.

## Layout

```
Package.swift
Sources/TcrBarCore/   FleetStatus.swift  StatusPoller.swift  ServerController.swift
                      AccountControl.swift  LoginItem.swift  TcrTool.swift
                      TerminationPolicy.swift
Sources/TcrBar/       TcrBarApp.swift  FleetView.swift  Tokens.swift  Updater.swift
Tests/TcrBarTests/    FleetStatusTests.swift
scripts/build-tcrbar.sh
```

The logic lives in the `TcrBarCore` library so it can be tested without linking a
test bundle against an `@main` executable; `TcrBar` is the SwiftUI shell.
