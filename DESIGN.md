# teamclaude-rs — lean single-user rotating Anthropic proxy (Rust)

A drop-in replacement for Gil's **personal** teamclaude proxy on `:3456`. It is NOT the
company multi-tenant pool (that stays a separate multi-tenant service). Single user, his own
~7 accounts, rotate on quota, refresh tokens, stream to Anthropic, show a live TUI. Target
~1000–1500 lines. Binary name: `tcr`.

## Drop-in config — `~/.config/teamclaude.json` (existing, unchanged)
Read the SAME file the JS proxy uses. serde structs (camelCase via `#[serde(rename_all="camelCase")]`), tolerant of unknown fields (`#[serde(default)]` + ignore extras — the file also has `routes` and `sx` we don't need; `quotaProbeSeconds` and `warmupSeconds` we DO read — see `manager/state.rs`). Persist token refreshes back to it atomically (temp file + rename), 0600, preserving unknown fields (round-trip via `serde_json::Value` for the parts we don't model, OR re-serialize a struct that carries `#[serde(flatten)] extra: Map<String,Value>`).

```
Config { proxy: {port:u16=3456, apiKey:Option<String>}, upstream:String="https://api.anthropic.com",
         switchThreshold:f64=0.95, accounts: Vec<Account> }
Account { name:String, r#type:String="oauth", accountUuid:Option, orgUuid:Option, orgName:Option,
          accessToken:String, refreshToken:Option<String>, expiresAt:Option<i64/*epoch ms*/>,
          priority:Option<i64>=0, switchThreshold:Option<f64>, disabled:Option<bool>=false }
```
Runtime-only (NOT persisted): current index, per-account learned quota + reset times, rate-limit holds, status (ok/throttled/error), request/token counters, refresh coalescing latch.

