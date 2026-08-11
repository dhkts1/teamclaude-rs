# `tcr` command reference

Every subcommand and flag the `tcr` binary accepts, with the behaviour behind it. Back to
[the README](../README.md); the config file those commands read and write is documented in
[configuration.md](configuration.md).

All argument parsing lives in one file, `src/main.rs`; `src/cli.rs` is the *implementation*
of the account subcommands, not the definitions. There are twelve subcommands plus the bare
`tcr` form, and twenty-five flags between them. No flag has a single-dash short form, an
alias, an environment-variable binding or a clap-level default value; every default below is
either a Rust default (`false`, `None`, empty vector) or a fallback applied after parsing,
and each one was derived from the source, so start from `src/main.rs` to check any of them.

## Conventions

`--config <path>` appears on every subcommand except `update`, `demo` and `ui`. Unset, it
resolves to `~/.config/teamclaude.json`.

`tcr` and a subcommand cannot be mixed: the parser sets
`args_conflicts_with_subcommands = true`, so `tcr --port 9000 status` is a usage error
rather than a port override on `status`.

---

## `tcr` (bare) and `tcr server`

Runs the proxy. The bare form flattens the same arguments and dispatches to the same
function, so `tcr` and `tcr server` are the same command; the explicit form exists so you
can name it in a script or a launch agent.

| flag | type | default | effect |
|---|---|---|---|
| `--port <u16>` | number | config's `proxy.port` | bind port, overriding the config |
| `--config <path>` | path | `~/.config/teamclaude.json` | which config to load |
| `--headless` | bool | `false` | run without the TUI |
| `--replace` | bool | `false` | kill a proxy already on the port and take it |
| `--no-replace` | bool | `false` | **deprecated no-op** |

### Taking over the port

By default, starting a second `tcr` while one is already serving does **not** disturb the
incumbent: the new process stands down and exits. That is the safe direction, because replacing a
live proxy wipes its session-to-account pin map and cold-starts every live session's prompt
cache, which is the most expensive event in this system. `--replace` opts into doing it
anyway.

The stand-down carries information in its exit code. `0` means a peer proxy holds the port
and is serving code you have no reason to doubt. `3` means the incumbent is serving a
*different commit* than the binary you just ran, so `cargo build && tcr` stops instead of
proceeding as though your new build were live. `4` means the incumbent holds the listening
socket and never answered the liveness probe, which is the wedged shape and the case where
`--replace` is a recovery rather than an upgrade.

### `--no-replace` is a deprecated no-op

It parses, and it does nothing. Not-disturbing-an-incumbent is the *default's* behaviour;
the flag contributes nothing to it and is kept accepted only so existing scripts and launch
agents that already pass it keep working. Its field is read at no site in the binary.

Do not write `--replace --no-replace` in the same invocation. clap now rejects the pair by
name as a hard `ArgumentConflict`. The previous wiring made `--no-replace` a silent veto
over `--replace`, so an operator adding `--replace` to force a rebuilt binary onto the port
got a stand-down and exit 0 while `--help` told them the flag they had left in place did
nothing. The conflict error is the only outcome that cannot be misread, but it does mean an
invocation that used to "work" now fails loudly.

### Where the logs actually go

`--headless` logs to stdout **and** to a daily-rotating file under
`~/.cache/teamclaude/logs/` (`Rotation::DAILY`). The file sink is not a headless-only
feature: with the TUI running, tracing goes to the file *only*, because writing events to
stdout would corrupt the alternate screen.

The file is the sink that matters in practice. Anything launching the proxy as a background
child (TcrBar included) discards its stdout, so the log file is the only place those
events survive. The directory is created `0700` and re-asserted owner-only at every process
start, because log lines can carry account emails; if it cannot be made owner-only, `tcr`
refuses to log there rather than writing into a world-readable directory. `XDG_CACHE_HOME`
relocates it.

---

## `tcr run [-- <args>]`

Launches Claude Code already pointed at the proxy.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | which config to read the port from |
| `-- <args>` | strings | empty | passed verbatim to `claude` |

Trailing args are captured with `trailing_var_arg` and `allow_hyphen_values`, so
`tcr run -- -p "hi"` reaches `claude` intact.

