# `tcr` command reference

Every subcommand and flag the `tcr` binary accepts, with the behaviour behind it. Back to
[the README](../README.md); the config file those commands read and write is documented in
[configuration.md](configuration.md).

All argument parsing lives in one file, `src/main.rs` — `src/cli.rs` is the *implementation*
of the account subcommands, not the definitions. There are twelve subcommands plus the bare
`tcr` form, and twenty-five flags between them. No flag has a single-dash short form, an
alias, an environment-variable binding or a clap-level default value; every default below is
either a Rust default (`false`, `None`, empty vector) or a fallback applied after parsing.

## Conventions

`--config <path>` appears on every subcommand except `update`, `demo` and `ui`. Unset, it
resolves to `~/.config/teamclaude.json`.

`tcr` and a subcommand cannot be mixed: the parser sets
`args_conflicts_with_subcommands = true` (`src/main.rs:24-31`), so `tcr --port 9000 status`
is a usage error rather than a port override on `status`.

---

## `tcr` (bare) and `tcr server`

Runs the proxy. The bare form flattens the same arguments and dispatches to the same
function (`src/main.rs:225`), so `tcr` and `tcr server` are the same command; the explicit
form exists so you can name it in a script or a launch agent.

| flag | type | default | effect |
|---|---|---|---|
| `--port <u16>` | number | config's `proxy.port` | bind port, overriding the config (`src/main.rs:180-181`) |
| `--config <path>` | path | `~/.config/teamclaude.json` | which config to load (`src/main.rs:182-184`) |
| `--headless` | bool | `false` | run without the TUI (`src/main.rs:185-187`) |
| `--replace` | bool | `false` | kill a proxy already on the port and take it (`src/main.rs:188-193`) |
| `--no-replace` | bool | `false` | **deprecated no-op** (`src/main.rs:194-206`) |

### Taking over the port

By default, starting a second `tcr` while one is already serving does **not** disturb the
incumbent — the new process stands down and exits. That is the safe direction: replacing a
live proxy wipes its session-to-account pin map and cold-starts every live session's prompt
cache, which is the most expensive event in this system. `--replace` opts into doing it
anyway.

The stand-down carries information in its exit code (`src/main.rs:488-503`). `0` means a
peer proxy holds the port and is serving code you have no reason to doubt. `3` means the
incumbent is serving a *different commit* than the binary you just ran — so
`cargo build && tcr` stops instead of proceeding as though your new build were live. `4`
means the incumbent holds the listening socket and never answered the liveness probe, which
is the wedged shape and the case where `--replace` is a recovery rather than an upgrade.

### `--no-replace` is a deprecated no-op

It parses, and it does nothing. Not-disturbing-an-incumbent is the *default's* behaviour;
the flag contributes nothing to it and is kept accepted only so existing scripts and launch
agents that already pass it keep working (`src/main.rs:194-196`). Its field is read at no
site in the binary.

Do not write `--replace --no-replace` in the same invocation. clap now rejects the pair by
name as a hard `ArgumentConflict` (`src/main.rs:205`). The previous wiring made
`--no-replace` a silent veto over `--replace`, so an operator adding `--replace` to force a
rebuilt binary onto the port got a stand-down and exit 0 while `--help` told them the flag
they had left in place did nothing. The conflict error is the only outcome that cannot be
misread — but it does mean an invocation that used to "work" now fails loudly.

### Where the logs actually go

`--headless` logs to stdout **and** to a daily-rotating file under
`~/.cache/teamclaude/logs/` (`src/main.rs:898-930`; the directory at `:756-758`, the
appender at `:820-858`, `Rotation::DAILY`). The file sink is not a headless-only feature:
with the TUI running, tracing goes to the file *only*, because writing events to stdout
would corrupt the alternate screen.

The file is the sink that matters in practice. Anything launching the proxy as a background
child — TcrBar included — discards its stdout, so the log file is the only place those
events survive. The directory is created `0700` and re-asserted owner-only at every process
start, because log lines can carry account emails (`src/main.rs:820-847`); if it cannot be
made owner-only, `tcr` refuses to log there rather than writing into a world-readable
directory. `XDG_CACHE_HOME` relocates it (`src/main.rs:734-745`).

---

## `tcr run [-- <args>]`

Launches Claude Code already pointed at the proxy.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | which config to read the port from |
| `-- <args>` | strings | empty | passed verbatim to `claude` (`src/main.rs:167-175`) |

Trailing args are captured with `trailing_var_arg` and `allow_hyphen_values`, so
`tcr run -- -p "hi"` reaches `claude` intact.

If the proxy is not listening, `tcr run` launches `claude` **untouched** and says so on
stderr, so a stopped proxy never breaks the shell alias. When the proxy is up, the child
gets the routing environment, plus `ANTHROPIC_API_KEY` set to `proxy.apiKey` when one is
configured (`src/main.rs:329-331`). The process exits with `claude`'s own exit code.

---

## `tcr login`

Runs the browser OAuth flow and writes the resulting account into the config.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to write into |
| `--force` | bool | `false` | log in even while a proxy holds the port (`src/main.rs:155-165`) |

**It refuses while a proxy is running on the configured port.** The refusal is not caution:
the server reads the config at boot and its next token refresh writes its *boot-time* tokens
back over the file, silently clobbering the ones a fresh login just wrote — observed live
(`src/oauth.rs:782-797`). The message names the port, the pid, and the ordered remedy: stop
the server, run `tcr login`, then start it again.

If the pid it names belongs to a host application serving the proxy in-process (TcrBar), the
message says so and tells you to quit the application rather than kill the pid — killing it
skips shutdown and loses the session pin map.

