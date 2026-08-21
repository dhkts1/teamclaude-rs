# Can TcrBar be Rust? — feasibility, measured

Design study, 2026-08-21. Read-only survey plus two working spikes; no app source changed.
Companion to [`in-process-proxy-target-state.md`](in-process-proxy-target-state.md) and
[`ffi-in-process-target-state.md`](ffi-in-process-target-state.md).

**Question:** replace the SwiftUI menu-bar app with Rust, preserving the UI exactly.

**Answer: yes, it is buildable — and it should not be started yet.** Both of the risks that
could have killed it were retired with measurements rather than argument. What blocks the port is
not technical: `FleetView.swift` is being actively redesigned, and a port is a bet that its target
holds still.

> **Committable to a public repo.** No account emails, UUIDs, credentials or absolute paths outside
> this checkout appear below. The accessibility dump in §3 is described by shape; its live capture
> contained real account names and was not retained.

---

## 1. What the port actually is

8,137 lines of Swift, split more usefully than that number suggests.

| Bucket | Lines | Note |
|---|---:|---|
| Evaporates — the process boundary disappears | ~1,490 | `TcrTool.swift` entirely; most of `ServerController.swift`; `ToggleVerdict`'s `notHonoured` state is **structurally impossible** in-process against `Manager::set_disabled` (`src/manager/mod.rs`), which mutates live rotation *and* persists |
| Already has a Rust counterpart | ~750 | `QuotaState` (`src/stats.rs`), `ProbeStatus` (`src/probe.rs`), `StatusSource` and `HeldWindow` (`src/cli.rs`), bar fill (`src/tui.rs`). Group colour is **better** in Rust — APCA (`src/config.rs`) against Swift's WCAG, plus `derive_group_color`, which Swift has no counterpart for |
| Genuinely new Rust | **~1,250** | ordering/severity, readiness and tallies, the group tag + menu model, controller state |
| Platform code, stays `objc2` regardless | ~790 | IOKit power assertions, `SMAppService`, the menu-bar image composition |

**Plan against ~1,250 lines of new logic, not 8,137.** Two caveats on those figures: the evaporation
bucket is a *lower* bound (it excludes the `#[serde(default)]` forward-compat scaffolding and
`.unknown(String)` degrade arms that exist only because a newer client may talk to an older server —
in-process, version skew is a compile error), and the reuse bucket is an *upper* bound, because
every reusable formatter in `src/tui.rs` is a private `fn` returning a `ratatui::Style`. Reuse costs
a `pub` plus a severity-as-data/style split.

One hypothesis that did **not** survive: `src/tui.rs` was expected to be a large reuse win. It covers
the primitives and almost none of the fleet-level derivations the menu bar exists for, and it has
never rendered an account group tag at all — its "group" concept is session→account grouping.

### The real cost driver is edges, not lines

There are 19 `@Published` properties (three of them dead, with no readers). But each drives several
UI properties, and per-row edges multiply by fleet size:

```
panel chrome                    ~68 edges
per AccountRow                  ~48 edges
                      total = 68 + 48N      ≈ 692 at 13 accounts
```

Cross-checked independently: ~77 show/hide-or-recolour decisions across the view builders. SwiftUI's
diff resolves every one for free; imperative AppKit wires each by hand plus a constraint
invalidation. Every candidate architecture is really a different answer to this one number.

---

## 2. Probe A — the hardest rendering constraint, reproduced in Rust

`MenuBarMark` composes the menu-bar image so that `NSColor.labelColor` resolves **at draw time**,
letting the gauge follow the system appearance while the keep-awake cup stays cyan in both. Its
doc-comment carries the six-variant measurement showing why the app owns its `NSStatusItem` at all.
If that could not be reproduced, the port was dead.

It reproduces exactly. A standalone Rust binary using `objc2` + `objc2-app-kit`, building the image
through `NSCustomImageRep::initWithSize_flipped_drawingHandler` with a `block2::DynBlock` handler,
printed:

```
7 ON image: gauge differs between .aqua and .darkAqua, cup cyan in both
  — aqua gaugeLuma=0.000 cyan=516 · dark gaugeLuma=0.847 cyan=516
```

