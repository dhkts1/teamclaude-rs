# Library adoption — verified audit and lane plan

Audited 2026-08-08 against `origin/main`. Versions, release dates, licences and
open-issue facts were read from crates.io/docs.rs that day (σ4). Every "it fits our code"
judgement is a source reading and caps at **σ2** until something compiles.

`moka` is **CUT** by Gil's decision — do not adopt, do not re-propose.
`oauth2` and `rcgen` were already direct dependencies; they were never candidates.

## The rule every lane inherits

Each lane ships with a gate that **constructs the failure it must catch**, not a green
suite. A gate that has only ever passed proves nothing about what it guards. Watch every
new test fail under a real mutation, and have the harness prove the mutation landed —
two traps already bit agents on this repo: restoring a file with mtime preserved makes
cargo re-run the *mutated* binary, and a mutation that fails to compile proves nothing
(report INCONCLUSIVE and find a stronger one).

## Lane table

| # | Crate | Version | Files it touches | Lines Δ | Gate |
|---|---|---|---|---|---|
| 1 | `tracing-appender` | 0.2.5, MIT | `src/main.rs`, `Cargo.toml` | −7 + rotation | write past a rotation boundary, assert ≥2 files and the oldest pruned at `max_log_files`; assert the containing dir is `drwx------` |
| 2 | `governor` | 0.10.4, MIT | `src/manager/throttle.rs`, `src/manager/mod.rs` | −45 + keyed limiting | port `throttle_slot_burst1_is_strict_spacing` and `throttle_slot_burst3_admits_then_paces` (`mod.rs:1094-1121`) onto governor with a `FakeRelativeClock`; the same three `allow_at` deltas must reproduce exactly |
| 3 | `tempfile` | 3.27.0, MIT/Apache-2.0 | `src/config.rs` | −17, deletes `SAVE_SEQUENCE` | assert the persisted file is mode 0600 **and** that `sync_all` ran before `persist` — delete that line and watch a durability test go red |
| 4 | `listeners` 0.6.1 + `sysinfo` 0.39.6 | both MIT | `src/singleton.rs` | +4, removes 2 subprocess spawns | bind an ephemeral port in-process, assert `listeners` returns *this* pid; re-run every `classify_proxy_server` case through `sysinfo`'s argv vector **plus a new case with a space in the executable path**, which fails today |
| 5 | `fs4` | 1.1.0, MIT/Apache-2.0 | `src/config.rs`, `src/oauth.rs`, `src/singleton.rs` | +25, new capability | two processes racing `save_tokens` on one file: unlocked the merge loses one side's refresh token, locked neither is lost. Watch the unlocked version fail **first** |
| 6 | `arc-swap` 1.9.2 + `notify` 8.2.0 | MIT/Apache-2.0; **notify is CC0-1.0** | `src/manager/{mod,reload,state}.rs`, `src/main.rs` | ≈ −20 | a reload interleaved between the spacing and burst reads must never yield a mixed pair (assert on a single `ArcSwap` load); a `write_atomic` rename into the watched dir fires exactly one reload, a token-only write fires zero |
| 7 | `tower::retry` | tower 0.5.3, MIT | `src/proxy.rs` | 0 to **+40** | `overloaded_529_failover_worst_case_latency_is_bounded` and `the_mixed_transport_and_529_ladder_fits_the_attempt_budget` must pass unchanged; add a test that violates `MAX_SENDS_PER_ACCOUNT` and watch the Policy reject it |

## Per-lane traps — these are the reason each lane is not a one-liner

