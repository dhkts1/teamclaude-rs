# teamclaude-rs (`tcr`)

A lean, single-user **rotating Anthropic proxy** in Rust. Point your Claude Code (or
any Anthropic API client) at it and it spreads requests across several Claude
accounts, refreshes their OAuth tokens automatically, and shows a live TUI with
per-account quota and request counts.

It's a from-scratch Rust rewrite of the Node proxy [teamclaude](https://github.com/KarpelesLab/teamclaude) — same on-disk config, same certs,
so it's a drop-in on the same port.

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
ln -sfn "$PWD/target/release/tcr" ~/.local/bin/tcr   # put `tcr` on PATH
```

## Configure

Config lives at `~/.config/teamclaude.json`:

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

## Feature status

Everything the Node original did (and a couple it didn't) is implemented: OAuth
`login`, per-model (Fable-aware) routing, the account CLI (`accounts` / `remove` /
`priority` / `enable` / `disable` / `status`), `update`, keep-warm, and session
affinity. Two are opt-in via `~/.config/teamclaude.json`, off by default:

- `"sessionAffinity": true` — pin a client session to one account for its
  lifetime. **Anthropic's prompt cache is per-account**, so per-request rotation
  gives every turn a cold cache; affinity keeps a session's cache warm on its
  account while different sessions still spread across accounts.
- `"warmupSeconds": <n>` — periodically warm idle accounts so their 5-hour window
  stays active. This one *spends real quota*, so enable it deliberately.

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
