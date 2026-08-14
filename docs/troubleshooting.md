# Troubleshooting

Symptoms, in the order people hit them. Flag-level detail lives in
[`cli.md`](cli.md); config keys and their defaults live in
[`configuration.md`](configuration.md).

## "The fix is in `main`" and "the fix is in the process serving traffic" are different facts

The running proxy can be several commits behind your checkout. `tcr status --json` reports
the SHA of the build that is actually serving; check it before concluding a change is live.

## Where the logs are

`tcr server --headless` writes to stdout *and* to a daily-rotating file under the cache
directory. The file is the half that survives supervision, because anything running the
proxy as a background child, TcrBar included, discards its stdout. That file is where to
look when a supervised proxy misbehaves. The exact path, the permissions it enforces, and the
environment variable that relocates it are in [Where the logs actually
go](cli.md#where-the-logs-actually-go).

## The port is already in use

By default a second `tcr` stands down rather than killing the incumbent, because two
proxies sharing one config both hold the same single-use refresh tokens, and the first
refresh by either revokes the other's. `tcr server --replace` takes the port deliberately.
Prefer standing down: a restart costs every live session its prompt cache, which is
per-account at Anthropic's end and the most expensive event in this system.

The stand-down's exit code tells you which case you are in: a healthy peer, an incumbent
running a different commit, or a wedged process holding the socket without answering. See
[Taking over the port](cli.md#taking-over-the-port).

## A restart is not automatically total loss

With `sessionAffinity` on, pins are persisted continuously and restored at boot within a
TTL; a restart inside that window keeps most sessions warm, and one outside it restores
nothing at all. The server logs how many pins it restored at boot; read that line rather
than assuming either outcome. With `sessionAffinity` off — the explicit opt-out, no longer
the default — there are no pins to restore. The TTL and the reasoning are in
[Session-affinity pins survive a
restart](architecture.md#session-affinity-pins-survive-a-restart).

## An account is disabled in the config but still getting traffic

`tcr enable` and `tcr disable` ask the running server first and fall back to the file only
when nothing is listening. Against a proxy too old for that route, the CLI warns that the
change will not take effect until the server restarts. Believe the warning.

## 503 with `Retry-After: 5`

Name resolution is failing, which means this machine is offline. No account was rotated and
no account was blamed; retry. That is deliberately not a 502, which would point you at the
upstream for a local fault.

## `tcr login` refuses to run

A running proxy on its own no longer causes this — `tcr login` adds the account to a live
proxy without stopping it. The refusal means the proxy holding the port is an **older build
without the `/_tcr/accounts` route**, and against that build the original hazard is real: it
reads the config at boot and its next token refresh writes its boot-time tokens back over the
file, silently clobbering the fresh login.

Update the running proxy and log in again, or stop it, log in, and start it again. `--force`
also gets past it, but it skips the live path and writes the file, which is the unsafe
behaviour this refusal exists to prevent — reach for it last, not first.

If the pid named belongs to TcrBar, quit the application rather than killing the pid: killing
it skips shutdown and loses the session pin map.

## `tcr ui` or a TcrBar update will not replace the app

You cannot replace `/Applications/TcrBar.app` while the proxy is running: TcrBar supervises
the bundled `tcr` as a child, so macOS sees an executing image inside the bundle being
swapped and Finder and Sparkle both refuse. Quitting TcrBar clears it, which means every
app update also restarts the proxy, with the prompt-cache cost above. Plan it for a quiet
moment.

## The TcrBar dev build fights the release build for the menu bar

macOS keys the menu-bar status item on the bundle id, and two processes registering the
same one get that id blacklisted by ControlCenter permanently, across reboots. The dev
build (`TCRBAR_DEV_BUILD=1`) has its own bundle id as well as its own path for exactly this
reason; do not give it the release id.

## The TcrBar build stops asking for `git fetch --unshallow`

The app's version is derived from the repository's commit count, so a `--depth 1` clone
cannot build it. Unshallow the clone and build again.
