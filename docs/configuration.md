# Configuration reference

Everything `tcr` reads out of `~/.config/teamclaude.json`, with the real default for each
key. Back to [the README](../README.md); the command-line surface is in [cli.md](cli.md).

Every default here was derived from the Rust source, not from a live config file. If you
want to check one, start from `src/config.rs`.

## The minimum viable config

Two keys are hard-required in the whole document, and both live on an account: `name` and
`accessToken` (neither carries a `#[serde(default)]`, so a missing one is a parse error).
Everything else has a default.

```json
{
  "accounts": [
    { "name": "alice@example.com", "accessToken": "sk-ant-oat01-REDACTED" }
  ]
}
```

That runs the proxy on port 3456 against `https://api.anthropic.com`, rotating the one
account at a 0.95 switch threshold, with the global egress throttle on.

In practice you never hand-write that block: `tcr login` performs the OAuth flow and
writes the account for you. Hand-editing is for the *settings* keys below.

## How the file is read

The document deserializes into `Config` with `#[serde(rename_all = "camelCase")]`, so every
key is camelCase. Unmodelled keys are preserved verbatim across a load→save round trip
rather than dropped, but seven of them are also *read*, and are documented in their own
section below.

## Top level

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `proxy` | object | `{}` (all sub-defaults) | no | listener settings, see below |
| `upstream` | string | `"https://api.anthropic.com"` | no | API base URL every rotated request is sent to |
| `switchThreshold` | float | **`0.95`** | no | fraction of an account's quota at which rotation prefers a different account |
| `pacing` | object | both knobs unset → **OFF** | no | per-account concurrency/spacing, see below |
| `throttle` | object | `{minSpacingMs: 350, burst: 4}` → **ON** | no | fleet-wide egress rate limiter, see below |
| `lockAccount` | string | absent → normal routing | no | pin ALL traffic to one account by `name` |
| `http1Only` | bool | **`false`** (OFF) | no | force the upstream client onto HTTP/1.1, see below |
| `pricing` | object | `{}` → the built-in table only | no | per-model price overrides for the usage figures, see below |
| `usageRetentionDays` | u32 | **`90`** | no | how many days of usage ledger files to keep |
| `accounts` | array | `[]` | no | the rotatable accounts |

`switchThreshold` is **0.95**, not 0.90. The default function is
`default_switch_threshold()` and it returns `0.95`; the older README said 0.90, which was
never the shipped value.

## `pricing` and `usageRetentionDays`

`tcr status --json` reports what each account's traffic **would have cost on Anthropic's API**. The
accounts are subscriptions, so nothing here is a bill; it is the only unit that compares across
accounts, models and days.

Prices come from a table built into the binary (`src/pricing.rs`), matched by longest prefix so a
dated id like `claude-haiku-4-5-20251001` resolves to its family. A model the table has no published
rate for is reported as `costUsd: null` with `unpricedRequests` counting it — never as `$0.00`, which
would be a spend nobody measured presented as one.

`pricing` is how you add or change a rate. Keys are a model id or any prefix of one; `input` and
`output` are USD per million tokens and both are required. The three cache dimensions default to the
same multipliers the built-in table uses — cache read 0.1x input, a 5-minute cache write 1.25x, a
1-hour cache write 2x — and can be set explicitly:

```json
"pricing": {
  "claude-sonnet-4-5": { "input": 3, "output": 15 },
  "claude-opus-5": { "input": 5, "output": 25, "cacheRead": 0.5 }
}
```

An override beats the built-in table outright, even when its key is the shorter prefix: a price you
wrote down is what this proxy should believe.

`usageRetentionDays` bounds the ledger under `~/.cache/teamclaude/usage/`, one `<UTC-date>.jsonl` file
per day at roughly 2 MB per 17k requests. Files older than this are deleted once, at boot. Only today's
and yesterday's are ever read back (that is what makes a restart keep the day's totals); the rest are
history for you to grep.