`--force` is the deliberate escape hatch, and it is genuinely unsafe: the login succeeds and
then the running server's next refresh overwrites it. Detection is read-only either way; the
server is never signalled.

The callback server binds a random loopback port, and tokens are never printed or logged.

---

## `tcr accounts`

Lists the configured accounts. Offline by construction — it builds its own view from the
file and never asks the server, so its serving counters render as unmeasured rather than as
zeroes (`src/cli.rs:862-882`).

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to list |
| `--probe` | bool | `false` | refresh each account's live quota first — a real network call per account (`src/main.rs:67-75`) |

---

## Account resolution: `remove`, `priority`, `enable`, `disable`

These four take a positional `<query>` naming one account, and they all resolve it the same
way (`src/identity.rs:169-197`).

The rule is: **exact `name`, and if nothing matched, exact email** — where "email" means the
name with a trailing ` (Org)` suffix stripped (`src/identity.rs:129-137`). Both comparisons
are `==` on the raw string. That means resolution is **case-sensitive and never a
substring**. Given an account named `alice@example.com (Acme)`:

```
tcr disable "alice@example.com (Acme)"   # matches — exact name
tcr disable alice@example.com            # matches — exact email, org suffix stripped
tcr disable alice                        # NO MATCH — not a substring
tcr disable alice@                       # NO MATCH — not a prefix
tcr disable ALICE@EXAMPLE.COM            # NO MATCH — case-sensitive
```

A query matching nothing is an error and the config is left byte-identical — resolution runs
before any mutation, so there is no partial write (`src/cli.rs:125-133`). A query matching
two or more accounts is also an error, and it lists the candidates and tells you to narrow
with `--org`.

`--org <name-or-uuid>` filters the candidates to one organization. It matches an org name
exactly, or an org uuid exactly or by prefix (`src/identity.rs:188-195`).

### `tcr remove <query>`

Deletes the account from the config. Flags: `--config`, `--org`.

This is destructive and there is no confirmation prompt. The entry is removed and the file
is rewritten in place; the access and refresh tokens go with it, so recovering the account
means running `tcr login` again, not editing anything back. It is also a file-only
operation — a running proxy keeps the account in its in-memory fleet until it is restarted,
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
every existing priority sits on the same side of it (`src/cli.rs:158-199`).

The positional value conflicts with `--first`/`--last`, and `--first` conflicts with
`--last`. There is no default: omitting all three is a runtime error,
`provide a priority value, or one of --first / --last` (`src/main.rs:242-254`). This is a
file-only write; it does not reach a running proxy.

### `tcr enable <query>` and `tcr disable <query>`

`disable` holds an account out of rotation; `enable` clears the flag. Flags: `--config`,
`--org`.

**These act on the running proxy first, not on the file.** A file-only write was the
original bug: the proxy reads `disabled` from the config once, at startup, and never again —
so `tcr disable alice@example.com` exited 0, printed a confident line, and the proxy kept
handing that account live traffic while every surface reported it benched
(`src/cli.rs:201-217`). The command now POSTs to the proxy's `/_tcr/accounts/disabled`
control route and only touches the file when it has to.

The four outcomes (`src/cli.rs:227-268`):

- **The proxy applied it.** Done. Any caveat the proxy returned is printed as a warning.
- **Nothing is listening.** The quiet, historical case — the file is written, and there is no
  live rotation to disagree with it.
- **The proxy rejected the key.** `proxy.apiKey` did not match. The command changes
  *nothing* and exits non-zero, on purpose: writing the file here would put the old lie in a
  new place, with the config saying benched while the proxy you could not reach keeps
  rotating the account.
- **The route is missing.** An older `tcr` is serving. The file gets written, and the command
  says loudly that this is only half a disable.

If you have set `proxy.apiKey`, these two commands need it — they go through the same
loopback-plus-key gate as every other `/_tcr/` route, with no loopback exemption. See
[configuration.md](configuration.md#proxyapikey-is-a-security-control-not-a-convenience).

---

## `tcr status`

Probes every account's live quota and prints the fleet.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to read |
| `--json` | bool | `false` (text) | emit a JSON array instead of greppable text (`src/main.rs:138-146`) |

It asks the running proxy where there is one and falls back to an offline read where there
is not; the output labels which it got, so a fallback is never silently presented as a live
measurement.

---

## `tcr update`

Self-update. One flag: `--force` — rebuild or reinstall even when the source reports it is
already up to date (`src/main.rs:148-153`).

What it does depends on how `tcr` was installed, which it classifies at runtime
(`src/update.rs:896-923`). From a git checkout it runs `git pull --ff-only` and
`cargo build --release` in that checkout, with git and cargo output inherited so you see
progress live. From an installed copy it fetches the newest published release's installer
and runs it against the directory the running binary is in, so the update replaces the copy
on your `PATH` instead of adding a second one. From inside a `.app` bundle it hands the
request to the app's own updater, falling back to printed manual instructions when that
handoff cannot be made.

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
bundle id `io.github.dhkts1.tcrbar` (`src/main.rs:291`).

It exists for discoverability, not capability — `open -a TcrBar` already worked. Without the
subcommand, nothing in `tcr --help` reveals that a UI exists at all, so the app was only
findable by already knowing about it (`src/main.rs:276-285`). It deliberately does not build
the app or know where your checkout is; when the bundle id is not registered it says TcrBar
is not installed and names the install script, rather than surfacing LaunchServices' exit
code. On non-macOS builds the subcommand still exists so `--help` is identical everywhere,
and fails with that reason.