If the proxy is not listening, `tcr run` launches `claude` **untouched** and says so on
stderr, so a stopped proxy never breaks the shell alias. When the proxy is up, the child
gets the routing environment and nothing else, in particular **no `ANTHROPIC_API_KEY`**,
even when `proxy.apiKey` is set. It used to get one, and that broke Claude Code: an
`ANTHROPIC_API_KEY` outranks claude's own claude.ai login as an auth source, which
**disables every claude.ai connector**, announced in one startup line that scrolls away,
after which the tools are simply absent. It bought nothing in exchange: the proxy's
`x-api-key` gate exempts loopback clients and the server binds `127.0.0.1` only, so a
`tcr run` child was always exempt. When a key is configured, `tcr run` prints a line on
stderr saying it is deliberately withholding it.

A value **you** exported is inherited untouched: an explicit choice wins, and it is the
escape hatch for a `claude` with no claude.ai login of its own, which does need some
credential to start. The process exits with `claude`'s own exit code.

---

## `tcr login`

Runs the browser OAuth flow and adds the resulting account to the pool.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to write into |
| `--force` | bool | `false` | skip the live proxy entirely and write the config file |

**A running proxy no longer has to be stopped.** Before opening the browser, `tcr login`
asks the proxy on the configured port whether it can take an account live, by POSTing a
deliberately-invalid body to `/_tcr/accounts` and reading the reply. What happens next
depends only on that answer:

| the proxy on the port | what login does |
|---|---|
| answers, and has the route | hands the account to the **running** proxy; it joins rotation immediately and the proxy writes the config itself |
| answers, but has no such route (an older `tcr`) | refuses, exactly as it always did |
| nothing listening | writes the config file, exactly as it always did |

The live path is the one worth having. Restarting the proxy to pick up a new account
discards the session→account pin map, so every live session cold-starts its prompt cache on
its next turn — the most expensive event in this system. Adding an account live costs none of
that. It also removes a second hazard: when the CLI writes the file itself while a proxy is
running, the two can interleave, and because Anthropic's refresh tokens are single-use, a
reverted write is not recoverable by retrying. On the live path only one process writes.

Logging in again as an account already in the pool is the same operation — its credentials
are replaced in place, and it keeps its position, its priority and its learned quota.

**The refusal still exists, and still means what it said.** Against a proxy too old to have
the route, the original hazard is real: that server reads the config at boot and its next
token refresh writes its *boot-time* tokens back over the file, silently clobbering a fresh
login (observed live). The message names the port, the pid, and the ordered remedy — stop the
server, run `tcr login`, then start it again. If the pid belongs to a host application serving
the proxy in-process (TcrBar), it says so and tells you to quit the application rather than
kill the pid: killing it skips shutdown and loses the pin map.

`--force` **skips the probe and writes the config file**, even when the running proxy would
have accepted the account safely. That makes it the unsafe path rather than the way around a
blocked one, and you almost certainly do not want it: the login succeeds and the running
server's next refresh can overwrite it. It remains only as an escape hatch for a proxy that
answers but misbehaves. Detection is read-only throughout; the server is never signalled.

The callback server binds a random loopback port, and tokens are never printed or logged.

---

## `tcr accounts`

Lists the configured accounts. Offline by construction: it builds its own view from the
file and never asks the server, so its serving counters render as unmeasured rather than as
zeroes.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to list |
| `--probe` | bool | `false` | refresh each account's live quota first, a real network call per account |

---

## Account resolution: `remove`, `priority`, `enable`, `disable`

These four take a positional `<query>` naming one account, and they all resolve it the same
way.

The rule is: **exact `name`, and if nothing matched, exact email**, where "email" means the
name with a trailing ` (Org)` suffix stripped. Both comparisons are `==` on the raw string.
That means resolution is **case-sensitive and never a substring**. Given an account named
`alice@example.com (Acme)`:

```
tcr disable "alice@example.com (Acme)"   # matches: exact name
tcr disable alice@example.com            # matches: exact email, org suffix stripped
tcr disable alice                        # NO MATCH: not a substring
tcr disable alice@                       # NO MATCH: not a prefix
tcr disable ALICE@EXAMPLE.COM            # NO MATCH: case-sensitive
```

A query matching nothing is an error and the config is left byte-identical: resolution runs
before any mutation, so there is no partial write. A query matching two or more accounts is
also an error, and it lists the candidates and tells you to narrow with `--org`.

`--org <name-or-uuid>` filters the candidates to one organization. It matches an org name
exactly, or an org uuid exactly or by prefix.

### `tcr remove <query>`