## Modules (src/)
- `main.rs` — clap CLI: `tcr server` (default; TUI unless `--headless`), flags `--port`, `--config`, `--headless`. Boot: load config → build `Manager` → spawn axum server task → run TUI (or block headless) → on Ctrl-C, flush.
- `config.rs` — load/save (atomic, preserve unknown fields), `Config`/`Account`.
- `oauth.rs` — `refresh_access_token(refresh_token) -> Tokens` (reqwest POST to the Anthropic OAuth token endpoint; port the exact request shape from `~/git/teamclaude/src/oauth.js` refreshAccessToken — client id, grant_type=refresh_token, the token URL). `is_expired(expires_at)`, `is_expiring_soon`. Typed error distinguishing 4xx auth-reject (dead refresh token → mark account error) vs transient 5xx/network (keep token, fail this request over).
- `quota.rs` — parse `anthropic-ratelimit-unified-*` (and 5h/weekly) response headers into `{utilization:f64, reset:OffsetDateTime}` per window. `is_near(threshold)`; **a window whose `reset` is in the past reads as fresh (0%)** — computed live, never a stale cached bar (this is bug #2, designed out).
- `manager.rs` — `Manager` holding `Vec<AccountState>` behind `Arc<RwLock<..>>` (or `parking_lot::RwLock`; single-writer selection is fine). `select(&self, tried:&HashSet<usize>) -> Option<usize>`: priority tier first (lowest value wins; higher-priority preempts a healthy current), then within tier soonest-reset / unknown-quota first, eligibility = not disabled + not held + under threshold (per-account `switchThreshold` else global). `ensure_fresh(idx)` → refresh if hard-expired, **coalesced** (one in-flight refresh per account via a `tokio::sync::Mutex`/`OnceCell` per account or a shared map — concurrent requests on the same expired account await one refresh, not N). `update_quota`, `update_usage(idx, input, output)`, `mark_rate_limited(idx, until)`, `clear_rate_limited(idx)`. Counters increment on the ACTUAL serving account index (bug #3 — never a stale current index after a mid-request rotation).
- `proxy.rs` — axum catch-all `/{*path}` handler, all methods:
  1. auth: `x-api-key` header must equal `config.proxy.apiKey` when set (single user; loopback may bypass like the JS one). Missing/wrong → 401.
  2. buffer the request body once (`Bytes`) — needed to re-send on rotation.
  3. loop up to `accounts.len()`: `idx = manager.select(tried)` (None → 429 + `retry-after`); `manager.ensure_fresh(idx)`; build upstream request: clone headers minus `x-api-key`/hop-by-hop/`accept-encoding`, set `authorization: Bearer <access_token>`; `reqwest` to `{upstream}{path}` with the buffered body; `manager.update_quota(idx, resp.headers())`.
     - 2xx → **stream** the body back: return an axum `Body` from the reqwest `bytes_stream()`, passing every chunk through UNCHANGED, while a **side** consumer (feed a cloned byte stream OR inspect each chunk) parses SSE via `eventsource-stream` to capture `message_start`→input tokens and `message_delta`→output tokens → `manager.update_usage(idx, ..)`. The passthrough must not be gated on the parser (no coupled backpressure). Non-stream JSON body → parse usage from the buffered JSON.
     - 429 → if unified-status "rejected"/quota → `mark_rate_limited` + rotate; else bounded wait (clamp [1,300]s) + retry SAME account (body re-sent). 401 → force-refresh once + retry; 2nd 401 → mark error + rotate. 5xx → forward verbatim. network error → transient, rotate; exhausted → 502.
- `stats.rs` — the shared snapshot the TUI reads (accounts, per-account quota windows + reset, usage counters, current index, recent request log ring buffer). Written by the proxy path, read by the TUI. Live (Arc), not polled from disk.
- `tui.rs` — ratatui + crossterm. 500ms tick + event stream. Table of accounts (name, priority, status, 5h/weekly quota bars computed LIVE against reset, request count, tokens, last-used). A request log pane. Keys: `q` quit, maybe `d`/`e` disable/enable. **The quota bars recompute expiry every tick** — never show a window past its reset as still-full (fixes the "stats look stale" symptom).

## The two bugs we design out (from the JS proxy)
1. **SSE usage miscount on chunk-split boundaries** — the JS parser can drop a `message_start`/`message_delta` split across two network chunks. FIX: `eventsource-stream` buffers incomplete events across chunks. Correct by construction.
2. **Quota bars stale between requests / past reset** — the JS TUI shows the last-learned utilization until the next request re-learns it, and can show an expired window as still-full. FIX: store `reset` timestamps; the TUI computes display live each tick and treats `now > reset` as a fresh window (0%). Usage counters increment on the true serving account.

## Gates (the coder MUST run)
- `cargo build` clean, `cargo clippy -- -D warnings` clean, `cargo fmt --check`.
- `cargo test` — unit tests: config round-trips (loads Gil's real key set shape; unknown fields preserved on save); quota header parse; expired-window-reads-fresh; SSE usage parse across a deliberately chunk-split stream; account selection (priority/threshold/disabled/hold); refresh coalescing (N concurrent → 1 refresh).
- A local end-to-end smoke on a FREE port (NOT 3456 — Gil's live proxy owns it): boot headless with a 1-account test config (a dummy account is fine for the non-network paths), assert the server binds + `/` with no key → 401 + with the key → attempts upstream. Do not hit the real Anthropic API in tests; mock or gate that.

## Non-goals (keep it lean — NOT in this binary)
Per-client keys, admin API, client locks, per-client attribution, egress routing, the configurable route table. Single global user. If Gil wants any later it's additive.

**Keep-warm was a non-goal and is now shipped** (`src/warmer.rs`, `src/manager/warm.rs`), opt-in via
`warmupSeconds` and OFF by default because — unlike the zero-spend usage probe — it costs real quota.
It is called out here so this section is not read as a claim that it is absent: a doc that denies a
shipped feature is worse than no doc, because the next reader reasons from it. This one did mislead a
reader, in 2026-07.

**Persisting runtime state across restarts was evaluated and rejected** (2026-07-31). Rate-limit holds
looked like the one candidate worth keeping — they are self-expiring and no probe can re-derive them —
but persisting them removes the restart escape hatch: with every account held, a restored fleet refuses
every selection path and the proxy makes no upstream attempt at all until the holds expire, turning
"restart to clear it" into an outage with a non-obvious recovery. The measured benefit was a handful of
429 round trips per restart. Not worth it. Quota is likewise deliberately not persisted — the probe
re-derives it within seconds, and a stale window can outlive its truth.
