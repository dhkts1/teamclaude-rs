# Configuration reference

Everything `tcr` reads out of `~/.config/teamclaude.json`, with the real default for each
key and the source line it comes from. Back to [the README](../README.md); the command-line
surface is in [cli.md](cli.md).

Every value here was read out of the Rust source, not out of a live config file. Paths are
relative to the repository root.

## The minimum viable config

Two keys are hard-required in the whole document, and both live on an account: `name` and
`accessToken` (`src/config.rs:95`, `:104`; neither carries a `#[serde(default)]`, so a
missing one is a parse error). Everything else has a default.

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

The document deserializes into `Config` (`src/config.rs:213-247`) with
`#[serde(rename_all = "camelCase")]`, so every key is camelCase. Unmodelled keys are
preserved verbatim across a load→save round trip rather than dropped
(`src/config.rs:245-246`), but five of them are also *read*, and are documented in their
own section below.

## Top level

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `proxy` | object | `{}` (all sub-defaults) | no | listener settings, see below (`src/config.rs:216-217`) |
| `upstream` | string | `"https://api.anthropic.com"` | no | API base URL every rotated request is sent to (`src/config.rs:218-219`, default at `:29-31`) |
| `switchThreshold` | float | **`0.95`** | no | fraction of an account's quota at which rotation prefers a different account (`src/config.rs:220-221`, default at `:32-34`) |
| `pacing` | object | both knobs unset → **OFF** | no | per-account concurrency/spacing, see below (`src/config.rs:228-229`) |
| `throttle` | object | `{minSpacingMs: 350, burst: 4}` → **ON** | no | fleet-wide egress rate limiter, see below (`src/config.rs:233-234`) |
| `lockAccount` | string | absent → normal routing | no | pin ALL traffic to one account by `name` (`src/config.rs:240-241`) |
| `accounts` | array | `[]` | no | the rotatable accounts (`src/config.rs:242-243`) |

`switchThreshold` is **0.95**, not 0.90. The default function is
`default_switch_threshold()` at `src/config.rs:32-34` and returns `0.95`; the older README
said 0.90, which was never the shipped value.

## `proxy.*`

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `proxy.port` | u16 | `3456` | no | port the proxy binds on loopback (`src/config.rs:72-73`, default at `:26-28`) |
| `proxy.apiKey` | string | absent | no | shared secret required by the local `/_tcr/` control routes; the request path exempts loopback callers, and it is never handed to `claude` (`src/config.rs:74-75`) |

`--port` beats the file. `tcr server --port 9000` overrides `proxy.port` at runtime; the
flag is threaded into `ServeOptions.port` at `src/main.rs:548` and wins wherever it is
`Some`.

### `proxy.apiKey` is a security control, not a convenience

When it is set, it is **required on every request to the `/_tcr/` control routes, with no
loopback exemption**. That is deliberate and stricter than the request path: the doc
comment on `local_endpoint_gate` (`src/proxy.rs:810-852`) spells out why. The ordinary
proxy path exempts loopback because `claude` authenticates with its own OAuth and never
sends the proxy key, but nothing on the machine has any business reading or steering the
fleet without the operator's secret, so `/_tcr/status` and `/_tcr/accounts/disabled` cost
the same secret that using the proxy does. The check is a constant-time compare of the
`x-api-key` header at `src/proxy.rs:871-880`; a miss is a 401.

Binding to loopback is not authorization. `127.0.0.1` is reachable by every process and
every container on the host, which is exactly the reasoning in that comment. If you set
`proxy.apiKey`, `tcr enable` and `tcr disable` need it too; they reach the running proxy
through the same gate, and a rejected key makes them refuse rather than silently fall back
to a file write (`src/cli.rs:246-252`).

**`tcr run` does not give this value to `claude`.** It used to export it as
`ANTHROPIC_API_KEY`, and that made Claude Code take an API key as its auth source ahead of
its claude.ai login, which **disables every claude.ai connector**, announced in one
startup line that scrolls away, after which the tools are simply absent. It bought nothing:
the request path exempts loopback clients from this key, so the child was always exempt
anyway. When a key is configured, `tcr run` says on stderr that it is withholding it
(`src/main.rs:329-334`, reasoning at `:387-408`); a value the caller exported is inherited
untouched. So the key's whole job is the `/_tcr/` gate above; it is not a credential
anything downstream of the proxy needs.