Lines are written by a dedicated thread, off the request path, so a stalled disk costs accounting and
never latency. A clean shutdown drains that thread's queue and waits for it to close the file, so the
requests served in the final moments before a restart are in the day it replays. Two things can still
cost lines — a write that fails, and a queue that fills faster than the disk drains it — and both are
said out loud at the moment they start, and again when they stop: the server logs the transition with a
running dropped-line count, and reports `persisting=false` for as long as it lasts — rather than a short
day presented afterwards as a measured one.

`0` is the floor, and it keeps today AND yesterday: those two files are what the boot replay reads, and
the pruner never deletes them whatever this is set to. It has to be both — on a UTC+3 machine the first
three hours of the local day live in yesterday's UTC-named file, so deleting it would cost the second
restart of the day those hours, silently and while still reporting them as measured.

**"Today" is the local calendar day of the machine the server runs on**, resolved per record against
that record's own instant, so a day boundary is correct across a daylight-saving change. Ledger FILE
names are UTC dates and need not agree with it — the file name is a shard key, not a claim about your
calendar. If the machine's UTC offset cannot be read at all, day boundaries fall back to UTC and the
server says so once in its boot log.

## `proxy.*`

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `proxy.port` | u16 | `3456` | no | port the proxy binds on loopback |
| `proxy.apiKey` | string | absent | no | shared secret required by the local `/_tcr/` control routes; the request path exempts loopback callers, and it is never handed to `claude` |

`--port` beats the file. `tcr server --port 9000` overrides `proxy.port` at runtime; the
flag is threaded into `ServeOptions.port` and wins wherever it is `Some`.

### `proxy.apiKey` is a security control, not a convenience

When it is set, it is **required on every request to the `/_tcr/` control routes, with no
loopback exemption**. That is deliberate and stricter than the request path: the doc
comment on `local_endpoint_gate` spells out why. The ordinary proxy path exempts loopback
because `claude` authenticates with its own OAuth and never sends the proxy key, but
nothing on the machine has any business reading or steering the fleet without the
operator's secret, so `/_tcr/status` and `/_tcr/accounts/disabled` cost the same secret
that using the proxy does. The check is a constant-time compare of the `x-api-key` header;
a miss is a 401.

Binding to loopback is not authorization. `127.0.0.1` is reachable by every process and
every container on the host, which is exactly the reasoning in that comment. If you set
`proxy.apiKey`, `tcr enable` and `tcr disable` need it too; they reach the running proxy
through the same gate, and a rejected key makes them refuse rather than silently fall back
to a file write.

**`tcr run` does not give this value to `claude`.** It used to export it as
`ANTHROPIC_API_KEY`, and that made Claude Code take an API key as its auth source ahead of
its claude.ai login, which **disables every claude.ai connector**, announced in one
startup line that scrolls away, after which the tools are simply absent. It bought nothing:
the request path exempts loopback clients from this key, so the child was always exempt
anyway. When a key is configured, `tcr run` says on stderr that it is withholding it; a
value the caller exported is inherited untouched. So the key's whole job is the `/_tcr/`
gate above; it is not a credential anything downstream of the proxy needs.

## `accounts[]`

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `name` | string | n/a | **yes** | display name; `tcr login` writes the account email here |
| `type` | string | `"oauth"` | no | `"oauth"` accounts are refreshed, probed and kept warm; anything else is a static-key account |
| `accountUuid` | string | absent | no | spliced into the outbound body's `metadata.user_id.account_uuid` so it agrees with the injected token |
| `orgUuid` | string | absent | no | organization identity, used to match an in-memory account back to its on-disk entry |
| `orgName` | string | absent | no | organization display name; also what `--org` matches against |
| `accessToken` | string | n/a | **yes** | the OAuth access token |
| `refreshToken` | string | absent | no | used to mint a new access token before expiry |
| `expiresAt` | i64 | absent | no | access-token expiry as **epoch milliseconds** |
| `priority` | i64 | absent → `0` | no | rotation order, **lower value = preferred** |
| `switchThreshold` | float | absent → the global value | no | per-account override of the top-level threshold |
| `disabled` | bool | absent → `false` | no | held out of rotation; this is the key `tcr disable` writes |

`type` is not decorative. Only `"oauth"` accounts get token refresh, quota probing and
keep-warm; any other value is treated as a static key.

