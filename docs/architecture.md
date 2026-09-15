# How `tcr` works

The request path, the two entry modes, how an account gets picked, and how quota is kept
fresh. Every tunable named here has its type, default and source citation in
[`configuration.md`](configuration.md); this document describes the mechanism rather than
the values.

## One listener, two entry modes

One TCP listener on `127.0.0.1:<port>` serves both entry modes. Which one you get is
decided by a non-destructive peek at the first eight bytes of each connection.

```
client                      tcr: ONE listener on 127.0.0.1:<port>
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
never decrypted. That is what keeps Claude Code's other endpoints working through a single
`HTTPS_PROXY`, and it is also a real property to understand before you run it; see
[Security](../README.md#security).

`tcr run` prefers forward-proxy mode: Claude Code decides whether you are a first-party
client by string-comparing `ANTHROPIC_BASE_URL`, so leaving that variable alone and
intercepting one layer down at `HTTPS_PROXY` keeps the client in its normal configuration.

## Account selection

Account selection is not one sort. On the normal path, `tcr` picks the eligible account
minimising `(priority ascending, least-recently-selected, soonest weekly reset)`, so
`priority` is a hard tier and rotation breaks ties within it. Eligibility means: not
disabled, not in an error state, not under a live rate-limit hold, under the switch
threshold, and not blocked for this request's model class. Four things change that picture:
`lockAccount` short-circuits everything to one account, an honoured session-affinity pin is
served even over the utilization threshold, the pacing fallback ranks in-flight count above
priority, and `revalidationServe` (on by default) serves the least-utilized survivor rather
than returning an error when the whole fleet reads over the soft threshold.

Each of those keys (`lockAccount`, `sessionAffinity`, `pacing`, `revalidationServe`, and
the switch threshold itself) is documented with its default in
[`configuration.md`](configuration.md).

### Session-affinity pins survive a restart

With `sessionAffinity` on, the session-to-account pin map is flushed to disk continuously
and reloaded at boot, so a restart does not automatically cold-start every conversation's
prompt cache. Pins expire against `affinity::PIN_TTL_MS`, **15 minutes**: a restart inside
that window restores most of the fleet's pins, and one outside it restores none. The server
logs how many it restored; read that line rather than assuming either outcome. With
`sessionAffinity` off — the explicit opt-out, no longer the default — there are no pins to
restore.

## Keeping the quota bars fresh

Between requests, a quota probe keeps the bars fresh: one plain `GET` per account against
Anthropic's OAuth usage endpoint. It issues no `/v1/messages` call, so it creates no
messages.

**Each account is probed on its own randomly drawn schedule, not a fleet sweep.** Boot does
one whole-fleet pass so the bars populate immediately, and from then on every account
sleeps for an independently drawn interval, centred on the configured cadence, before its
own next probe, with a random initial offset so a restart re-scatters the fleet instead of
re-aligning it. This replaced a 75-second fleet-wide sweep that touched every account
inside the same window, on the dot.

Keep-warm (`warmupSeconds`) got the same treatment; it is the opposite of the probe and is
off by default: it really does post messages, and spends quota to do it.

The distributions, the bounds each draw actually produces, and the generator behind them
are in [Both cadences are random per account, not a fleet
sweep](configuration.md#both-cadences-are-random-per-account-not-a-fleet-sweep).

## When every account is out

If the rotation loop finds nothing eligible, `tcr` answers the client with a 429 carrying a
`retry-after`. The number on that header is deliberately **not** the real wait. A quota
window can be sixteen hours from resetting, and a real incident produced advertised waits of
57,950 to 59,975 seconds, but Claude Code ignores any `retry-after` above 60 seconds and
exits instead of sleeping. Advertising the true figure therefore killed the run outright. The
header is capped at 59 seconds; the real wait is still stated in the response body, and both
values are logged.

The cap is 59 rather than 60 because `exhaustion_hint` already uses exactly 60 to mean "no
account advertises a reset at all". Keeping the cap one second below that leaves the two
readable apart: anything at or under 59 came from a measured reset instant, and 60 on the
wire can only be the sentinel. This matters to a log reader, to an operator, and to a `tcr`
chained behind another `tcr`.

Measured against a stub endpoint on 2026-09-15 with client 2.1.272, two runs per value:

| advertised `retry-after` | what the client does |
|---|---|
| 30s | sleeps 30s, retries, exits 0 |
| 59s | sleeps 59s, retries, exits 0 |
| 60s | sleeps 60s, retries, exits 0 |
| 61s and above | never retries, exits 1 in about a second |

The cap holds across repeated refusals, which is what makes it a fix rather than a delay.
Against an endpoint that answered every single request with the same 59, the client kept
sleeping and re-asking on an exact 59.0-second cadence for seven attempts across six minutes,
and was still going when the test harness timed out. It never gave up on its own. So a
genuinely hours-long window no longer ends the run at the first refusal: the client waits
until some account frees, which is what the body text always claimed would happen.

Raising the cap above 60 would restore the immediate death. Lowering it only costs extra
round trips.