All four numbers match the figures recorded in `MenuBarMark.swift`.

**The oracle was falsified before it was believed.** Substituting `blackColor()` for `labelColor()`
*inside* the drawing handler — the exact defect the design exists to prevent — drove
`dark gaugeLuma` from `0.847` to `0.000` and the assertion to exit 1. Restoring it returned both.

### What cost time, and was not a wall

Every failure in the spike was one of two shapes: a missing per-header feature flag, or an import.

- **`objc2-app-kit` has no `"all"` meta-feature.** Each symbol needs its own flag, discoverable from
  the `#[cfg(feature = …)]` line directly above the failing item in the generated source. The spike
  was blocked for one round on exactly two: `NSTextField` is gated on `NSControl + NSResponder +
  NSView`, and `NSStatusItem::button` needs `NSButton + NSControl + NSResponder + NSStatusBarButton +
  NSView` simultaneously.
- `retain()` requires `objc2::Message` in scope; `alloc()` requires `objc2::AnyThread`. Neither is
  implied by importing the type.
- `NSRectEdge` lives in `objc2-foundation`, not `objc2-app-kit`.
- Swift's `usingColorSpace` is `colorUsingColorSpace:`.

---

## 3. Probe B — can parity be *proved* at cutover?

The port's real risk is not writing the code, it is knowing the result matches. The existing
snapshot harness cannot help: `ImageRenderer` will not draw AppKit controls, so all 34 golden PNGs
change by construction and cannot serve as a cross-language oracle.

The answer is the **accessibility tree**, and getting there took two attempts.

**First attempt, wrong conclusion.** A synthetic SwiftUI view hosted in `NSHostingView` and walked
via `NSAccessibility` accessors returned exactly one node with no children, under every window and
activation state tried. That looked like "SwiftUI exposes nothing."

**The controls said otherwise.** A plain AppKit view in the same process exposed its children
immediately. The difference was not SwiftUI — it was that the probe was an **unbundled** SwiftPM
Mach-O, which the accessibility server never registers as an inspectable application.

**Two findings worth keeping, because both cost a round to learn:**

1. `AXUIElementCreateApplication(getpid())` returns **`kAXErrorNotImplemented` (-25208)** for
   *same-process* querying even when fully trusted. The observer must be a **separate process**,
   exactly as a real assistive client is.
2. The target must be a real `.app` bundle. Testing against a bare executable measures nothing.

**Against the running, bundled app the tree is rich.** With the panel open, an external observer
returned a 58-node tree: application → menu bar → menu-bar item → popover → hosting view → panel,
with roles, labels, values, frames and enabled state throughout. Specifically:

- **`.accessibilityElement(children: .combine)` genuinely collapses** — each account row is *one*
  node carrying the whole combined utterance, not eight leaves. This was the largest unknown, since
  a ~70-line label builder exists to produce exactly one utterance per row.
- The drawn `QuotaBar`, which has no text, keeps its semantics via that combined label.
- **Frames are real, and expose non-uniform row heights** (84pt for ordinary rows, 100pt for rows
  carrying a reset caption). That is precisely the quantity the `GeometryReader` + `PreferenceKey`
  two-pass measurement exists to compute — so the oracle measures the hardest-to-port mechanism for
  free, and would catch a text-overflow regression of the kind already recorded in `FleetView.swift`.
- Output was byte-identical across three in-process dumps and three separate process launches.
- **Tooltips are absent from the tree**, so they need a separate flat dump checked against the
  `.help()` call sites.

### 🔴 A golden cannot be captured from a live fleet

The labels are built from real data, so a capture contains real account names. This is a public
repository with a disclosure scan. **Goldens must be generated against `src/demo.rs`**, which already
seeds believable fake accounts in varied states and hands them to the real UI. That constraint has to
be designed into the first capture, not retrofitted — otherwise it is a disclosure bug with a green
test suite.

---

## 4. Architecture, if and when it is built

