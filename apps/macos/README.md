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

## Keeping the Mac awake

"Keep this Mac awake" in the panel holds an idle-system-sleep power assertion —
the same thing `caffeinate -i` does — for as long as the box is ticked. While it
is on, a second tinted mark appears beside the capacity gauge in the menu bar.

Three things it deliberately does not do:

- **It does not keep the display awake.** The job is "a long run keeps running",
  and a dark screen does not stop a run. Holding the backlight on all night is a
  cost nobody asked for.
- **It does not survive closing the lid.** No assertion of this class does. The
  panel says so while the mode is on, because an operator who believes otherwise
  comes back to a dead run and blames the proxy.
- **It does not persist across launches.** A Mac that silently never sleeps
  because of a box ticked last week is a worse bug than having to tick it again;
  the symptom (a laptop cooking in a bag) is nowhere near the cause.

Untick it, or quit TcrBar, and the assertion is released.

### Proving it, without a screenshot

The control is a checkbox, and nothing on the machine can click it for you.
`screencapture` needs Screen Recording, which a build machine or a headless
agent may not have, so "look at the menu bar" is not available as a gate. This
is:

```sh
BIN="$(swift build --package-path apps/macos --show-bin-path)/TcrBar"
"$BIN" --keep-awake-probe 10 &
sleep 3
pmset -g assertions | grep TcrBar        # PreventUserIdleSystemSleep, named
pmset -g assertions | grep -ci caffeinate # positive control: the grep can see assertions
wait
```

The flag holds the assertion for the given number of seconds and exits; like
`--render-states` it is handled before the app starts, so no menu-bar item
appears and no `tcr` subprocess is spawned.

It stays alive for three seconds *after* releasing, and that linger is the point
of the gate rather than politeness: an assertion also disappears when its process
dies, so a reading taken after the probe exits would pass whether or not the
release ever happened. Sampling while the process is still running is what makes
the result attributable.

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
                      AwakeController.swift  KeepAwakeGlyph.swift  KeepAwakeProbe.swift
                      MenuBarMark.swift  LaunchPreference.swift
Sources/TcrBar/       TcrBarApp.swift  MenuBarShell.swift  ShellProbe.swift
                      FleetView.swift  Tokens.swift  RenderStates.swift  AppIcon.swift
Tests/TcrBarTests/    FleetStatusTests.swift
scripts/build-tcrbar.sh
```

The logic lives in the `TcrBarCore` library so it can be tested without linking a
test bundle against an `@main` executable; `TcrBar` is the shell.

## Why the shell is AppKit and not `MenuBarExtra`

It was a SwiftUI `MenuBarExtra`, and a `MenuBarExtra` renders its label
**monochrome no matter what the image says**. Six label constructions were each
hosted in a real one and rasterised off the real `NSStatusBarButton`:
`Image(nsImage:)` with `isTemplate = false`, the same with
`.renderingMode(.original)`, a coloured `Text("●")`, and a symbol pre-flattened
to a plain bitmap all came back with **0 coloured pixels**; an emoji managed 14.
Setting `button.image` directly on the button gave **533 of 533**.

So the app owns an `NSStatusItem`, composes the menu-bar image itself
(`MenuBarMark`), and hosts the same unchanged `FleetView` in an `NSPopover`.
Three things `MenuBarExtra` did for free are now explicit, and each is a silent
regression if it is dropped: the popover is told the panel's preferred size, the
login-item bit is re-read on every open rather than only on the first, and the
app is activated before the panel is shown so text selection works.

### Proving the shell, without a screenshot

```sh
BIN="$(swift build --package-path apps/macos --show-bin-path)/TcrBar"
"$BIN" --shell-probe        # one line per assertion, non-zero if any failed
```

It builds the real shell in-process and checks nine things, including that the
ON mark rasterises to cyan pixels off the real status button **and** that the OFF
mark rasterises to none — the negative control, without which the first
assertion passes on a mark that is cyan in both states. Every one of the nine was
broken on purpose and watched go red.

What it does not cover: the window server's final composite of the menu bar, a
real mouse click, and anything about a signed bundle. Those are still a human's
eyes.
