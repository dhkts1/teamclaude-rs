# tcr MITM / forward-proxy "backwards mode"

Goal: make `tcr` a drop-in for the JS teamclaude's `HTTPS_PROXY` forward-proxy mode, so Gil's
existing terminals (which export `HTTPS_PROXY=http://127.0.0.1:<port>` +
`NODE_EXTRA_CA_CERTS=~/.config/teamclaude-ca.pem`) work against `tcr` unchanged. Reference impl:
`~/git/teamclaude/src/mitm.js` + `src/x509.js`. Reuse `tcr`'s existing account-selection + token
injection — only the CONNECT + TLS-termination front-end is new.

## How it works
A client with `HTTPS_PROXY` set sends `CONNECT api.anthropic.com:443` to us. Instead of blind
byte-tunneling, we terminate the client's TLS ourselves (presenting a leaf cert the client trusts
via its CA), read the plaintext HTTP request, run it through the SAME proxy path as base-URL mode
(select account → inject `authorization: Bearer <account token>` → forward to the real
api.anthropic.com over our own TLS client → stream back).

## Cert reuse (the key win — no re-trusting)
- `~/.config/teamclaude-leaf.pem` (leaf cert, SAN `api.anthropic.com`, valid to 2028) +
  `~/.config/teamclaude-leaf.key` (PKCS#8) already exist and are signed by the CA Gil's clients
  trust. **Load these into a rustls `ServerConfig`** and present them. tcr does NOT need the CA
  private key (the leaf is already signed).
- If the leaf is missing/expired/doesn't cover the host: fall back to generating a fresh CA+leaf
  with `rcgen`, persist `~/.config/tcr-ca.pem` + `tcr-leaf.pem`/`.key` (leaf key 0600), and print
  `NODE_EXTRA_CA_CERTS=~/.config/tcr-ca.pem` for the user to trust. (Primary path is reuse.)

## Security (what shipped — this section was written as a plan and the plan changed)
- **Three hosts are MITM-terminated**, not one: `api.anthropic.com`, `console.anthropic.com` and
  `platform.anthropic.com` (`ALLOWED_HOSTS`, `src/mitm.rs:49-53`). The OAuth hosts are on the
  allowlist unconditionally, not "if needed". Which of the three completes a handshake depends on
  the leaf: the reused `teamclaude-leaf.pem` has a SAN for `api.anthropic.com` only
  (`src/mitm.rs:45-48`), so on the reuse path that is the one host in practice; the rcgen fallback
  leaf minted on a fresh install carries SANs for all three plus the JS test host `www.example.org`
  (`src/mitm.rs:57-62`), and terminates all three.
- **Every other `CONNECT` target is blind-tunneled** — a raw TCP byte-pipe to `<host>:<port>` with
  TLS left untouched, so we decrypt nothing and inject nothing (`src/mitm.rs:445-451`, `tunnel` at
  `:475-501`). This section originally specified `403` and close, and refusing to replicate the JS
  `tunnel` mode; the open tunnel is what shipped, because Claude Code depends on it to reach
  `platform.claude.com`, its bridge and telemetry through the same `HTTPS_PROXY`. The SSRF/pivot
  cost is bounded by the bind, not by an allowlist: `tcr` listens on `127.0.0.1` only, so the
  tunnel is reachable solely by the local user, who can already open any connection directly — no
  wider a surface than their own shell.
- Three `CONNECT` carve-outs answer instead of tunneling: a malformed request line → `400`
  (`src/mitm.rs:439-443`); an allowlisted host with no TLS material loaded → `503`
  (`src/mitm.rs:454-459`); and a tunnel whose upstream connect fails → `503` (`src/mitm.rs:483-491`).
- The same x-api-key gate as base-URL mode applies to the decrypted requests.

## Where it plugs in
- New `src/mitm.rs`. The server listener becomes hybrid: peek the first request line on each
  accepted TCP connection.
  - `CONNECT <host>:<port>` → MITM path (if host allowed): reply
    `HTTP/1.1 200 Connection Established\r\n\r\n`, `tokio_rustls::TlsAcceptor::accept` with the leaf
    config, then serve HTTP/1.1 over the TLS stream (hyper server conn) with a service that calls the
    existing proxy handler (share `Arc<Manager>`).
  - `CONNECT` to a host NOT on the allowlist → blind tunnel, not a refusal (see Security above).
  - anything else → the existing axum/base-URL path (unchanged).
- Keep base-URL mode working simultaneously (dual): a terminal using `ANTHROPIC_BASE_URL` and one
  using `HTTPS_PROXY` both work against the same `tcr` port.
- CRITICAL: the OUTBOUND client to the real api.anthropic.com must keep `.no_proxy()` (already fixed
  in manager/oauth/probe) — do NOT route our own upstream back through an ambient `HTTPS_PROXY`.

## Acceptance (must prove end-to-end, not just compile)
- `curl --proxy http://127.0.0.1:<port> --cacert ~/.config/teamclaude-ca.pem -H "x-api-key: <master>"
  -H 'anthropic-version: 2023-06-01' -d '{"model":"claude-haiku-4-5","max_tokens":10,"messages":[{"role":"user","content":"Reply with exactly: mitm works"}]}'
  https://api.anthropic.com/v1/messages` → **200 with a real completion**, served by a pooled
  account. (This is the real test — base-URL mode "compiled but didn't proxy" until a live request
  found the proxy-loop bug.)
- `curl --proxy http://127.0.0.1:<port> https://example.com/` → a **normal end-to-end response from
  the real host**, presenting the real host's own certificate: a non-allowlisted target is
  blind-tunneled, not refused. This line asked for a `403` while the acceptance above was being
  written, and never matched the code (`src/mitm.rs:445-451`).
- base-URL mode still returns a real completion (no regression).
- Never bind 3456 in tests; unit-test the CONNECT parse + host-allowlist + cert load without network.

## Ergonomics (fold in if quick, else a follow-up)
- `tcr run [-- claude args]`: if a `tcr` server is up on the config port, exec `claude` with
  `HTTPS_PROXY=http://127.0.0.1:<port>` + `NODE_EXTRA_CA_CERTS=<caPath>` set (mirrors `teamclaude run`,
  so `alias claude='tcr run --'` works). If the server is down, exec `claude` directly (passthrough).