Per-account keys the proxy does not model (`models`, `upstream`, `sx`, anything inherited
from the older Node proxy) parse fine and survive a load→save round trip untouched, but
nothing reads them.

## `pacing.*`: opt-in, ships OFF

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `pacing.maxInFlightPerAccount` | u32 | unset → no cap | no | an account at or over this many concurrent requests is skipped in selection |
| `pacing.minSpacingMs` | u64 | unset → no spacing | no | minimum gap between two selects of the *same* account |

An absent `pacing` key and a literal `"pacing": {}` are identical, and both are inert. A
configured `0` for the cap is normalised back to "unset"; `Some(0)` would make
`in_flight >= 0` true for every account and hold out the entire fleet permanently.

It ships off on purpose. The doc comment on the pacing settings gives the reason: a
per-account concurrency cap trades prompt-cache locality for load spread, and on a
single-user proxy the cache is the scarce resource while the accounts are not. Turning it
off leaves per-account concurrency genuinely unbounded; the global throttle below is a
*rate* limiter and is explicitly not a substitute for a concurrency bound.

## `throttle.*`: ships ON

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `throttle.minSpacingMs` | u64 | **`350`** with the key absent | no | steady-state emission interval across the whole fleet |
| `throttle.burst` | u32 | **`4`** with the key absent | no | how many sends fire instantly after an idle period |

### The absent-versus-empty inversion

This is the one key in the file where leaving it out and writing an empty object do
*opposite* things, and it surprises everyone:

- `throttle` **absent** → `default_throttle()` → **ON** at `minSpacingMs: 350, burst: 4`.
- `"throttle": {}` **present and empty** → every knob deserializes to `None` → `is_active()`
  is false → **fully OFF**. This is the documented escape hatch, named as such in the doc
  comment on the throttle settings.

So the way to disable the throttle is to write the key, not to delete it. Deleting it turns
the throttle back on.

A fresh proxy is therefore rate-limited out of the box: a GCRA token bucket over the single
upstream send site, four requests admitted instantly after idle and then one per 350ms
across the entire fleet. 350ms mirrors the measured probe-path aggregate rate; burst 4
covers a normal within-turn fan-out untaxed while staying below a cold-start fan-out so the
throttle actually engages on the burst. Inside a present `throttle` object an unset `burst`
is clamped to `1` (strict spacing), not to 4.

## Keys read from the unmodelled map

These seven are not fields of `Config`. They land in the flattened `extra` map and are read
back out by name across the manager (see `src/manager/state.rs` and `src/manager/throttle.rs`).
They are fully live config with a caller in the boot or request path, not compatibility
leftovers.

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `quotaProbeSeconds` | i64 seconds | **`300`** | no | quota probe cadence, the CENTRE of a per-account random draw, not a fleet period; `<= 0` disables probing |
| `warmupSeconds` | i64 seconds | **`0`** (OFF) | no | keep-warm cadence, drawn the same per-account random way; `<= 0` spawns no warm task |

### Both cadences are random per account, not a fleet sweep

Neither `quotaProbeSeconds` nor `warmupSeconds` is a period the fleet fires on. Each is the
**centre of an independent random draw per account** (`src/schedule.rs`):

| draw | distribution | at the 300s default |
|---|---|---|
| initial offset (first fire after boot, and whenever an account becomes eligible) | uniform over `[0, cadence]` seconds | `0..=300s` |
| every subsequent interval | uniform over `[cadence - 30%, cadence + 30%]` seconds, floored at 1s | `210..=390s`, 181 distinct values |

Consequences worth knowing before you tune either number:

- Two accounts do not share a probe instant, and a restart re-draws every offset rather
  than re-anchoring the fleet's phase on boot time.
- A single account's gap between probes is never the number you configured; it is a fresh
  draw each time, centred on it. Halving `quotaProbeSeconds` halves the centre, not a period.
- The probe (not keep-warm) still does ONE whole-fleet sweep at boot, spaced 350 ms, so the
  bars populate immediately instead of after a random offset.