Three architectures were designed independently and judged; a retained-mode **shim** won. Roughly
800 lines of Rust differ driving real AppKit views, where a conditional node is a `setHidden` flip
riding `NSStackView`'s own re-flow via `setDetachesHiddenViews`, and structural mutation is confined
to one keyed-reconcile function for the account list.

**It does not win on line count** — against a fully imperative port it is roughly a wash. It wins on
time-to-fatal-flaw: the bet it makes is settleable by a ~20-line spike on day one, rather than
discovered in week six.

Rejected, with reasons: self-drawing toolkits (they cannot keep native context menus, tooltips,
VoiceOver or text selection, all of which are required); `cacao`, the one ergonomic native-widget
crate, which has been unmaintained since 2023 and has no status-bar support at all; and `NSTableView`
for the account list, because the current list is an eager stack of persistent per-row views and a
reuse queue would introduce recycling that does not exist today — losing a text selection on scroll
and changing the accessibility tree's shape.

### Four traps found in review

1. **Do not add a GUI crate to `[workspace] members`.** CI runs `cargo clippy --all-targets --locked`
   and `cargo test --all --locked` on **ubuntu**, and `--all` is `--workspace` — Linux would compile
   the AppKit tree. Two of the three designs made this mistake. Independently, `src/lib.rs` carries
   `#![forbid(unsafe_code)]`, which rules the GUI out of the root package regardless.
2. **The shared wire type did not exist.** `AccountStatus` (`src/status.rs`) is the *server↔CLI* wire
   and lacks `quota`, `cacheHitRatio`, `serverSha`, `held`, `source`, `control` and `groupColors`.
   The wire the app decodes was an untyped `serde_json::json!` literal. Creating that type was
   valuable on its own and has since landed as `crates/tcr-status-wire`.
3. **The main-thread boundary, which no design raised.** `NSView` is `MainThreadOnly`, so no view
   handle is `Send` or `Sync`. Under the in-process decision the proxy's multi-threaded runtime
   shares a process with the view tree: only plain data may cross, and every mutation must run in a
   main-thread-confined actor fed by a channel.
4. **`.help("")` is not a no-op.** Two quota-bar call sites pass an empty string on their non-stale
   branch. `NSView.setToolTip(Some(""))` draws an empty tooltip box on the most-hovered element in
   the panel, and no snapshot, unit test or probe would see it. Map empty to `None`.

---

## 5. Why not now

A big-bang port is a bet that the target holds still. Measured 2026-08-21:

```
FleetView.swift        current size: 1,706 lines
  last  3 days   +601 / -295   over  9 commits     (~200 added lines/day)
  last  7 days  +1183 / -439   over 22 commits
  last 15 days  +2267 / -561   over 41 commits
```

The file churned more than its own length in fifteen days, and the rate is rising rather than
settling. The commit titles are the clearer signal — the same feature was designed four times in
three days: a Groups view, then dropping the tab in favour of sectioning the list, then collapsing
sections into deck cards, then replacing the whole thing with a coloured tag on the account.

That is a healthy design search. It is also the worst possible thing to port against: it means
choosing one of four designs and spending weeks translating it into a language where changing your
mind costs about three times as much. CI already says so out loud — the Swift formatter is scoped
away from `Sources/TcrBar` because it "has a large change in flight."

**The condition that changes this answer:** a week in which `FleetView.swift` takes under ~50 net
lines and no commit title contains the word "replace." When the panel stops moving, freeze it and
port it.

## 6. One correction to the motivation

The port is often justified by the shipped decode crash — a `valueNotFound` on a never-probed
account's null quota, caused by a hand-written Swift mirror of the status contract.

**That defect class is already closed**, by a mechanism that predates this study:
`tests/fixtures/status-contract.json` is verified from both the Rust and Swift sides, and its own
doc-comment calls that "the only arrangement in which a silent rename is impossible."

So the port must stand on forward value — one language, one build, one test suite, one
`cargo build` producing both binaries, and the deletion of a second toolchain, a second formatter
and a second test framework. That is a real case. It is a weaker one than the bug story, and it
should be stated honestly so nobody later measures the port against a defect it was never buying.