Deletes the account from the config. Flags: `--config`, `--org`.

This is destructive and there is no confirmation prompt. The entry is removed and the file
is rewritten in place; the access and refresh tokens go with it, so recovering the account
means running `tcr login` again, not editing anything back. It is also a file-only
operation: a running proxy keeps the account in its in-memory fleet until it is restarted,
so removing an account is not a way to stop traffic going to it. Use `tcr disable` for that.

### `tcr priority <query> [N]`

Sets rotation priority. **Lower value is preferred.** Flags: `--first`, `--last`, `--config`,
`--org`.

| form | effect |
|---|---|
| `tcr priority alice@example.com 5` | writes `5` verbatim |
| `tcr priority alice@example.com --first` | `min(0, all existing priorities) - 1` |
| `tcr priority alice@example.com --last` | `max(0, all existing priorities) + 1` |

The `0` seed in those relative forms guarantees the move crosses the default tier even when
every existing priority sits on the same side of it.

The positional value conflicts with `--first`/`--last`, and `--first` conflicts with
`--last`. There is no default: omitting all three is a runtime error,
`provide a priority value, or one of --first / --last`. This is a file-only write; it does
not reach a running proxy.

### `tcr enable <query>` and `tcr disable <query>`

`disable` holds an account out of rotation; `enable` clears the flag. Flags: `--config`,
`--org`.

**These act on the running proxy first, not on the file.** A file-only write was the
original bug: the proxy reads `disabled` from the config once, at startup, and never again,
so `tcr disable alice@example.com` exited 0, printed a confident line, and the proxy kept
handing that account live traffic while every surface reported it benched. The command now
POSTs to the proxy's `/_tcr/accounts/disabled` control route and only touches the file when
it has to.

The four outcomes:

- **The proxy applied it.** Done. Any caveat the proxy returned is printed as a warning.
- **Nothing is listening.** The quiet, historical case: the file is written, and there is no
  live rotation to disagree with it.
- **The proxy rejected the key.** `proxy.apiKey` did not match. The command changes
  *nothing* and exits non-zero, on purpose: writing the file here would put the old lie in a
  new place, with the config saying benched while the proxy you could not reach keeps
  rotating the account.
- **The route is missing.** An older `tcr` is serving. The file gets written, and the command
  says loudly that this is only half a disable.

If you have set `proxy.apiKey`, these two commands need it; they go through the same
loopback-plus-key gate as every other `/_tcr/` route, with no loopback exemption. See
[configuration.md](configuration.md#proxyapikey-is-a-security-control-not-a-convenience).

---

## `tcr status`

Probes every account's live quota and prints the fleet.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to read |
| `--json` | bool | `false` (text) | emit a JSON array instead of greppable text |

It asks the running proxy where there is one and falls back to an offline read where there
is not; the output labels which it got, so a fallback is never silently presented as a live
measurement.

---

## `tcr update`

Self-update. One flag, `--force`: rebuild or reinstall even when the source reports it is
already up to date.

What it does depends on how `tcr` was installed, which it classifies at runtime. From a git
checkout it runs `git pull --ff-only` and `cargo build --release` in that checkout, with git
and cargo output inherited so you see progress live. From an installed copy it fetches the
newest published release's installer and runs it against the directory the running binary is
in, so the update replaces the copy on your `PATH` instead of adding a second one. From
inside a `.app` bundle it hands the request to the app's own updater, falling back to printed
manual instructions when that handoff cannot be made.

Updating the binary does not update a running proxy. The process that is serving traffic
keeps its own image until it is restarted, and `tcr status --json` reports the running
build's SHA if you need to know which one is live.

---

## `tcr demo`

Takes no flags. Renders the TUI against fake accounts, which is how the sanitized README
screenshots are produced. It touches no real config and makes no network calls.

---

## `tcr ui`

Takes no flags. Opens TcrBar, the macOS menu-bar app, by asking LaunchServices for the
bundle id `io.github.dhkts1.tcrbar`.

It exists for discoverability, since `open -a TcrBar` already worked. Without the
subcommand, nothing in `tcr --help` reveals that a UI exists at all, so the app was only
findable by already knowing about it. It deliberately does not build the app or know where
your checkout is; when the bundle id is not registered it says TcrBar is not installed and
names the install script, rather than surfacing LaunchServices' exit code. On non-macOS
builds the subcommand still exists so `--help` is identical everywhere, and fails with that
reason.