- Randomness is drawn from a SplitMix64 generator seeded once from the boot clock. There is
  no `rand` dependency, and per-account draws are decorrelated by construction; re-reading
  the clock per account in a tight loop would not have been.
| `sessionAffinity` | bool | **`true`** (ON) | no | pin a session to the account it started on |
| `revalidationServe` | bool | **`true`** (ON) | no | serve over-threshold rather than synthesizing a 429 when the whole fleet reads over the soft threshold |
| `loadBalanceMigration` | bool | **`false`** (OFF) | no | move an already-warm session to a cooler account to even out pinned-session counts |
| `throttleExemptNoise` | bool | **`false`** (OFF) | no | skip the fleet-wide GCRA entirely for `Noise`-classified traffic (`/api/event_logging*`, `/mcp-registry*`) instead of making it queue behind inference |
| `divertBudget` | u32 | **`0`** (unlimited) | no | max distinct destination accounts one session may be diverted to inside a single hold episode; `0` = unlimited, today's behaviour byte-for-byte |

### `sessionAffinity` also pins identity-less loopback requests

"Pin a session to the account it started on" normally means keying on an `x-api-key` or the
body's `metadata.user_id` — a stable per-client identity. With `sessionAffinity` on, a
**loopback** `POST /v1/messages` that carries **neither** — a plain SDK call or a bare
`curl`, not `claude`, which always sends `metadata.user_id` — is still pinned, on a hash of
its `system` + `tools` fields (its Anthropic prompt-cache prefix) instead of a client
identity. Byte-identical requests land on the same account and keep the prompt cache warm;
different requests spread across the fleet on their own.

This only applies to a loopback caller on `POST /v1/messages` specifically —
`/v1/messages/count_tokens` and every other endpoint are excluded, since Anthropic does not
apply prompt caching to token counting, so pinning it would only concentrate load for no
cache benefit. A caller whose peer address the OS reports as non-loopback (this proxy binds
`127.0.0.1` only, in practice) never gets this pin, so N workers sharing one remote harness
and one system prompt still spread across the fleet rather than collapsing onto one account.