**1 · tracing-appender.** Its opener is `append(true).create(true)` with **no `mode()`**, so
rotated files land 0644 where `main.rs:686-690` deliberately creates 0600 ("the log holds
account emails + request paths"). Do **not** fix with `umask` — that is process-wide and
inherited, so every file a `tcr run`-spawned `claude` creates becomes 0600. Give the
appender its own directory under `$TMPDIR` and `chmod 0700` it once at boot; `$TMPDIR` is
already `drwx------`, so enforcement moves to where it effectively lives. **Set
`max_log_files`** — without it, rotation converts one unbounded file into unbounded files.

**2 · governor.** `Quota::with_period(spacing).allow_burst(burst)` maps 1:1 onto
`throttle_slot`. Pass `MonotonicClock` explicitly — the default `quanta` clock has open
issue #299 ("not monotonic"), which is parity with today's wall-clock `now_ms()` but
strictly worse than what governor can give. Two pacing changes are expected and must be
declared: integer-ms vs nanosecond rounding, and fleet-wide → keyed is a behaviour change
by construction, so it ships behind the existing `ThrottleConfig` gate, not as a silent
default.

**3 · tempfile.** `persist()` is `rename(2)` and **never fsyncs**. `config.rs:311`'s
`sync_all()` guards durability for a file holding live OAuth tokens and must be re-added
by hand as `f.as_file().sync_all()?` before `persist`. That single line is the whole
migration risk. 0600 is tempfile's *default*, so it needs no asking for — but assert it.

**4 · listeners + sysinfo.** Both, not either. `listeners` gives port→pid via libproc,
unprivileged on macOS — verified by reading its `proc_listpids`/`proc_pidfdinfo` path —
but it **cannot yield argv**, and `classify_proxy_server` needs the full command line to
recognise `node …/teamclaude server`. `sysinfo`'s `Process::cmd()` covers that, and its
pre-split argv vector removes a real defect: today's `cmd.split_whitespace()` mis-tokenises
any executable path containing a space. `sysinfo` MSRV is **1.95**; pin
`default-features = false` to avoid dragging in `objc2-*` and `rayon`. Read `listeners`
issue #36 (`test_consistency` failure) before starting.

**5 · fs4.** It does **not** replace the port owner file — `flock(2)` answers *is it
locked*, never *who holds it*, and the two cases `singleton.rs` exists for (a legacy JS
proxy, a non-proxy port holder) will never take our lock. Its job is an exclusive lock
across `save_tokens`' read-modify-write, turning `tcr login`'s documented "refuse to run
beside a live server" (`singleton.rs:137-144`) into *serialise*. Chosen over `fd-lock`
because `fd-lock` constrains `windows-sys` to `<0.60.0`, which collides with `notify`'s
`^0.60.1` in lane 6.

**6 · arc-swap + notify.** Watch the **parent directory**, not the file — `write_atomic`
renames, so a file watch sits on an orphaned inode after the first save and never fires
again; `~/.config` has many other writers, so the filename filter is load-bearing. This
lane also fixes a real defect on `feat/config-hot-reload`: `throttle_send` reads spacing
and burst from two independent atomics, so a reload landing between them yields a mixed
pair (σ3, read in that branch and corroborated by its own doc-comment). Read notify
issues #975 and #970 (both about the macOS fsevent backend) first. Note the CC0-1.0
licence in an otherwise MIT repo.

**7 · tower::retry.** The **same-account half only**. All four ladders (401 force-refresh,
transient-429 wait, 529 backoff, transport retry) are expressible as a `Policy` — its
`&mut self` carries state and its returned future can perform an async action, not just a
delay. The **rotation half cannot move**: tower has no way to consume a retry turn without
calling the inner service, and the soft-wait iteration at `proxy.rs:930-969` re-runs
`select()` with no upstream send at all. Adopt for the structural invariant —
`MAX_SENDS_PER_ACCOUNT` becomes something a Policy asserts rather than a formula kept in
sync, which is exactly the drift that produced a CONFIRMED bug — **not** for a line
reduction. Ship last, after the other lanes have de-risked the pattern.

## Rejected, with reasons — do not re-propose

**`tower::limit` — structural reject.** `RateLimit` is a **fixed window**:
`tower/limit/rate/service.rs` resets to `Ready { until: now + per, rem: num }` when the
window elapses, so `num` requests fire the instant a window opens. That is
burst-at-boundary, the precise thing GCRA exists to prevent and the precise thing
`throttle.rs:8-12` says the throttle is for. It also backpressures via `Poll::Pending`,
where the throttle guarantees it "holds NO resource across the sleep". `ConcurrencyLimit`
waits for a permit where the design must rotate to another account.

**`axum::serve().with_graceful_shutdown()` — net-negative, +27 lines.** It waits on
in-flight connections with **no timeout**, which is the hazard `server.rs:363-377` and
`mitm.rs:328-331` both document. The custom `Listener` trait *can* host the byte-peek
classification, so it is not a false fit on that axis — but the shutdown axis is
disqualifying. **The valuable half is reachable without it:** `mitm.rs:540-545` hard-codes
`hyper::server::conn::http1::Builder`, so base-URL clients cannot negotiate h2;
`hyper_util::server::conn::auto::Builder` gives h1+h2 auto-negotiation and **`hyper-util`
is already a dependency**. Open that as its own small lane.

**`tokio::sync::Semaphore` — net-negative, −30/+35.** Expressible (the structural blocker
I hypothesised, no non-blocking permit reduction, is false — `forget_permits()` exists).
But the repo never *waits* on the cap (an over-cap account is made ineligible and the
request rotates), so the one reason a Semaphore exists is unused; and `in_flight` is a
**rendered metric**, which `available_permits()` cannot express when the cap is `None`.

**`moka` — CUT by Gil.**

## Sequencing — derived from file overlap, not guessed

`src/manager/mod.rs` is touched by lanes **2 and 6** → sequence `2 → 6`, never concurrent.
`src/config.rs` by **3 and 5** → `3 → 5`. `src/singleton.rs` by **4 and 5** → `4 → 5`.
Lane **1** is the only genuinely isolated one (`src/main.rs` + `Cargo.toml`), which is a
second reason to run it first. Lane **7** is alone in `src/proxy.rs`.

Every lane edits `Cargo.toml`; those conflicts are mechanical (distinct added lines) and
should be resolved rather than sequenced around.

**Not a blocker, contrary to the original audit:** `tempfile`/`fs4`/`listeners` all want
`rustix ^1`, and the tree already carries `rustix 1.1.4` alongside `0.38.44` — so there is
no first-mover cost to sequence around. (The duplicate `crossterm` that caused the second
`rustix` was collapsed in PR #43.)

**Phase 3 of `in-process-proxy-target-state.md` converts the repo to a cargo workspace and
rewrites `Cargo.toml`.** It must land AFTER all of these, or it conflicts with every lane.
