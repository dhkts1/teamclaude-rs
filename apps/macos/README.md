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
already holding it, costing every live session a full cold prompt-cache prefix.
That is the worst case, not the rule, and this file used to state it as the rule.
Session affinity persists the session→account pins to
`~/.cache/teamclaude/session-affinity.json` — flushed every 5s while the map is
dirty (`src/server.rs:674-690`) and restored at boot (`src/server.rs:643`) — so a
takeover *inside* the pin TTL (`affinity::PIN_TTL_MS`, 15 minutes,
`src/affinity.rs:71`) keeps most sessions warm, while one after it restores
nothing at all. That the flush is incremental rather than shutdown-only is
exactly because of this path: `--replace` follows SIGTERM with SIGKILL, and no
shutdown hook runs for a SIGKILL (`src/affinity.rs:29-37`). None of it applies
with `sessionAffinity` off, which is the default (`src/manager/state.rs:33-46`).

That kill is now behind an explicit `--replace`; the default is to stand down and
exit without binding.

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

`tcr`'s account query is an *exact* match — the configured name, else the email
parsed out of it — and it is case-sensitive and untrimmed. It is not a substring
match, which this file claimed until 0.2.3: for an account named
`alice@example.com`, the queries `alice`, `alice@`, `example`, the same address
spelled in capitals, and the address with a trailing space all resolve to nothing. Two accounts sharing one name string are refused as ambiguous
rather than resolved, so a toggle cannot land on a row you did not name.

The exit code and both streams are captured. A non-zero exit renders the CLI's own
words in the row; a *zero* exit that still wrote to stderr is not treated as a
clean success either, because that is how `tcr` says a change was applied but not
saved. On success the panel re-polls and compares what the fleet then reports
against what was asked, rather than flipping its own copy of `disabled` — so a
change the running proxy did not honour reads as exactly that, not as a tick.

Since 0.2.3 the change also reaches a **running** proxy: `tcr enable`/`disable`
ask the live server first (`POST /_tcr/accounts/disabled`, loopback-only, api-key
required when one is configured, JSON content-type required) and fall back to
writing the config only when no server answers. A proxy too old for that route
still gets a config write, and the row says so instead of showing a tick.

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

"Keep this Mac awake" in the panel holds the three power assertions
`caffeinate -i -m -s` holds — `PreventUserIdleSystemSleep`,
`PreventSystemSleep` and `PreventDiskIdle`, taken through
`IOPMAssertionCreateWithName` — for as long as the box is ticked. While it is
on, a second tinted mark appears beside the capacity gauge in the menu bar.

Three things it deliberately does not do:

- **It does not keep the display awake.** The job is "a long run keeps running",
  and a dark screen does not stop a run. Holding the backlight on all night is a
  cost nobody asked for.
- **It does not hold sleep off on battery.** `PreventSystemSleep` "is valid only
  when system is running on AC power" (`man caffeinate`), and the power log
  agrees: on AC the effective state carries `PrevSleep`, on battery it does not.
  The panel says so while the mode is on. Whether the hold survives closing the
  lid on AC has not been measured here, so nothing claims an answer either way.
- **It does not persist across launches.** A Mac that silently never sleeps
  because of a box ticked last week is a worse bug than having to tick it again;
  the symptom (a laptop cooking in a bag) is nowhere near the cause.

Untick it, or quit TcrBar, and all three are released. The take is
all-or-nothing — if any one of the three fails the others are released again and
the control reports OFF, so the checkbox can never disagree with what the
machine is holding.

### Proving it, without a screenshot

The control is a checkbox, and nothing on the machine can click it for you.
`screencapture` needs Screen Recording, which a build machine or a headless
agent may not have, so "look at the menu bar" is not available as a gate. This
is:

```sh
# The build is its own line on purpose. `--show-bin-path` PRINTS the path and
# builds nothing, so folding the two together silently probes whatever binary
# happened to be there — which is how an earlier draft of this gate passed a
# controller with two of its three assertions deleted (2026-08-10, found by
# mutating the source and watching the gate stay green).
swift build --package-path apps/macos || exit 1
BIN="$(swift build --package-path apps/macos --show-bin-path)/TcrBar"

# Two positive controls, started HERE so the gate generates its own. A bare
# `caffeinate` holds exactly one assertion; `caffeinate -i -m -s` holds three.
# The pair is what makes the count below discriminating: with only the 1-control
# in place, a probe that had silently dropped two of its three assertions would
# still look like "the grep can see assertions".
caffeinate -t 20 &
CAFF1=$!
caffeinate -i -m -s -t 20 &
CAFF3=$!

"$BIN" --keep-awake-probe 6 &
PROBE=$!

sleep 3
pmset -g assertions | grep -c "pid $CAFF1("  # control: 1
pmset -g assertions | grep -c "pid $CAFF3("  # control: 3
pmset -g assertions | grep -c "pid $PROBE("  # held:    3 — one line per assertion
pmset -g assertions | grep -A0 "pid $PROBE(" # named:   the three types, all "TcrBar is …"

sleep 4                                      # past the release, inside the linger
ps -p "$PROBE" >/dev/null && echo "probe still alive"
pmset -g assertions | grep -c TcrBar         # released: 0, while the process still runs

wait "$PROBE"; kill "$CAFF1" "$CAFF3" 2>/dev/null
```

`grep -c "pid $PROBE("` rather than `grep TcrBar`: the count is the assertion
that matters, and the trailing `(` stops pid 1847 matching pid 18470. A run of
this gate on 2026-08-10 printed `1`, `3`, `3`, then `0` — the three named lines
are `PreventUserIdleSystemSleep`, `PreventSystemSleep` and `PreventDiskIdle`.

The flag holds the assertion for the given number of seconds and exits; like
`--render-states` it is handled before the app starts, so no menu-bar item
appears and no `tcr` subprocess is spawned.

Both samples are load-bearing, and the second one is the reason the probe stays
alive for three seconds *after* releasing. An assertion also disappears when its
process dies, so a reading taken after the probe exits would pass whether or not
`endActivity` ever ran — it would be consistent with the release working and
with the kernel cleaning up after a probe that never released. Sampling while
`ps` still shows the pid is what separates those two, so a gate that only ever
samples the hold proves half of what the linger was built for.

The control obeys the same rule. `pmset -g assertions | grep -ci caffeinate`
looks like a check that the grep can see assertions, but on a machine with no
`caffeinate` running it prints `0` and scrolls past looking like a result — its
outcome depends on ambient state rather than on anything the snippet did. So the
snippet starts one itself and greps for that pid.

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
defaults write io.github.dhkts1.tcrbar TcrExecutablePath /path/to/tcr
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