"Loopback" here means whatever socket address the OS hands the listener — the same check
the `proxy.apiKey` loopback exemption above ("`proxy.apiKey` is a security control, not a
convenience") already relies on. A TCP forwarder terminating locally
(`ssh -L 3456:localhost:3456 host`, for instance) makes
every tunneled remote caller present as `127.0.0.1` to this process, just as it already
bypasses the `proxy.apiKey` exemption — this is an existing property of binding to loopback,
not something this tier introduces, but it does mean the collapse this section describes as
excluded for remote callers is not excluded for one tunneled through a locally-terminating
forwarder.

There is no separate flag for this: it is entirely governed by `sessionAffinity`, and turning
that off turns this off too.

### Which knobs are opt-in and which are opt-out

Seven knobs ship OFF and must be turned on deliberately: `warmupSeconds`,
`loadBalanceMigration`, `pacing`, `lockAccount`, `http1Only`, `throttleExemptNoise` and
`divertBudget` (whose off state is `0`, unlimited, today's behaviour). (An older README said
three; `pacing` and `lockAccount` were missing from that list. `sessionAffinity` was on this
list too until it flipped to default ON — see below.)

Two knobs ship ON and are opt-**out**: `revalidationServe`, default `true`, disabled by
writing `"revalidationServe": false`; and `sessionAffinity`, also default `true`, disabled by
writing `"sessionAffinity": false`. So is `throttle`, per the inversion above, though its
off-switch is an empty object rather than a `false`.

`loadBalanceMigration` is off because Anthropic's prompt cache is per-account: every
balancing move costs a full prompt-cache re-creation of the whole conversation prefix on
the target account. The reasoning is in the doc comment on that knob.

### `warmupSeconds` spends real quota

Keep-warm is not a free health check. It issues actual upstream requests on your accounts
on a timer, and that consumption counts against the same quota your sessions draw on,
which is exactly why it defaults to `0` while the probe defaults to `300`. The doc comment
says it outright: it "spends real quota, so it ships dark and is only ever running when
explicitly enabled". Set it only when you have measured that a cold prefix costs you more
than the warming requests do.

## `lockAccount`: read this before setting it

`lockAccount` pins **all** traffic to the single account whose `name` it names. LRU
rotation, session affinity and load-balancing migration are all bypassed.

The part that bites: a locked account has **no failover**. If it is throttled, disabled or
down, requests fail: the proxy does not rotate away from it, because rotating away is the
behaviour you turned off. On a fleet of one that is no loss; on a fleet you built for
resilience it removes the resilience entirely. Treat it as a debugging tool.

A name matching no account is not a startup failure: the proxy logs an error naming the
available accounts and runs **unlocked**. So a typo'd `lockAccount` looks exactly like an
ordinary rotating proxy unless you read the log at boot.

## `groups[]` and `controlAccount`: the combination that routes nothing

`accounts[].groups` labels an account. A client asks for a label with `tcr run --group
<label>`, which sets the `x-tcr-group` header on its requests, and selection then **prefers**
accounts carrying that label.

Prefers, not requires. If no account in the group can serve the request, the proxy does not
queue and does not fail — it falls back to the whole pool and serves from any account,
because dropping a servable request is worse than serving it from the wrong place. The
fallback logs one line naming the group and the reason, at the moment it happens.

The trap is `controlAccount`. That account is the identity plane's designated account, and
inference requests **never** select it — not when it is enabled, not when it is idle, not
ever. So a group whose only member is the control account can never serve inference. Every
request asking for it falls back, on every request, permanently. Nothing about the group
looks wrong: it has a member, a colour, and a healthy line in `tcr status`.

`tcr group ls` names this directly:

```
group research accounts=1 members=gil@example.com reserved=no routes=no route_block=control-account-only ...
group dev      accounts=2 members=a@example.com,b@example.com reserved=no routes=yes ...
```

`routes=no` means the group exists and serves nothing. Two ways out: add a second, ordinary
account to the group, or move the control account elsewhere with `tcr control --clear`.
`tcr group add` prints the same warning at the moment you create the situation.

Reserving is the other half and solves a different problem. `tcr group reserve <label>` makes
an account carrying that label off-limits to traffic that did *not* ask for one of its
groups. That keeps other sessions off the account; it does **not** make `--group` strict, and
a reserved group with no available member still falls back to the pool.

## `http1Only`: caps blast radius, costs multiplexing

A connection-level fault on HTTP/2 (GOAWAY, a framing error) kills **every** multiplexed
stream sharing that connection at once — one fault can take down several concurrent
sessions on the same account. `http1Only` forces the upstream-forwarding client onto
HTTP/1.1, which does not multiplex, so the same fault costs exactly one in-flight request
instead. It is a structural cap on blast radius, not a statistical one, and it stacks with
the per-account connection pool (each account already owns its own client, so this narrows
the blast radius further within an account rather than duplicating that isolation).

The trade is real, which is why it ships OFF: h1 gives up multiplexing entirely, opens one
TCP+TLS connection per concurrent request instead of sharing one, and raises the open-socket
count against Anthropic's edge. Turn it on only if you have actually observed the
connection-fault symptom (many sessions failing at once, not a single request failing).

Its state is visible from outside the process on purpose — a knob nobody can see the state
of is how this repo once lost seven hours of prompt-cache to session-affinity being silently
off. Check `tcr status` (`http1Only=` on the `source=` line, `http1Only` per row with
`--json`) or the `"server started"` log line at boot.

## File permissions and secrecy

The config holds live OAuth access and refresh tokens for every account. It is written
`0600` (owner read/write only) and that is enforced twice: the temp file is created with
mode `0o600` before any bytes are written, and after the atomic rename an explicit
`set_permissions` normalises the mode so a restrictive umask cannot leave it somewhere
unexpected. Every write path funnels through that one function: `save`, `save_tokens`,
`save_disabled`, and by extension `tcr login` and the CLI mutators. The session-affinity
pin file and the singleton owner file get the same treatment.

Never commit this file, never paste its contents into an issue, a PR or a chat, and never
copy it into a repository checkout, including this one, which is public. Tests in this
repo write their own temporary configs with obviously fake values; do the same.