## `accounts[]`

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `name` | string | n/a | **yes** | display name; `tcr login` writes the account email here (`src/config.rs:95`) |
| `type` | string | `"oauth"` | no | `"oauth"` accounts are refreshed, probed and kept warm; anything else is a static-key account (`src/config.rs:96-97`, default at `:64-66`) |
| `accountUuid` | string | absent | no | spliced into the outbound body's `metadata.user_id.account_uuid` so it agrees with the injected token (`src/config.rs:98-99`) |
| `orgUuid` | string | absent | no | organization identity, used to match an in-memory account back to its on-disk entry (`src/config.rs:100-101`) |
| `orgName` | string | absent | no | organization display name; also what `--org` matches against (`src/config.rs:102-103`) |
| `accessToken` | string | n/a | **yes** | the OAuth access token (`src/config.rs:104`) |
| `refreshToken` | string | absent | no | used to mint a new access token before expiry (`src/config.rs:105-106`) |
| `expiresAt` | i64 | absent | no | access-token expiry as **epoch milliseconds** (`src/config.rs:107-109`) |
| `priority` | i64 | absent → `0` | no | rotation order, **lower value = preferred** (`src/config.rs:110-111`; the `unwrap_or(0)` at `src/manager/mod.rs:313`) |
| `switchThreshold` | float | absent → the global value | no | per-account override of the top-level threshold (`src/config.rs:112-113`) |
| `disabled` | bool | absent → `false` | no | held out of rotation; this is the key `tcr disable` writes (`src/config.rs:114-115`) |

`type` is not decorative. Only `"oauth"` accounts get token refresh, quota probing and
keep-warm; any other value is treated as a static key.

Per-account keys the proxy does not model (`models`, `upstream`, `sx`, anything inherited
from the older Node proxy) parse fine and survive a load→save round trip untouched
(`src/config.rs:117-118`), but nothing reads them.

## `pacing.*`: opt-in, ships OFF

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `pacing.maxInFlightPerAccount` | u32 | unset → no cap | no | an account at or over this many concurrent requests is skipped in selection (`src/config.rs:132-133`) |
| `pacing.minSpacingMs` | u64 | unset → no spacing | no | minimum gap between two selects of the *same* account (`src/config.rs:136-137`) |

An absent `pacing` key and a literal `"pacing": {}` are identical, and both are inert. A
configured `0` for the cap is normalised back to "unset" (`src/config.rs:147-152`);
`Some(0)` would make `in_flight >= 0` true for every account and hold out the entire
fleet permanently.

It ships off on purpose. The doc comment at `src/config.rs:35-49` gives the reason: a
per-account concurrency cap trades prompt-cache locality for load spread, and on a
single-user proxy the cache is the scarce resource while the accounts are not. Turning it
off leaves per-account concurrency genuinely unbounded; the global throttle below is a
*rate* limiter and is explicitly not a substitute for a concurrency bound.

## `throttle.*`: ships ON

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `throttle.minSpacingMs` | u64 | **`350`** with the key absent | no | steady-state emission interval across the whole fleet (`src/config.rs:181-182`) |
| `throttle.burst` | u32 | **`4`** with the key absent | no | how many sends fire instantly after an idle period (`src/config.rs:187-188`) |

### The absent-versus-empty inversion

This is the one key in the file where leaving it out and writing an empty object do
*opposite* things, and it surprises everyone:

- `throttle` **absent** → `default_throttle()` (`src/config.rs:56-63`) → **ON** at
  `minSpacingMs: 350, burst: 4`.
- `"throttle": {}` **present and empty** → every knob deserializes to `None` → `is_active()`
  is false → **fully OFF**. This is the documented escape hatch, named as such in the doc
  comment at `src/config.rs:169-175`.

So the way to disable the throttle is to write the key, not to delete it. Deleting it turns
the throttle back on.

