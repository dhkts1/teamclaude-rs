# Long-lived OAuth tokens

Measured 2026-09-12 against the live endpoints, not inferred. Every number below sits
beside the observation that produced it. Five authorization round trips were spent
establishing it; do not re-derive it by guessing.

## The finding

`POST https://platform.claude.com/v1/oauth/token` with `grant_type=authorization_code`
accepts a client-supplied `expires_in` in the JSON body and honours it.

| request | granted `expires_in` |
|---|---|
| scope `user:inference`, no `expires_in` sent | `28800` (8 h) |
| scope `user:inference`, `expires_in: 31536000` | `31536000` (365 d) |
| scope `user:inference user:profile user:file_upload`, `expires_in: 31536000` | `31536000` (365 d) |
| all six of tcr's current scopes, `expires_in: 31536000` | HTTP 400 (below) |
| `grant_type=refresh_token`, `expires_in: 31536000` | `28800`, parameter ignored |

The 400 names its own cause, which is what makes this searchable rather than guesswork:

```json
{"error": "invalid_request",
 "error_description": "Custom expires_in not allowed for scope 'user:mcp_servers'"}
```

So the restriction is per-scope and the server enumerates the offending scope one at a
time. `user:inference`, `user:profile` and `user:file_upload` are all compatible with a
custom lifetime. `user:mcp_servers` is not. `org:create_api_key` and
`user:sessions:claude_code` were not tested individually, only as part of the failing six.

## Why this matters more than the lifetime

Refresh tokens rotate on every use and carry an ABSOLUTE expiry that refreshing does not
extend. Two readings of `refresh_token_expires_in` thirty seconds apart returned `2497155`
then `2497125`, a difference of exactly 30. So the deadline is fixed at roughly 29 days
from the browser login, and no amount of refreshing moves it.

That is the treadmill: every account needs a browser re-login about monthly, for ever.
`~/.cache/teamclaude/logs` recorded 59 `refresh token rejected - re-login needed` lines
over three days, concentrated on three of the eighteen accounts (26, 24 and 9 rejections).

A one-year access token obtained at login removes the need to refresh at all, which makes
the 29-day refresh wall irrelevant for a year at a time.

## A reduced scope set loses nothing tcr uses

A 365-day token granted `user:file_upload user:inference user:profile` was used for a real
`POST /v1/messages` (`claude-haiku-4-5-20251001`, `max_tokens: 1`): HTTP 200 with a real
completion, and every quota header `Quota::update_from_headers` consumes was present:

```
anthropic-ratelimit-unified-5h-status: allowed          5h-utilization: 0.0
anthropic-ratelimit-unified-7d-status: allowed          overage-status: rejected
anthropic-ratelimit-unified-representative-claim: five_hour
anthropic-ratelimit-unified-5h-reset / 7d-reset: present
```

`rate_limit_tier` is NOT in the token response, so `fetch_profile` is still required for
it. `user:profile` is retained precisely so that keeps working.

## Identity arrives free at exchange time

The token-endpoint response carries identity even for `scope=user:inference` alone:

```json
"account":      { "uuid": "...", "email_address": "alice@example.com" }
"organization": { "uuid": "...", "name": "acme-corp" }
```

This contradicts the premise stated in `login_with_token`'s doc comment and in
`docs/` notes elsewhere, that an inference-only credential has no identity. That is true
of `/api/oauth/profile`, which does need `user:profile`, and false of the token response.
tcr's `unnamed` / `unnamed-N` fallback exists to paper over a problem that does not exist
on this path. Note `email + "/" + organization.name` is exactly the existing row-naming
convention (`alice@example.com/acme-corp`).

## The endpoint 429s clients it does not recognise

An otherwise valid exchange was refused `{"type": "rate_limit_error"}` 21 times across
7 minutes from a client sending no `User-Agent`. The identical payload with
`user-agent: axios/1.13.6` and `accept: application/json, text/plain, */*` was evaluated
immediately (HTTP 400 `invalid_grant`, the code having aged out by then). tcr's own
refreshes never see this because `refresh_access_token_at` already sends that User-Agent.

`exchange_code` does NOT send it. It sends only `Content-Type`. Logins are therefore
exposed to a false `rate_limit_error` that no amount of waiting fixes.

## 429 is not handled on the refresh path

In `refresh_access_token_at`, `is_server_error()` covers 5xx only and 429 is not in the
`{400, 401, 403}` auth set, so a 429 falls through to `OAuthError::Transient` on the first
attempt with no retry and no `Retry-After` read. The caller then stamps
`REFRESH_RETRY_COOLDOWN_MS = 2_000`, i.e. a 2-second retry aimed at whatever just refused.
`grow_error_backoff`'s own comment warns about exactly this hazard for the error path.

This is latent, not active: five days of logs contain zero
`token refresh transient failure` lines. Fix it, but do not assume 429 means "back off",
because on this endpoint it can mean "unrecognised client" and waiting never helps. Log
the body and distinguish.

## Not verified

- Every probe used the out-of-band `redirect_uri=https://platform.claude.com/oauth/code/callback`.
  tcr's login uses a localhost callback. No evidence the redirect target affects the
  granted lifetime, and no test either. Verify against a real `tcr login` before merging.
- Streaming/SSE traffic on a long-lived reduced-scope token. Only a non-streaming
  1-token completion was exercised.
- Whether `user:file_upload` actually carries uploads; the scope was granted, never used.
- Whether Anthropic revokes long-lived tokens earlier in practice than the year it grants.
