<div align="center">

<img src="assets/tcrbar-icon.png" alt="" width="116">

# teamclaude-rs (`tcr`)

A rotating Anthropic proxy that lives in your menu bar.

Point Claude Code — or any Anthropic API client — at it, and it spreads requests across
several Claude accounts, refreshes their OAuth tokens, and shows what each one has left.

[![CI](https://github.com/dhkts1/teamclaude-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/dhkts1/teamclaude-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-stable-orange.svg)

[Install](#install) · [Usage](#usage) · [Configuration](docs/configuration.md) · [CLI](docs/cli.md) · [Security](#security)

<img src="assets/tcrbar-panel-healthy.png" alt="TcrBar menu-bar panel showing a healthy fleet" width="400">

</div>

## Key Features

- Rotates requests across a pool of Claude accounts, with `priority` as a hard tier.
- Refreshes OAuth tokens in the background, so accounts do not expire out from under you.
- Native macOS menu-bar app: live quota bars, per-account enable/disable, server supervision.
- Two entry modes on one port — base-URL and forward-proxy — chosen per connection.
- A live terminal dashboard, on macOS and Linux alike, showing everything the app shows.
- Session affinity keeps a conversation on one account, so its prompt cache stays warm.
- Drop-in for the Node [teamclaude](https://github.com/KarpelesLab/teamclaude): same config, certs and port.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dhkts1/teamclaude-rs/releases/latest/download/teamclaude-rs-installer.sh | sh
```

Or from source, with a Rust toolchain — use the script rather than `cp`, because [`cp` onto
a running binary is a same-inode rewrite macOS punishes](CONTRIBUTING.md#installing-it-onto-your-path):

```sh
cargo build --release
scripts/install-cli.sh          # places `tcr` at ~/.local/bin/tcr by default
```

| Platform | `tcr` CLI | TcrBar menu-bar app |
|---|---|---|
| macOS, Apple silicon (`aarch64-apple-darwin`) | prebuilt | yes |
| macOS, Intel (`x86_64-apple-darwin`) | prebuilt | yes |
| Linux x86_64, musl (`x86_64-unknown-linux-musl`) | prebuilt | — |
| Linux aarch64, musl (`aarch64-unknown-linux-musl`) | prebuilt | — |
| Anything else | build from source | — |

Linux builds are static musl, so Alpine and glibc both work; TcrBar and `tcr ui` are macOS-only.

## Usage

```sh
tcr login          # PKCE browser flow, once per account you want in the pool
tcr                # start the proxy with the live TUI (q quits)
tcr run -- <args>  # launch Claude Code already pointed at the proxy
```

To point a client yourself instead of using `tcr run`:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:3456   # base-URL mode
# or
export HTTPS_PROXY=http://127.0.0.1:3456          # forward-proxy mode
export NODE_EXTRA_CA_CERTS=<the CA path tcr logs at boot>
```

Forward-proxy mode terminates TLS with a locally generated certificate, so the client has to
trust it; `tcr` prints the CA path to advertise when it starts. Config lives at
`~/.config/teamclaude.json`, where `name` and `accessToken` are the only required keys —
every other key, its default and the file's permissions are in
[`docs/configuration.md`](docs/configuration.md).

## Menu bar app

`apps/macos` is a native front end over the same `tcr status --json` the TUI reads. The
menu-bar item is the whole app: no Dock icon, no window. The glyph carries fleet capacity at
a glance; each row carries quota bars, a reset countdown, probe health, and an
Enable/Disable button that shells out to `tcr` — a control surface, not just a readout. It
can also supervise the proxy, and self-updates through [Sparkle](https://sparkle-project.org).

Install it from the [latest release](https://github.com/dhkts1/teamclaude-rs/releases/latest), or run
`tcr ui`. Build it here with `apps/macos/scripts/install.sh`; releases: [`docs/RELEASING.md`](docs/RELEASING.md).

The panel never renders a blank list, and it tells `tcr` being missing, a failing poll, an
empty fleet and an offline read apart. An operator who cannot tell those apart misreads it.

<p>
  <img src="assets/tcrbar-panel-offline.png" alt="TcrBar panel showing an offline read" width="400">
  <img src="assets/tcrbar-panel-no-capacity.png" alt="TcrBar panel with no capacity left" width="400">
</p>

<details>
<summary>The full fleet view, and the terminal dashboard</summary>

<img src="assets/tcrbar-panel-fleet.png" alt="TcrBar panel listing a full fleet of accounts" width="400">

![tcr live TUI](assets/tui-demo.gif)

`tcr demo` renders the real TUI against fake accounts — how these screenshots were made. It
needs no config and contacts nothing.

</details>

## How it works

One TCP listener serves both entry modes, decided by a non-destructive peek at the first
eight bytes of each connection: a `CONNECT` for an Anthropic API host is TLS-terminated with
a locally generated leaf, anything else is copied through as raw bytes, and plain HTTP is
base-URL mode. Requests then run a bounded rotation loop — pick an eligible account, refresh
its token if it is expiring, swap the client's credentials for the pooled one, send, and
rotate on a 401, 429, 529 or transport failure. The request-flow diagram, the account
selection ordering and the probe schedule are in [`docs/architecture.md`](docs/architecture.md).

## Documentation

| Document | What is in it |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | Every config key, its type, default and source citation. |
| [`docs/cli.md`](docs/cli.md) | Every command and flag, exit codes, account resolution. |
| [`docs/architecture.md`](docs/architecture.md) | Request flow, entry modes, account selection, quota probes. |
| [`docs/troubleshooting.md`](docs/troubleshooting.md) | Symptoms and what they mean. |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Development setup, test and lint gates, what `main` requires. |

## Security

Read this before running it, not after.

**Bind scope is not authorization.** `tcr` binds `127.0.0.1`, but loopback is reachable by
every process and container on the host. The forwarding path exempts loopback from the
api-key gate, and nothing generates a `proxy.apiKey` for you — so on a default install being
on this host is the whole gate. Set one if that is not the boundary you want.

**The forward proxy is an open tunnel by design.** The MITM allowlist is three hosts:
`api.anthropic.com`, `console.anthropic.com` and `platform.anthropic.com`. Every other
`CONNECT` target is copied through as raw bytes — never decrypted, never filtered — which
makes `tcr` an unrestricted forward proxy to any host for any local process. That is
intentional, and it is not a firewall; which hosts actually terminate depends on the leaf
certificate in use, which [`MITM-DESIGN.md`](MITM-DESIGN.md) works through.

**Credentials.** The client's own `authorization` and `x-api-key` headers are dropped before
the pooled Bearer is injected, so a client credential is never forwarded alongside ours.
`git config core.hooksPath .githooks` enables a pre-commit secret scan and the other gates
listed in [CONTRIBUTING.md](CONTRIBUTING.md#git-hooks) — a backstop, not the plan, since it
only sees what you stage and `--no-verify` exists.

Found a security issue? Please open a private report through GitHub's security advisories
rather than a public issue.

## Credits and license

MIT — see [`LICENSE`](LICENSE). This is a from-scratch Rust rewrite of the Node proxy
[KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) (MIT); the original's
copyright and license are preserved in [`NOTICE`](NOTICE). Contributors should also read
[`CLAUDE.md`](CLAUDE.md), which records the things this project learned expensively.
