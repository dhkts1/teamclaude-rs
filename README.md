# teamclaude-rs (`tcr`)

[![CI](https://github.com/dhkts1/teamclaude-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/dhkts1/teamclaude-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-stable-orange.svg)

A lean, single-user **rotating Anthropic proxy** in Rust. Point your Claude Code (or
any Anthropic API client) at it and it spreads requests across several Claude
accounts, refreshes their OAuth tokens automatically, and shows a live TUI with
per-account quota and request counts.

It's a from-scratch Rust rewrite of the Node proxy [teamclaude](https://github.com/KarpelesLab/teamclaude) — same on-disk config, same certs,
so it's a drop-in on the same port.

![tcr live TUI](assets/tui-demo.gif)

> The dashboard above is `tcr demo` — the real TUI rendered with fake accounts. Run it yourself with `tcr demo`.

## What it does

- **Load-balances** across your accounts: least-recently-selected rotation, so no
  single account gets hammered. Priority tiers are respected (keep "pillow"
  accounts as last resort).
- **Two entry modes on one port:**
  - **Base-URL:** set `ANTHROPIC_BASE_URL=http://127.0.0.1:3456`.
  - **Forward-proxy (MITM):** set `HTTPS_PROXY=http://127.0.0.1:3456` +
    `NODE_EXTRA_CA_CERTS=<ca.pem>`. Only `api.anthropic.com` is intercepted and
    token-injected; every other host is blind-tunneled, so Claude Code's other
    endpoints keep working.
- **Zero-spend quota probe:** reads each account's usage from the OAuth usage
  endpoint (no message quota spent) so the bars stay fresh even when idle.
- **Honest live TUI:** per-account status, 5h / 7d quota bars, probe health,
  request counts, and a recent-request log. Near-limit accounts read as
  "near"/"full" — never a false "error".
- **Localhost only:** binds `127.0.0.1`, so local clients need no API key.

## Build & install

```sh
cargo build --release
scripts/install-cli.sh          # put `tcr` on PATH (~/.local/bin/tcr by default)
```

## Configure

Config lives at `~/.config/teamclaude.json`. It holds **working OAuth credentials** for every account
in the pool, so it never belongs in a commit — this repository is public, and its contents (tokens,
account emails, org UUIDs) must stay out of code, fixtures, tests and PR descriptions. Contributors:
see [`CLAUDE.md`](CLAUDE.md).

The shape:

```json
{
  "proxy": { "port": 3456 },
  "switchThreshold": 0.90,
  "quotaProbeSeconds": 75,
  "accounts": [
    {
      "name": "you@example.com",
      "type": "oauth",
      "accessToken": "sk-ant-oat01-...",
      "refreshToken": "sk-ant-ort01-...",
      "expiresAt": 1893456000000,
      "priority": 0
    }
  ]
}
```

`priority` is lowest-wins (default 0); give backup accounts a higher number so
they're used only when the primaries are near their cap.

> **Adding accounts:** run `tcr login` — it walks you through Anthropic's OAuth
> browser flow (PKCE) and writes the resulting tokens straight into the config.
> You can also paste existing OAuth tokens in by hand. Either way the file is
> written `0600`; never commit it.

## Run

```sh
tcr                 # start the proxy with the live TUI (q to quit)
tcr server --headless   # run in the background, log to stdout
tcr run -- <args>   # launch `claude` already pointed at the proxy
```

Then either export `ANTHROPIC_BASE_URL` / `HTTPS_PROXY` as above, or use `tcr run`.

## TcrBar — the menu bar app (macOS)

`apps/macos` is a native front end over the same `tcr status --json` the TUI
reads. The menu-bar item is the whole app — no Dock icon, no window
(`LSUIElement`). The glyph carries fleet capacity at a glance; the dropdown
carries the detail: per-account quota bars, whether each account is in the
rotation, the countdown to the next reset on a held account, and probe health.

It polls every 3 seconds and never renders a blank list. `tcr` missing, the poll
failing (usually: no server), and an offline read whose counters are
structurally zero are three different facts, and each gets its own banner — an
operator who cannot tell them apart will misread the panel.

It can also supervise the proxy. "Start server at launch" runs
`tcr server --no-replace`, so a proxy that is already serving is never
disturbed; the flip side is that quitting TcrBar then stops the server it
started, which the footer says before you tick the box. "Take over port…" is the
deliberate exception — it replaces an incumbent proxy, which wipes the
session-to-account pin map — so it sits below its own rule, away from the row
your hand is already in, and asks first.

```sh
apps/macos/scripts/install.sh     # build, sign, install to /Applications, launch
apps/macos/scripts/uninstall.sh   # remove it
```

To try a local build without losing the one you use, install it as a separate
app:

```sh
TCRBAR_DEV_BUILD=1 apps/macos/scripts/install.sh   # -> /Applications/TcrBar Dev.app
```

That gives the dev build its own bundle id as well as its own path, and both
matter: macOS keys the menu-bar status item on the bundle id, and two processes
registering the same one get that id blacklisted by ControlCenter — permanently,
across reboots.

### Design tokens

The palette is generated, not picked by eye. `scripts/tcrbar-palette.py` authors
it in OKLCH, converts to sRGB, rejects anything out of gamut, and measures WCAG
contrast against the surface each colour is actually drawn on — in all four
appearances (light, dark, and an increased-contrast variant of each). A token
that fails is a build error rather than a matter of taste.

The same generator emits the palette for anything designed outside the app:

```sh
python3 scripts/tcrbar-palette.py \
  --emit-css apps/macos/design-tokens/tokens.css \
  --emit-json apps/macos/design-tokens/tokens.json
```

Colour comes from the generator; the spacing ramp, radii and type sizes are read
out of `Tokens.swift`, where they are authored. Neither is copied, so neither
can drift. Committing a stale copy is blocked by the pre-commit hook.

## Updating

The CLI and the app update by different mechanisms, and only one of them is
automatic.

TcrBar self-updates through [Sparkle](https://sparkle-project.org). Use
**Check for Updates…** in the panel footer, or `open tcrbar://check-for-updates`
— the CLI can trigger the same user-initiated check that way. Background checks
stay quiet when there is nothing to install; a check you asked for reports
either way. The feed is the `appcast.xml` published on the latest GitHub
release, and Sparkle orders releases on `CFBundleVersion`, which this project
derives from the commit count.

A build can only *install* an update if it was built with the Sparkle public key
in its `Info.plist`. Local builds omit it unless you say otherwise, and then
Sparkle will check the feed and refuse to install what it downloads — that is
deliberate, since a placeholder key would fail the same way while looking
configured. If you want a local build that can still self-update:

```sh
export TCRBAR_SPARKLE_PUBLIC_KEY=1WZWEwzSEijRarey7qE0a+n4AO/+7e4Fj/nW8Y8ZKMM=
```

Publishing that key is the point of it — it verifies downloads, it does not sign
them. Cutting a release is a separate, local, credentialled process; see
[`docs/RELEASING.md`](docs/RELEASING.md).

## Feature status

Everything the Node original did (and a couple it didn't) is implemented: OAuth
`login`, per-model (Fable-aware) routing, the account CLI (`accounts` / `remove` /
`priority` / `enable` / `disable` / `status`), `update`, keep-warm, and session
affinity. Three are opt-in via `~/.config/teamclaude.json`, off by default:

- `"sessionAffinity": true` — pin a client session to one account for its
  lifetime. **Anthropic's prompt cache is per-account**, so per-request rotation
  gives every turn a cold cache; affinity keeps a session's cache warm on its
  account while different sessions still spread across accounts.
- `"warmupSeconds": <n>` — periodically warm idle accounts so their 5-hour window
  stays active. This one *spends real quota*, so enable it deliberately.
  Keep-warm never warms an account whose quota it has not actually read, so it
  does nothing until the first probe sweep has reported.
- `"loadBalanceMigration": true` — re-pin a session off an account that several
  sessions stack on, onto a less-loaded one. Off by default for the same
  per-account-cache reason: a session that has a pin is already warm, so every
  such move re-creates its whole conversation prefix on the target. A session's
  account is chosen at start, or when its pin fails a hard gate — not to even
  out counts.

## Security

- Binds `127.0.0.1` only; the forward-proxy tunnel is reachable solely by the
  local user (no wider than the shell itself).
- Only `api.anthropic.com` is TLS-terminated; all other hosts are pass-through
  byte tunnels (never decrypted).
- Config and leaf key are `0600`. No secrets belong in this repo.
- **Secret scanning:** a `.githooks/pre-commit` hook runs `gitleaks git --staged`
  (config: `.gitleaks.toml`) so no secret reaches a commit. Enable it after
  cloning with `git config core.hooksPath .githooks` (needs `gitleaks` on PATH).

## License

MIT — see [`LICENSE`](LICENSE). This is a Rust rewrite of
[KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) (MIT); the
original's copyright and license are preserved in [`NOTICE`](NOTICE).