A fresh proxy is therefore rate-limited out of the box: a GCRA token bucket over the single
upstream send site, four requests admitted instantly after idle and then one per 350ms
across the entire fleet. 350ms mirrors the measured probe-path aggregate rate; burst 4
covers a normal within-turn fan-out untaxed while staying below a cold-start fan-out so the
throttle actually engages on the burst. Inside a present `throttle` object an unset `burst`
is clamped to `1` (strict spacing), not to 4 (`src/config.rs:200-203`).

## Keys read from the unmodelled map

These five are not fields of `Config`. They land in the flattened `extra` map and are read
back out by name in `src/manager/state.rs`. They are fully live config with a caller in the
boot or request path, not compatibility leftovers.

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `quotaProbeSeconds` | i64 seconds | **`300`** | no | quota probe cadence, the CENTRE of a per-account random draw, not a fleet period; `<= 0` disables probing (`src/manager/state.rs:9-17`, constant at `src/probe.rs:30`) |
| `warmupSeconds` | i64 seconds | **`0`** (OFF) | no | keep-warm cadence, drawn the same per-account random way; `<= 0` spawns no warm task (`src/manager/state.rs:23-31`) |

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
| `sessionAffinity` | bool | **`false`** (OFF) | no | pin a session to the account it started on (`src/manager/state.rs:38-46`) |
| `revalidationServe` | bool | **`true`** (ON) | no | serve over-threshold rather than synthesizing a 429 when the whole fleet reads over the soft threshold (`src/manager/state.rs:53-61`) |
| `loadBalanceMigration` | bool | **`false`** (OFF) | no | move an already-warm session to a cooler account to even out pinned-session counts (`src/manager/state.rs:74-82`) |

### Which knobs are opt-in and which are opt-out

Five knobs ship OFF and must be turned on deliberately: `sessionAffinity`, `warmupSeconds`,
`loadBalanceMigration`, `pacing` and `lockAccount`. (An older README said three; `pacing`
and `lockAccount` were missing from that list.)

One knob ships ON and is the file's only opt-**out**: `revalidationServe`, default `true`,
disabled by writing `"revalidationServe": false`. So is `throttle`, per the inversion above,
though its off-switch is an empty object rather than a `false`.

`loadBalanceMigration` is off because Anthropic's prompt cache is per-account: every
balancing move costs a full prompt-cache re-creation of the whole conversation prefix on
the target account. The reasoning is at `src/manager/state.rs:63-73`.

### `warmupSeconds` spends real quota

Keep-warm is not a free health check. It issues actual upstream requests on your accounts
on a timer, and that consumption counts against the same quota your sessions draw on,
which is exactly why it defaults to `0` while the probe defaults to `300`. The doc comment
says it outright: it "spends real quota, so it ships dark and is only ever running when
explicitly enabled" (`src/manager/state.rs:20-22`). Set it only when you have measured that
a cold prefix costs you more than the warming requests do.

## `lockAccount`: read this before setting it

`lockAccount` pins **all** traffic to the single account whose `name` it names. LRU
rotation, session affinity and load-balancing migration are all bypassed
(`src/config.rs:235-239`).

The part that bites: a locked account has **no failover**. If it is throttled, disabled or
down, requests fail: the proxy does not rotate away from it, because rotating away is the
behaviour you turned off. On a fleet of one that is no loss; on a fleet you built for
resilience it removes the resilience entirely. Treat it as a debugging tool.

A name matching no account is not a startup failure: the proxy logs an error naming the
available accounts and runs **unlocked** (`src/manager/mod.rs:637-647`). So a typo'd
`lockAccount` looks exactly like an ordinary rotating proxy unless you read the log at boot.

## File permissions and secrecy

The config holds live OAuth access and refresh tokens for every account. It is written
`0600` (owner read/write only) and that is enforced twice: the temp file is created with
mode `0o600` before any bytes are written (`src/config.rs:326-327`), and after the atomic
rename an explicit `set_permissions` normalises the mode so a restrictive umask cannot
leave it somewhere unexpected (`src/config.rs:434-439`). Every write path funnels through
that one function: `save`, `save_tokens`, `save_disabled`, and by extension `tcr login`
and the CLI mutators. The session-affinity pin file and the singleton owner file get the
same treatment.

Never commit this file, never paste its contents into an issue, a PR or a chat, and never
copy it into a repository checkout, including this one, which is public. Tests in this
repo write their own temporary configs with obviously fake values; do the same.
