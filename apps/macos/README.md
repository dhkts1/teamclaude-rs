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

`tcr`'s port singleton is a *takeover*: by default a starting server kills a proxy
already holding the port, which wipes the session→account pin map and costs every
live session a full cold prompt-cache prefix.

TcrBar therefore always spawns `tcr server --no-replace`, and only ever signals a
child **it** spawned. A server that was already running is displayed and left
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
Sources/TcrBar/       TcrBarApp.swift  FleetView.swift  Tokens.swift
Tests/TcrBarTests/    FleetStatusTests.swift
scripts/build-tcrbar.sh
```

The logic lives in the `TcrBarCore` library so it can be tested without linking a
test bundle against an `@main` executable; `TcrBar` is the SwiftUI shell.
