<img src="assets/tcrbar-icon.png" alt="" width="104" align="right">

# teamclaude-rs (`tcr`)

A lean, single-user **rotating Anthropic proxy** in Rust. Point Claude Code — or any
Anthropic API client — at it, and it spreads requests across several Claude accounts,
refreshes their OAuth tokens for you, and shows what every account has left.

[![CI](https://github.com/dhkts1/teamclaude-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/dhkts1/teamclaude-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-stable-orange.svg)

It is a from-scratch Rust rewrite of the Node proxy
[teamclaude](https://github.com/KarpelesLab/teamclaude): same on-disk config, same certs,
same port, so it drops into an existing setup.

![tcr live TUI](assets/tui-demo.gif)

The dashboard above is `tcr demo` — the real TUI rendered against fake accounts, which is
also how the screenshots further down were made. Run it yourself with `tcr demo`; it needs
no config and contacts nothing.

## Install

The published installer fetches a prebuilt binary for your platform and puts it on your
PATH:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dhkts1/teamclaude-rs/releases/latest/download/teamclaude-rs-installer.sh | sh
```

From source, if you have a Rust toolchain and would rather build it:

```sh
cargo build --release
scripts/install-cli.sh          # places `tcr` at ~/.local/bin/tcr by default
```

Use `scripts/install-cli.sh` rather than `cp` when a proxy is already running. It stages
the new binary in the destination directory and renames it into place; copying over a
binary that a live process is executing rewrites the same inode, and macOS then kills
every later `exec` of it with `Code Signature Invalid`.

| Platform | `tcr` CLI | TcrBar menu-bar app |
|---|---|---|
| macOS, Apple silicon (`aarch64-apple-darwin`) | prebuilt | yes |
| macOS, Intel (`x86_64-apple-darwin`) | prebuilt | yes |
| Linux x86_64, musl (`x86_64-unknown-linux-musl`) | prebuilt | — |
| Linux aarch64, musl (`aarch64-unknown-linux-musl`) | prebuilt | — |
| Anything else | build from source | — |

The Linux builds are static musl binaries, so they run on Alpine and on glibc distributions
alike. The menu-bar app and the `tcr ui` subcommand are macOS-only; everything else in this
README works on both.

## Quickstart

```sh
tcr login                 # PKCE browser flow; writes tokens into ~/.config/teamclaude.json
tcr                       # start the proxy with the live TUI (q quits)
```

Repeat `tcr login` for each account you want in the pool. It refuses to run while a proxy
is holding the port, because the running server's next token refresh would overwrite what
the login just wrote — stop the server, log in, start it again, or pass `--force` if you
know what you are trading away.

Then point a client at it. Either export the environment yourself:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:3456          # base-URL mode
# or
export HTTPS_PROXY=http://127.0.0.1:3456                 # forward-proxy mode
export NODE_EXTRA_CA_CERTS=<the CA path tcr logs at boot>
```

Forward-proxy mode terminates TLS with a locally generated certificate, so the client has
to trust it. `tcr` prints the CA path to advertise when it starts — it reuses the Node
proxy's `~/.config/teamclaude-ca.pem` when that exists, and otherwise mints its own and
tells you where it put it.

or let `tcr` do it and launch Claude Code for you:

```sh
tcr run -- <args>
```

`tcr run` prefers forward-proxy mode: Claude Code decides whether you are a first-party
client by string-comparing `ANTHROPIC_BASE_URL`, so leaving that variable alone and
intercepting one layer down at `HTTPS_PROXY` keeps the client in its normal configuration.

## How it works

One TCP listener on `127.0.0.1:<port>` serves both entry modes. Which one you get is
decided by a non-destructive peek at the first eight bytes of each connection.

```
client                      tcr — ONE listener on 127.0.0.1:<port>
  |
  |-- peek first 8 bytes: "CONNECT "?
  |     no  -> BASE-URL MODE (cleartext h1 or h2 prior-knowledge) ---+
  |     yes -> parse CONNECT host:port, host on the allowlist?       |
  |             no  -> BLIND TCP BYTE TUNNEL: TLS never terminated,  |
  |                    nothing decrypted, nothing injected           |
  |             yes -> 200 Established, TLS-accept with our leaf ----+
  v
one axum router, identical for both modes
  |
  v  local routes: GET /_tcr/status | POST /_tcr/accounts/disabled
  v  everything else -> the forwarding handler
api-key gate -> path-shape gate -> /_tcr guard -> host guard (421)
  -> relay bypass for /v1/code and the OAuth file endpoints
  v
buffer the body once | parse the target model | derive the session key
  v
ROTATION LOOP (bounded)
  select an account -> take an in-flight slot -> refresh the token if it is
  expiring -> splice metadata.user_id.account_uuid -> drop the client's
  authorization and x-api-key, inject ours -> global rate limiter -> send
  401 / 429 / 529 / transport failure -> retry the same account, or rotate
  v
2xx: the response streams back verbatim; SSE chunks are tee'd to a usage parser
```

**Base-URL mode** is plain HTTP to the listener. **Forward-proxy mode** terminates TLS for
Anthropic's own API hosts using a locally generated leaf, which is why the client needs
your CA in `NODE_EXTRA_CA_CERTS`. Every other host is copied through as raw bytes and is
never decrypted — that is what keeps Claude Code's other endpoints working through a single
`HTTPS_PROXY`, and it is also a real property to understand before you run it; see
[Security](#security).

Account selection is not one sort. On the normal path, `tcr` picks the eligible account
minimising `(priority ascending, least-recently-selected, soonest weekly reset)`, so
`priority` is a hard tier and rotation breaks ties within it. Eligibility means: not
disabled, not in an error state, not under a live rate-limit hold, under the switch
threshold, and not blocked for this request's model class. Four things change that picture:
`lockAccount` short-circuits everything to one account, an honoured session-affinity pin is
served even over the utilization threshold, the pacing fallback ranks in-flight count above
priority, and `revalidationServe` (on by default) serves the least-utilized survivor rather
than returning an error when the whole fleet reads over the soft threshold.

Between requests, a quota probe keeps the bars fresh: one plain `GET` per account against
Anthropic's OAuth usage endpoint. It issues no `/v1/messages` call, so it creates no
messages.

**Each account is probed on its own randomly drawn schedule, not a fleet sweep.** Boot does
one whole-fleet pass so the bars populate immediately (sequentially, 350 ms apart), and from
then on every account sleeps a random 210-390 seconds — uniform around the
`quotaProbeSeconds` default of 300, plus or minus 30% — before its own next probe, with a
random initial offset drawn from `0..=300s` so a restart re-scatters the fleet instead of
re-aligning it. Nothing shares an instant, nothing runs on an exact period. This replaced a
75-second fleet-wide sweep that touched every account inside the same window, on the dot,
forever. Keep-warm (`warmupSeconds`) got the same treatment; it is the opposite of the probe
and is off by default — it really does post messages, and spends quota to do it.

## TcrBar (macOS)

`apps/macos` is a native front end over the same `tcr status --json` the TUI reads. The
menu-bar item is the whole app: no Dock icon, no window. The glyph carries fleet capacity
at a glance, and the dropdown carries the detail.

<p>
  <img src="assets/tcrbar-panel-healthy.png" alt="TcrBar panel with a healthy fleet" width="420">
  <img src="assets/tcrbar-panel-offline.png" alt="TcrBar panel showing an offline read" width="420">
</p>

Install the released app from the
[latest release page](https://github.com/dhkts1/teamclaude-rs/releases/latest) — the DMG is
version-stamped, so there is no stable "latest" download URL. If you already have the CLI,
`tcr ui` opens the app. To build and install it from this checkout:

```sh
apps/macos/scripts/install.sh                       # build, sign, install to /Applications
TCRBAR_DEV_BUILD=1 apps/macos/scripts/install.sh    # -> /Applications/TcrBar Dev.app
apps/macos/scripts/uninstall.sh                     # remove it
```

The dev build gets its own bundle id as well as its own path, and both matter: macOS keys
the menu-bar status item on the bundle id, and two processes registering the same one get
that id blacklisted by ControlCenter permanently, across reboots. The app's version is
derived from the repository's commit count, so a `--depth 1` clone cannot build it; the
build stops and tells you to run `git fetch --unshallow`.

Each row shows the account's quota bars, its countdown to the next reset, its probe health,
and an Enable/Disable button that shells out to `tcr` — the panel is a control surface, not
just a readout. Probe health is three states, not two: never probed reads `UNMEASURED`,
while a probe that ran and failed shows the probe's own word with the error in the tooltip.
The panel never renders a blank list, and it distinguishes `tcr` being missing, the poll
failing, an undecodable response, an empty fleet and an offline read from one another,
because an operator who cannot tell those apart will misread the panel. An offline read is
not a banner: the rows still render, with an inline notice above them.

<details>
<summary>The full fleet view (tall screenshot)</summary>

<img src="assets/tcrbar-panel-fleet.png" alt="TcrBar panel listing a full fleet of accounts" width="420">

</details>

It can also supervise the proxy. "Start server at launch" runs
`tcr server --headless --no-replace`; `--headless` is load-bearing, because without it the
child would try to start the TUI on a pipe and die immediately. An already-serving proxy is
left alone — that is `tcr`'s default now, and `--no-replace` is passed only to stay safe
against an older `tcr` on PATH. The flip side is that quitting TcrBar stops the server it
started, and the panel only says so once the box is ticked. "Take over port…" is the
deliberate exception: it replaces an incumbent proxy. That costs every live session its
prompt cache, and with `sessionAffinity` on it costs the pin map too — although pins are
flushed to disk continuously and restored at boot, so a takeover inside the 15-minute pin
TTL restores most of them and one outside it restores none.

There is a "Keep this Mac awake" checkbox, which takes three power assertions together. Two
honest limits, both of which the panel states: the display still sleeps, and sleep is only
held off on AC power.

TcrBar self-updates through [Sparkle](https://sparkle-project.org) from the `appcast.xml`
published on the latest GitHub release; use **Check for Updates…** in the panel footer, or
`open tcrbar://check-for-updates`, which is what `tcr update` hands off to when it finds the
app. One surprise worth planning around: you cannot replace `/Applications/TcrBar.app` while
the proxy is running, because TcrBar supervises the bundled `tcr` as a child and macOS sees
an executing image inside the bundle being swapped. Quitting TcrBar clears it — which means
every app update also restarts the proxy. Release mechanics, including local builds that can
self-update, are in [`docs/RELEASING.md`](docs/RELEASING.md).

## Configuration

Config lives at `~/.config/teamclaude.json`, written `0600`. It holds **working OAuth
credentials** for every account in the pool, so it never belongs in a commit — this
repository is public, and tokens, account emails and org UUIDs must stay out of code,
fixtures, tests and pull request descriptions.

`name` and `accessToken` are the only two required keys in the whole file. Everything else
has a default, so this is a complete, valid config:

```json
{
  "accounts": [
    { "name": "alice@example.com", "accessToken": "sk-ant-oat01-..." }
  ]
}
```

In practice you want `refreshToken` and `expiresAt` too, which is what `tcr login` writes,
so tokens are refreshed instead of expiring on you. A fuller shape:

```json
{
  "proxy": { "port": 3456 },
  "switchThreshold": 0.95,
  "quotaProbeSeconds": 300,
  "accounts": [
    {
      "name": "alice@example.com",
      "type": "oauth",
      "accessToken": "sk-ant-oat01-...",
      "refreshToken": "sk-ant-ort01-...",
      "expiresAt": 1893456000000,
      "priority": 0
    }
  ]
}
```

Those four values are the real defaults. `priority` is lowest-wins and defaults to 0, so
give backup accounts a higher number to hold them back until the primaries are near their
cap. `expiresAt` is epoch milliseconds.

### What is already on

Two features ship enabled, which is the opposite of what most readers assume, and neither
appears in a config you have not edited:

| Key | Default | What it does |
|---|---|---|
| `throttle` | `{"minSpacingMs": 350, "burst": 4}` | A fleet-wide rate limiter on the single upstream send site. |
| `revalidationServe` | `true` | When every account reads over the soft threshold, serve the least-utilized one instead of failing. |
| `quotaProbeSeconds` | `300` | Probe cadence — the CENTRE of a per-account random draw (`+/-30%`), not a fleet period; `0` or less disables probing entirely. |

`throttle` has an inversion nobody guesses: **an absent `throttle` key means ON, and
`"throttle": {}` means OFF.** The empty object is the documented escape hatch. This is the
only key in the file where absent and empty differ.

Off by default, opt in deliberately: `sessionAffinity` (pin a session to one account so its
prompt cache stays warm), `warmupSeconds` (keep idle accounts' windows alive — this one
spends real quota), `loadBalanceMigration` (re-pin a session off a stacked account),
`pacing` (per-account concurrency and spacing), and `lockAccount` (pin all traffic to one
account, with no failover and no rotation).

The full schema — all 29 keys, their types, defaults and the per-account overrides — is in
[`docs/configuration.md`](docs/configuration.md).

## CLI

| Command | What it does |
|---|---|
| `tcr` | Start the proxy with the live TUI. Same as `tcr server`. |
| `tcr server` | Start the proxy. `--headless` logs instead of drawing the TUI; `--port`, `--replace`. |
| `tcr run -- <args>` | Launch `claude` already pointed at the proxy. |
| `tcr login` | PKCE browser flow; writes tokens into the config. `--force` overrides the running-server refusal. |
| `tcr accounts` | List the fleet. `--probe` refreshes quota from the network first. |
| `tcr status` | Fleet status as greppable text, or `--json`. Reports the running build's SHA. |
| `tcr priority <query> [N]` | Set rotation order. `--first` / `--last` for relative moves. |
| `tcr enable <query>` | Return an account to the rotation, in the running proxy where there is one. |
| `tcr disable <query>` | Park an account, in the running proxy where there is one. |
| `tcr remove <query>` | Delete an account from the config. |
| `tcr update` | Update `tcr` itself. `--force` rebuilds an up-to-date checkout anyway. |
| `tcr ui` | Open TcrBar (macOS). |
| `tcr demo` | Render the TUI against fake accounts. Contacts nothing. |

Every command takes `--config <path>`; the account commands also take `--org`. Account
queries are an exact name match falling back to an exact email match, and they are
case-sensitive: `alice` does not match `alice@example.com`, and neither does
`ALICE@EXAMPLE.COM`.

Full flag reference: [`docs/cli.md`](docs/cli.md).

## Troubleshooting

**"The fix is in main" and "the fix is in the process serving traffic" are different
facts.** The running proxy can be several commits behind your checkout. `tcr status --json`
reports the SHA of the build that is actually serving; check it before concluding a change
is live.

**Where the logs are.** `tcr server --headless` writes to stdout *and* to a daily-rotating
file under `~/.cache/teamclaude/logs/`. The file is the half that survives supervision —
TcrBar discards the child's stdout — so that directory is where to look when a supervised
proxy misbehaves.

**The port is already in use.** By default a second `tcr` stands down rather than killing
the incumbent, because two proxies sharing one config both hold the same single-use refresh
tokens, and the first refresh by either revokes the other's. `tcr server --replace` takes the
port deliberately. Prefer standing down: a restart costs every live session its prompt cache,
which is per-account at Anthropic's end and the most expensive event in this system.

**A restart is not automatically total loss.** With `sessionAffinity` on, pins are persisted
and restored at boot within a 15-minute TTL; a restart inside that window keeps most sessions
warm, and one outside it restores nothing. The server logs how many pins it restored at boot
— read that line rather than assuming either outcome. With `sessionAffinity` off (the
default) there are no pins to restore.

**An account is disabled in the config but still getting traffic.** `tcr enable` and
`tcr disable` ask the running server first and fall back to the file only when nothing is
listening. Against a proxy too old for that route, the CLI warns that the change will not
take effect until the server restarts. Believe the warning.

**503 with `Retry-After: 5`.** Name resolution is failing, which means this machine is
offline. No account was rotated and no account was blamed; retry. That is deliberately not a
502, which would point you at the upstream for a local fault.

**`tcr login` refuses to run.** A proxy is holding the port and its next token refresh would
overwrite what the login writes. Stop it, log in, start it again — or use `--force`.

## Security

Read this before running it, not after.

**Bind scope is not authorization.** `tcr` binds `127.0.0.1`, but loopback is reachable by
every process and every container on the host, so binding it is not a claim about who is
calling. The forwarding path exempts loopback from the api-key gate; the two `/_tcr/` control
routes are deliberately stricter and require the configured `proxy.apiKey` with **no**
loopback exemption, on top of proving the peer is loopback from the socket, requiring a
loopback `Host` header, refusing cross-site requests and requiring `application/json`.
Nothing generates a `proxy.apiKey` for you and it defaults to unset — so on a default
install, being on this host is the whole gate. Set one if that is not the boundary you want.

**The forward proxy is an open tunnel by design.** The MITM allowlist is three hosts:
`api.anthropic.com`, `console.anthropic.com` and `platform.anthropic.com`. Which of them are
actually TLS-terminated depends on the leaf certificate in use — a leaf reused from the Node
proxy carries a SAN for `api.anthropic.com` only, while the leaf `tcr` mints on a fresh
install covers all three, and on that install all three terminate and receive a pooled
Bearer token. Every other `CONNECT` target is copied through as raw bytes: never decrypted,
and never filtered either. That makes `tcr` an unrestricted forward proxy to any host for
any local process. It is intentional — Claude Code needs its other endpoints through the same
`HTTPS_PROXY` — but it is a pivot surface, and the design note in `src/mitm.rs` says plainly
that this is not a firewall. A plain-HTTP request naming another host is a different case and
is refused locally with `421`.

**Credentials.** The client's own `authorization` and `x-api-key` headers are dropped before
the pooled Bearer is injected, so a client credential is never forwarded alongside ours. The
config, the leaf private key, the session-affinity pin file and the singleton owner file are
all written `0600` through the same atomic write path. Three request paths deliberately relay
the client's own credential without rotating: `/v1/code` and the two OAuth file endpoints.

**The quota probe** issues a plain `GET` to Anthropic's OAuth usage endpoint and never a
`/v1/messages` call, so it creates no messages. What Anthropic meters on its side is their
business, not something this repository can assert.

**Self-update.** `tcr update` downloads the published installer over TLS and nothing else;
there is no checksum asset to verify against. Saying so explicitly, because a document that
lists every other defence makes silence read as "handled".

**Secret scanning.** `.githooks/pre-commit` runs `gitleaks git --staged` against
`.gitleaks.toml`, along with a private-disclosure scan, format gates and a release-version
gate. It requires `gitleaks` on PATH. Enable the hooks after cloning:

```sh
git config core.hooksPath .githooks
```

Treat it as a backstop rather than the plan; it only sees what you stage, and `--no-verify`
exists.

If you find a security issue, please open a private report through GitHub's security
advisories rather than a public issue.

## Contributing

Bug reports and pull requests are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for
development setup, the test and lint gates, and what `main` requires. Contributors should
also read [`CLAUDE.md`](CLAUDE.md), which records the things this project learned expensively.

## License

MIT — see [`LICENSE`](LICENSE). This is a Rust rewrite of
[KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) (MIT); the original's
copyright and license are preserved in [`NOTICE`](NOTICE).
