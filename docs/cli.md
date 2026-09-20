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

**A config file that does not exist yet is created, not an error.** The first command you run
on a new machine writes the defaults to that path (`0600`, an empty `accounts` array), prints
`[tcr] created <path> with no accounts` on stderr, and then does its job. That holds for the
server and for every other verb, so `tcr status` on a fresh install answers with an empty
fleet instead of `config i/o error: No such file or directory`. A file that exists but cannot
be parsed is still an error and is never overwritten.

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

Every child also gets `TCR_RUN_ACTIVE=1`, on all three paths including the proxy-down
passthrough. It means one thing: **a `tcr run` is already above you in this chain.**
`tcr run` finds `claude` on `PATH`, so on a machine where something else also wraps
`claude` that lookup can land on a launcher which wraps in `tcr run` again — the routing
environment applied twice is identical and nothing breaks, but every startup line prints
twice and a second `tcr` sits in the process tree for the session. A launcher that checks
this variable hands off to the real `claude` instead. The name avoids the `CMUX_` prefix
on purpose: cmux's own claude wrapper clears every `CMUX_*` variable before exec, so a
marker named after it would be erased in transit.

---

## `tcr login`

Runs the browser OAuth flow and adds the resulting account to the pool.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | config to write into |
| `--force` | bool | `false` | override a refusal and write the config file anyway — never overrides a confirmed live route or a rejected api-key |
| `--account <name>` | string | none | re-login a specific existing account, and refuse to write anything unless the identity that comes back resolves to it |
| `--token` | bool | `false` | add an account from a `claude setup-token` credential instead of the browser flow — see below |
| `--from-claude-code` | bool | `false` | add an account from the login the `claude` CLI on this machine already holds, instead of the browser flow — see below. A first run does this by itself |
| `--name <name>` | string | none | name this account explicitly instead of letting `login` mint a name for it; refused when another account already has that name |
| `--non-interactive` | bool | `false` | drive the login from another program: never reads stdin, never opens the browser, reports progress as JSON lines — see below |

### `--non-interactive`: driving the login from another program

This is the mode TcrBar's own "Add account…" sheet runs the CLI in, and it is usable by
anything else that can read lines and open a URL. It changes four things and nothing else:

* **stdin is never read.** The pasted-code fallback and the name prompt are not merely
  ignored, they are not constructed, so the loopback callback is the only way the login
  can complete. A profile with no email at all — the inference-only case that would
  otherwise prompt — is named with the same `unnamed` default an empty answer gives; pass
  `--name` to choose.
* **The browser is not opened here.** The URL is printed instead and the caller opens it,
  because only the caller can bring its own window forward afterwards.
* **Progress is JSON, one object per line on stdout**, and nothing else is printed there:

  ```
  {"event":"browser","url":"https://claude.ai/oauth/authorize?…"}
  {"event":"waiting"}
  {"event":"saved","account":"alice@example.com"}
  ```

  A failure is `{"event":"error","reason":"…"}` on stdout instead of `saved`.
* **A failure exits non-zero** with that same reason on ONE stderr line, rather than
  anyhow's indented multi-line chain. The 2-minute callback timeout is a failure like any
  other.

It refuses to combine with `--token`, which reads the credential from stdin: that is the
one input this mode has no way to supply.

**A login names the account itself, and never asks on the happy path.** The name is the
profile's email when no other account carries it, and `email/<org-slug>` when one does —
so signing into a second organization of the same person lands beside the first row
instead of on top of it. `<org-slug>` is the organization's name lower-cased with every
run of non-alphanumerics collapsed to `-` (`Henry Token` → `henry-token`), falling back
to the first eight characters of the org uuid when the organization reports no name.
`--name` overrides all of that, and is refused if the name is already in use: taking a
name off an existing row is how a login overwrites the wrong credential.

**`--account <name>` targets one existing account and refuses to fix the
wrong one.** Without it, `login` upserts by whatever identity the browser hands back — a
default-browser OAuth flow carries whatever claude.ai session is already signed in, so
re-logging in a dead account can silently refresh a *different*, already-healthy one and
report success while the broken account stays broken. `--account` resolves
`<name>` against the config with the same rule `tcr enable`/`tcr disable` use
(exact, case-sensitive, no substrings). A re-login keeps the row's existing name rather
than minting a new one — that name is what its pins, group labels and `controlAccount`
key already point at. It also passes the resolved account's email as OAuth's `login_hint`,
which pre-selects that address on a clean login page — this part is ergonomics only, not
a guarantee: it is unverified whether the hint overrides a browser that is *already*
signed in as someone else, which is exactly the case the default-browser flow produces.
The hint is only ever sent when the resolved account's name is address-shaped; an
account named e.g. `work` never produces `login_hint=work`.

The correctness guarantee is a separate check, after the browser round trip and before
anything is written. **An account UUID alone identifies the *person*, not the account**:
the same person routinely holds more than one organization (a corporate Pro org and a
personal Max org), each with its own token and quota, so a UUID can be identical on two
different rows in the config. The check therefore resolves the identity that came back
the same way a write would — UUID *and* org together, through the same resolution
`upsert_account` uses — and only a resolution landing on the SAME row as `--account`
counts as a match. Two rows sharing a UUID but a different org (Corp vs. Personal) are
correctly treated as different accounts, and a re-login intended for one can never be
silently written onto the other. On any other outcome — a different row, an ambiguous
resolution, or an unresolvable one — **nothing is written**, the config is left
byte-identical, and the error names both the account that was requested and the identity
the browser actually authenticated as. A profile with neither an email nor a uuid (a
failed profile fetch) is refused the same way, never treated as a pass. An account with
no stored UUID *and* a non-email display name (e.g. `work`) has nothing on file to check
a returned identity against at all; that case is refused too, with a message that says so
plainly instead of suggesting a browser sign-out that could not help. Omitting `--account`
behaves exactly as before: no identity check runs, and a fresh login is free to land on
whatever account the browser returns — that is the add-a-new-account path.

**A running proxy no longer has to be stopped.** Before opening the browser, `tcr login`
asks the proxy on the configured port whether it can take an account live, by POSTing a
deliberately-invalid body to `/_tcr/accounts` and reading the reply — always, even with
`--force`. What happens next depends on that answer:

| the proxy on the port | what login does |
|---|---|
| answers, and has the route | hands the account to the **running** proxy; it joins rotation immediately and the proxy writes the config itself — wins even under `--force` |
| answers, but rejects the configured api-key | refuses outright — `--force` does not override this |
| answers, but has no such route (an older `tcr`), or answers unusably (wedged or timed out) | refuses, unless `--force`, which writes the config file instead |
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

`--force` **overrides a refusal**, not the probe: the probe still runs, and still wins when it
confirms a safe path. It never overrides a confirmed live route — that path is already safe,
so there is nothing to force. It never overrides a rejected api-key either: a rejection is
proof the proxy is alive and answering, which makes it the worst-informed moment to write the
config file beside it — fix `proxy.apiKey` to match the running server, or stop it and log in
offline, instead. Where `--force` *does* apply — no route on the port (an older `tcr`), or an
unusable answer (wedged or timed out) — it makes the unsafe path available anyway: the login
succeeds and the running server's next refresh can overwrite it. It remains only as an escape
hatch for a proxy that answers but misbehaves. Detection is read-only throughout; the server is
never signalled.

The callback server binds a random loopback port, and tokens are never printed or logged.

### `--token`: adding a `claude setup-token` credential

`claude setup-token` mints a long-lived access token for a headless or CI-style login,
without a browser. `tcr login --token` puts that token into the pool instead of running
the browser flow — same downstream config write, same live-proxy-vs-file routing above,
same "tokens are never printed or logged" guarantee, replacing only the browser half.

The token is **read from stdin**, prompted for when stdin is a terminal:

```
claude setup-token | tcr login --token
# or, interactively:
tcr login --token
Paste the token from `claude setup-token`: <paste>
```

It is never accepted as a `--token=<value>` argument, on purpose: an argv value is
visible to every other process on the machine via `ps`, and shells routinely persist
argv into history files. A prompt or a pipe are the only ways in.

A `claude setup-token` credential is not shaped like a browser login, and two
consequences follow directly from that:

**There is no refresh token, ever — not "sometimes missing", genuinely never.** The
mint requests only the `user:inference` scope, not the six scopes a normal `tcr login`
gets, and even though the token endpoint itself does return a refresh token for this
scope, the `claude` CLI that talks to it discards it before you ever see the output.
There is nothing for `tcr` to capture. The account this adds therefore serves until its
access token expires and then goes dead, with nothing to renew it — `tcr login --token`
prints that hazard on every successful add, on both the live and the file route (the
existing live-add warning above only fires through the running proxy's own wire route,
which this offline path never touches). Its expiry is stamped as **one year from now**:
that is `tcr`'s ASSUMPTION about what the mint requested (`expires_in: 31536000`, the
value the Claude CLI asks for), not something read out of the token itself — `tcr
status` shows that stamped date so an operator has something truthful to watch, rather
than "no expiry".

**There is usually no email either.** `/api/oauth/profile` needs more than
`user:inference` to answer, so the profile fetch this add still makes will very likely
come back empty — that is expected, not an error. Name the account with `--name`, or
answer the prompt when it is omitted. Decline the prompt too and it is named `unnamed`,
or the first free `unnamed-2`, `unnamed-3` when that is taken.

That collision scan is load-bearing rather than cosmetic. An account added this way has
no email and no account uuid, so it is the one kind of row `tcr` can only tell apart by
its **name** — every other account is resolved by uuid and org. Two of them sharing one
name would resolve to the same row, and the second `--token` login would overwrite the
first one's credentials instead of adding an account. The browser flow's `account-N`
fallback is derived from the account *count*, which hands out a name already in use as
soon as a row is removed; that is harmless for a credential carrying an identity to be
resolved by, and is exactly the bug here, which is why this path does not share it.

**`--token` refuses outright when combined with `--account`, and writes
nothing.** That flag exists to confirm that the identity a fresh login authenticates as
matches a specific existing row — see `--account`'s own section above. An
inference-only token carries no email and no account id for that confirmation to run
against, so the check can never be evaluated, and an assertion that cannot be evaluated
must fail closed rather than silently pass. Drop `--token` and use the browser flow with
`--account` instead, or use `--token` alone.

**The token is checked before it is written.** `tcr login --token` makes one authenticated
call with it — the same 1-token `POST /v1/messages` the keep-warm sends — and refuses on a
401 or 403, writing nothing: mint a fresh one and try again. A 429 or a 5xx is accepted (an
answer that got past auth proves the credential), and an unreachable upstream stores the
token with a warning rather than blocking an offline add. This check exists because an
account added this way is the one kind nothing else ever validates: with no refresh
token there is nothing to refresh and nothing to probe, so a mistyped or revoked token
used to be stored and reported `active` while every request it served came back 401. The
proxy now closes the other half too — a 401 on an account with no refresh token marks it
`error` on the spot (it cannot be rotation churn, since the token never changes), so a
credential that dies later shows as `error` in `tcr status` after its first failed request
instead of never.

### `--from-claude-code`: importing the login `claude` already has

If you use the `claude` CLI on this machine, you are already logged in to the account you
were about to log in to again. `tcr login --from-claude-code` copies that credential into
the pool instead of opening a browser:

```bash
tcr login --from-claude-code
```

It reads the same store the CLI does — the login Keychain item `Claude Code-credentials`
on macOS, otherwise `~/.claude/.credentials.json`. Set `TCR_CLAUDE_CODE_CREDENTIALS` to a
file path to read that instead of both; a path that does not exist means "no Claude Code
login on this machine", which is how this crate's own tests stay off the real Keychain.
Nothing here ever prints or logs a token.

Unlike `--token`, this credential is a full login: it carries a refresh token, a real
expiry and the `user:profile` scope, so the account renews itself like any browser login,
its email names the row, and `--account` works with it (there is an identity to confirm
against). `--name`, `--force` and the live-proxy add route all behave exactly as they do
for the browser flow, because it is the same finishing path.

**The cost, and it is stated on every import: refresh tokens are single-use.** The first
time tcr refreshes the imported credential, the copy the `claude` CLI is still holding
stops working, and `claude` asks you to log in again in the browser — once. Sessions
started with `tcr run` are unaffected, because `tcr run` never hands `claude` a key of its
own.

**A first run does this by itself.** Any of `tcr status`, `tcr accounts` and the server's
own boot, finding a config with no accounts in it, imports this login before doing its own
work and says so on stderr:

```
[tcr] imported 'you@example.com' (max) from your Claude Code login; this copies the login
the `claude` CLI itself uses, and each refresh token is single-use, …
```

Only when no login is found does the usual `no accounts configured — run \`tcr login\`` line
print instead. It never runs against a config that already has accounts, so an account
removed on purpose stays removed — `--from-claude-code` is the explicit way to redo it. A
Keychain read or profile fetch that fails is one warning line and the verb carries on with
zero accounts: never a non-zero exit, never a retry, and `--json` output is unchanged.

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

## Account resolution: `remove`, `priority`, `enable`, `disable`, `token`

These five take a positional `<query>` naming one account, and they all resolve it the same
way.

The rule is: **the account's exact `name`**, compared with `==` on the raw string. That
means resolution is **case-sensitive and never a substring**. A name is unique across the
config (see "Account names are unique" below), so an exact name is a complete answer and
there is nothing to narrow it by. Given an account named `alice@example.com/acme`:

```
tcr disable alice@example.com/acme       # matches: exact name
tcr disable alice@example.com            # NO MATCH unless a row is named exactly that
tcr disable acme                         # NO MATCH: the suffix is not a name
tcr disable alice                        # NO MATCH: not a substring
tcr disable alice@                       # NO MATCH: not a prefix
tcr disable ALICE@EXAMPLE.COM/ACME       # NO MATCH: case-sensitive
```

A query matching nothing is an error and the config is left byte-identical: resolution runs
before any mutation, so there is no partial write.

### Account names are unique

`tcr` will not let two accounts share a name, so a name is the whole address every account
verb takes. There used to be an `--org` flag on each of these verbs; it is gone, because a
flag that no longer narrows anything is a lie in `--help`.

It existed because the name was the email, and one person's two organizations — a personal
Max org and a company Team seat — produced two rows carrying the same one. Every by-name
path was a latent bug: `tcr token` refused as ambiguous, `tcr group add` labelled whichever
row came first and reported success, and the menu-bar panel drew one row's numbers on both.

**A config holding duplicates migrates itself, once.** On the next load, one row per
duplicated name keeps the bare email — the row on a personal plan (`claude_max` /
`claude_pro`), else the lowest priority number — and every other row becomes
`email/<org-slug>` from its stored `orgName`, falling back to the first eight characters of
its `orgUuid`. It prints one line per rename:

```
[tcr] renamed account: henry@example.com -> henry@example.com/henry-token (org Henry Token)
```

The loader never refuses a duplicated config — that would take a fleet down on upgrade —
and it writes the result straight back, so the rename happens once rather than on every
boot. `controlAccount` follows if it named a renamed row, group labels ride along
untouched (they are per-entry), and the session-affinity pin file is rewritten in the same
pass so warm sessions keep their prompt cache. If the write itself fails, the server still
boots and says so, and a CLI verb that would edit the config refuses rather than writing
under names that exist nowhere else.

A config edited by hand back into a duplicate is simply migrated again on the next load.

### `tcr remove <query>`

Deletes the account from the config. Flags: `--config`.

This is destructive and there is no confirmation prompt. The entry is removed and the file
is rewritten in place; the access and refresh tokens go with it, so recovering the account
means running `tcr login` again, not editing anything back. It is also a file-only
operation: a running proxy keeps the account in its in-memory fleet until it is restarted,
so removing an account is not a way to stop traffic going to it. Use `tcr disable` for that.

### `tcr priority <query> [N]`

Sets rotation priority. **Lower value is preferred.** Flags: `--first`, `--last`, `--config`.

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

`disable` holds an account out of rotation; `enable` clears the flag. Flags: `--config`.

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

### `tcr token <query>`

Prints the account's current access token to stdout — one line, nothing else — so it can
be piped or copied. Flags: `--config`. Read-only; a non-matching query exits
non-zero with the file untouched. It reads the file rather than the running proxy, which is
current enough: every refresh the proxy performs is written straight back to the file.

The token is a credential. Pipe it (`tcr token alice@example.com | pbcopy`); do not paste
it into a chat, a ticket, or a command line that lands in shell history. TcrBar's
right-click **Copy Access Token** runs exactly this and puts stdout on the pasteboard.

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

With no accounts configured, it first tries to import this machine's Claude Code login
(see `tcr login --from-claude-code`). When there is none to import, it prints the line
`no accounts configured — run \`tcr login\` to
add one` on stderr and exits **0**: stdout stays the ordinary empty table (`[]` under `--json`), so a
caller piping it into `jq` — TcrBar's panel among them — decodes an empty fleet rather than a
failure. The same line and the same exit code come from `tcr accounts`.

### The weekly quota pair on `--json`

Each row carries `sevenDay`/`sevenDayState`/`sevenDayResetAtMs` for the shared weekly bucket, and
the same three keys again under an `Oi` suffix for the Fable weekly (`unified-7d_oi`) bucket. No
per-row table exists for the row's other quota fields, so this documents only the pair this change
adds:

| key | shape | what it is |
|---|---|---|
| `sevenDayOi` | fraction or `null` | Fable weekly utilization (0.0–1.0), evaluated live. `null` when never learned |
| `sevenDayOiState` | `"ok"` / `"near"` / `"spent"` or `null` | this window's own state against the account's threshold, `null` when `sevenDayOi` is |
| `sevenDayOiResetAtMs` | Unix ms or `null` | when this window resets, `null` once it has already elapsed with nothing learned since, or if it was never learned |

Unlike the shared `sevenDay` bucket, this window gates **Fable requests only** — a non-Fable
request never checks it, and `held[]`/the general `quotaState` never reflect it either. It exists on
the wire so a Fable-scoped caption and tint have something to read.

### The group keys on `--json`

Each row carries its group labels plus the subsets that change what the row can serve. All
three are always arrays — `[]`, never `null`: "this account has no groups" is a known fact,
not an unmeasured one.

| key | shape | what it is |
|---|---|---|
| `groups` | array of strings | every label this account carries |
| `reservedGroups` | array of strings | the subset marked `reserved` — the row serves only traffic that asked for one of its groups |
| `parkedGroups` | array of strings | the subset marked `parked` (`tcr group park`) — non-empty means the row is out of rotation entirely, and names which group did it |

A parked row's `gate` reads `"parked"`, beside the existing `"disabled"`, `"reserved"`,
`"login"`, `"rejected"`, `"hold"`, `"five-hour"`, `"seven-day"`, `"fable-weekly"`,
`"standard"` and `"ok"`. A row that is BOTH parked by its group and disabled on its own
reports `"disabled"` — the row's own state outranks the group's, so the output never
attributes a bench to a group when a person did it.

```
tcr status --json | jq -r '.[] | select(.parkedGroups | length > 0) | "\(.name)\t\(.parkedGroups | join(","))"'
```

### The plan and org keys on `--json`

Every row carries which plan the account is on, and which org it belongs to — facts to SHOW
beside a row, not a way to address it. The `name` is the address, and it is unique. These
answer the question a reader looking at two rows of one person's two organizations still
has: *which* of these is the company seat?

| key | shape | what it is |
|---|---|---|
| `plan` | string or `null` | the customer-facing label: `Max 20x`, `Team 5x`, `Team Standard`, `Pro`, … `null` for an account never profiled — never a fabricated default |
| `organizationType` | string or `null` | the provider's own word, verbatim: `claude_max`, `claude_team`, `claude_pro`, `claude_enterprise` |
| `rateLimitTier` | string or `null` | verbatim, e.g. `default_claude_max_20x` — the rate-limit multiplier, when it names one |
| `seatTier` | string or `null` | verbatim, e.g. `team_standard` / `team_tier_1` — which seat this row holds. `null` on Max and Pro, which have no seats |
| `orgUuid` | string or `null` | the org this account is scoped to |
| `orgName` | string or `null` | the org's display name. Two orgs can share one, which is why `orgUuid` is the exact fact and this is the readable one |

`plan` is DERIVED from the three raw keys, in one place, so the JSON, the plain text (`plan=`), the
TUI and the macOS panel cannot disagree. The plan word comes from `organizationType`; the suffix
after it is the `rateLimitTier` multiplier (`_20x` / `_5x`) when there is one, otherwise — on Team
and Enterprise only — the seat (`team_standard` → `Standard`, `team_tier_2` → `Tier 2`). The
multiplier wins because it is the stronger statement: a premium Team seat reads `team_tier_1` with a
`_5x` tier, and `Team 5x` says what the account can do where `Team Tier 1` says nothing. An
`organizationType` we do not recognize is passed through verbatim rather than bucketed into a plan we
do — a new plan name shows as itself:

```
tcr status --json | jq -r '.[] | "\(.name)\t\(.plan // "unprofiled")\t\(.orgUuid // "-")"'
```

The plan is learned at login, and backfilled for accounts that logged in before these keys existed:
the background quota probe fetches the profile once for any account that has no `organizationType`,
records it, and writes it to the config. So a fresh checkout shows `null` until each account's first
probe, not forever.

### The `usage` object on `--json`

Every row carries `usage`: what that account spent, aggregated by the proxy as it served each
request rather than by re-reading transcripts afterwards.

```
tcr status --json | jq '.[] | {name, today: .usage.today.costUsd, hour: .usage.lastHour.costUsd}'
```

| key | shape | what it is |
|---|---|---|
| `usage.today` | totals | the **local calendar day** of the machine the server runs on |
| `usage.window` | totals + `since` | this account's current 5-hour quota window, read from Anthropic's own reset header. `null` when that reset is unknown, because then the window's start cannot be named |
| `usage.lastHour` | totals | the trailing 60 minutes — burn rate is `lastHour.costUsd` per hour, by definition |
| `usage.todayByModel` | `{model: totals}` | `today`, split by model id |

Each totals object carries `requests`, `inputTokens` (BASE input — see below), `cacheCreationTokens`,
`cacheCreation1hTokens`, `cacheReadTokens`, `outputTokens`, `costUsd` and `unpricedRequests`.

`cacheCreationTokens` is ALL cache creation, both TTLs — the same quantity the row-level key of that
name carries, so the two never mean different things in one row. `cacheCreation1hTokens` is the
subset of it written under the extended 1-hour window (which bills at 2x base input rather than
1.25x); the 5-minute part is the difference between the two.

**`usage.today.inputTokens` is not the row's `inputTokens`.** The row-level one is the QUOTA counter
and folds cache creation and cache reads into a single number, which is what quota is charged on. The
usage object keeps the four dimensions apart because they bill at four different rates, so they have
to be separable to be priced at all. The new row-level `cacheCreationTokens` is the companion
`cacheReadTokens` never had: with both, a reader can see how much input was served FROM cache and how
much was spent WRITING it.

**`costUsd` is API list price, not a bill.** These accounts are subscriptions; nothing here is
charged. The figure answers "what would this traffic have cost on the API", which is the only unit
that compares across accounts, models and days. When a model has no published rate in this build,
`costUsd` is `null` and `unpricedRequests` counts the requests missing from the figure — a partial
total says so rather than passing itself off as the whole. Add a rate with `pricing` in the config
([configuration.md](configuration.md#pricing-and-usageretentiondays)).

`costUsd: null` means exactly that one thing: the bucket served requests and not one of them could be
priced. A bucket that served NOTHING reports `costUsd: 0.0` — an idle account is a measured zero, in
`today` and `lastHour` alike.

**`usage: null` means not measured, never "spent nothing".** Two cases produce it: the row came from
the offline path (no serving process, so there is nothing to aggregate), or the proxy that answered
was built before this field existed — which is the ordinary state here, since the binary on disk is
rebuilt on merge while the live process keeps serving until someone restarts it. `source` on the same
row says which.

Totals survive a restart: each served request is appended to `~/.cache/teamclaude/usage/`, and boot
replays the day back in. Fleet-wide totals are not a field — sum the rows.

### The `sessions` array on `GET /_tcr/status`

Reachable via the raw `/_tcr/status` endpoint, not the CLI's own array output: one entry per Claude
Code session seen in the last hour, each carrying its tool-call stats (`tools.running`, the pending
calls; `tools.slowest`, the ten slowest completed ones). `tools.running` entries for an `Agent` or
`Task` tool call carry the subagent's description in `commandHead` (prefixed with its type when
Claude Code sent one), and `tools.subagentsRunning` is that count precomputed, so a panel can show
"2 subagents" without scanning the list itself.

Each session also carries `tools.byTool` (a per-tool-name breakdown: calls, errors, a median
duration over a bounded reservoir, and `overOneMinute`) and `reqPerMinute` (30 wall-clock minutes
of request counts, oldest first, decaying to zeros as the session goes idle); the payload's
top-level `sessionsSummary` is the same `byTool` shape summed across every session server-side, so
a panel's fleet-wide headline never disagrees with the per-session rows by re-summing them itself.

---

## `tcr doctor`

Answers one question: **is Claude Code actually reaching this proxy, and if it is not, what decided
otherwise.** Every other verb here reports the fleet this proxy holds; this one reports whether
anything is pointed at it.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | the per-user config path | config to read |
| `--json` | bool | `false` (text) | emit one JSON object instead of greppable `key: value` lines |

```
$ tcr doctor
baseUrl: https://gateway.example.com
baseUrlSource: /Users/example/.claude/settings.json
proxyPort: 3456
portHolder: pid 4242 tcr
portHolderIsTcr: yes
requests10m: 0
verdict: this proxy is not on Claude's route: /Users/example/.claude/settings.json sets https://gateway.example.com
```

| exit | meaning |
|---|---|
| **0** | Claude Code is routed to this proxy and the proxy answered |
| **2** | Claude Code is routed somewhere else; the verdict names the file or variable that set it |
| **3** | the route points here and nothing answered on the port |

"Routed here" means the base URL names a loopback address (`127.0.0.1`, `localhost`, `[::1]`) on the
port this config uses. Another machine's proxy on the same port is not this one.

### Why the base URL can come from a file rather than the variable

`tcr run` exports `ANTHROPIC_BASE_URL` onto the `claude` it launches. Claude Code then applies its
settings files' `env` block **on top of** the environment it inherited, so a base URL written in a
settings file wins over the one the launcher exported, silently. `doctor` resolves the four sources
in Claude Code's own order and reports the winner:

1. `.claude/settings.local.json` in the working directory
2. `.claude/settings.json` in the working directory
3. `.claude/settings.json` in the home directory
4. the process environment's `ANTHROPIC_BASE_URL`
5. failing all of those, `https://api.anthropic.com`

A settings file that exists and cannot be parsed is reported on its own `problem:` line rather than
read as "sets nothing": it is the likeliest place for the answer to be hiding, and skipping it
quietly would let `doctor` name the wrong source with confidence.

`requests10m` is how many requests this proxy served in the last ten minutes, summed from the
per-minute sparklines on `/_tcr/status` (`reqPerMinute`, documented under `tcr status`). It reads
`none` when no proxy answered, which is a different fact from `0`: a proxy that is up and idle is
healthy, and only the first of those is exit **3**.

`portHolder` is whoever is listening on the port, by pid and process name, from the same listener
enumeration `tcr server` uses to decide a takeover. It answers the case the verdict line cannot:
the route is right, nothing answers, and the port is held by something that is not a `tcr`.

`--json` emits the same decision as one object, with `verdict` and `exitCode` written out so a
caller reads one document instead of re-deriving either.

`tcr status` prints this verdict line **first**, and only when it is exit 2 or 3, so a glance at the
fleet cannot show healthy accounts while Claude Code talks to something else. The `--json` form of
`status` is untouched: it stays a bare array.

---

## `tcr sessions`

`tcr sessions [--json]` — the sessions the RUNNING proxy has seen in the last hour. This is the
channel TcrBar's panel reads for its Sessions and Tools tabs: the rows live only on the proxy's own
`/_tcr/status` response, behind the proxy api-key, and the app may not read that key, so it shells
out here the way it does for every other fact.

Live only, with no offline fallback. A session is per-process state that exists nowhere on disk, so
an offline rendering could only be an empty list dressed up as a measurement. With nothing listening
the command exits non-zero and says so on stderr.

```
tcr sessions --json | jq -c '{supported, n: (.sessions|length)}'
```

`supported` answers **did this server's payload carry a `sessions` key**, not whether the array has
anything in it. A proxy built before sessions existed and a completely idle one both report zero
rows, and the panel has a different sentence for each, so the two must stay distinguishable. The
rows themselves are the same `SessionRow` shape `tcr status --json`'s payload documents above,
verbatim.

`tcr status --json` is unchanged and still emits a bare array of accounts. The `supported` flag has
nowhere to live in an array, which is why this is a separate verb rather than a flag on `status`.

The build-skew warning goes to **stderr** in both modes, as `status`'s does, so `--json`'s stdout
stays a single JSON object a script can pipe into `jq`.

---

## `tcr wrap`

`tcr wrap [--days N] [--json]` — a usage report for the last `N` days (default 7, ending
today UTC), read straight off the usage ledger in `~/.cache/teamclaude/usage/`. Unlike `tcr
status`, this needs no running proxy: the ledger is a durable, append-only record on disk, and
`tcr wrap` just reads it.

It reports, in order: totals (requests, input/output/cache-read tokens, cost, cache-hit
ratio), a per-model breakdown sorted by cost, a per-account breakdown sorted by requests, a
per-day breakdown with the busiest day marked, the count of distinct sessions plus the three
longest-running ones by request count, and a comparison against the previous `N`-day period
(`+12%` / `-3%`). `--config` (default `~/.config/teamclaude.json`) is consulted only for
pricing overrides — never for accounts, so a removed account's traffic still shows up under
the name it was recorded under. `--json` emits the same numbers as one object instead of the
plain-text layout.

"Day" here is the UTC calendar day a ledger line's own file is named after — the same day
`tcr status`'s `--json` `usage` object writes to and `attach_ledger`'s boot replay reads — not
the local day `today` on that object uses, and cost is the same API list-price figure `tcr
status` reports (see the note there: no dollar here is ever billed, every account in this
fleet is a subscription).

Every number is reproducible straight from the files, without running `tcr` at all — for
example, total requests and cost for one day:

```
$ jq -s '{requests: length, inputTokens: (map(.i) | add)}' ~/.cache/teamclaude/usage/2026-09-10.jsonl
{
  "requests": 5,
  "inputTokens": 812000
}
```

Example (fake data):

```
$ tcr wrap --days 7
tcr wrap: last 7 day(s), 2026-09-06 to 2026-09-12 (UTC)

totals requests=1204 input=812000 output=241500 cacheRead=98000 cost=$42.17 cacheHitRatio=12.1%

by model (sorted by cost):
  claude-opus-5                requests=610      cost=$31.80
  claude-sonnet-5              requests=594      cost=$10.37

by account (sorted by requests):
  alice@example.com            requests=812      cost=$28.44
  bob@example.com              requests=392      cost=$13.73

by day (busiest marked *):
  2026-09-06   requests=140      cost=$4.90
  2026-09-07   requests=210      cost=$7.35
  2026-09-08 * requests=305      cost=$10.68
  2026-09-09   requests=180      cost=$6.30
  2026-09-10   requests=145      cost=$5.08
  2026-09-11   requests=120      cost=$4.20
  2026-09-12   requests=104      cost=$3.66

sessions: 38 distinct; top 3 by requests:
  session 1234567890123456789 requests=210
  session 9876543210987654321 requests=140
  session 5566778899001122334 requests=95

compared with the previous 7 day(s): cost=+12% requests=-3%
```

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

---

## `tcr peer`

Finds other Macs on the network, trusts them, and lets trusted Macs share Claude accounts
with each other. A walkthrough of the whole flow, Find, Trust, Share, the network key, the
share link, and exactly what each act sends over the network, is in
[peers.md](peers.md); this section is the flag reference for every `tcr peer` subcommand.
None of it is on by default: a fresh install discloses nothing, opens no port and answers
nobody.

Every subcommand takes `--peers <path>`, defaulting to the peers file in the operator's
config directory, for the same reason `--config` is on every account verb: a test (or a
second identity on one Mac) points the whole peer surface at a different file with one flag.

### `tcr peer id`

Prints this node's own peer id, minting the keypair on first use.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file (and its node-key directory) to use |
| `--regenerate` | bool | `false` | mint a NEW keypair; every peer that pinned the old one is evicted and its next handshake fails the pin check. Requires `--yes` |
| `--yes` | bool | `false` | confirms `--regenerate`; has no effect alone |

### `tcr peer ls`

Lists pinned peers, what each may do, and what is in flight.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to read |
| `--json` | bool | `false` | machine-readable output. A sibling document to `tcr status --json`, never merged into it: that is a bare array of accounts, and clients depend on exactly that shape |
| `--config <path>` | path | `~/.config/teamclaude.json` | main config to read for the account labels the `lentTo` block is keyed by. Nothing else is taken from it; a config that is missing or unreadable leaves `lentTo` empty instead of failing the listing |

### `tcr peer find <on\|off>`

Turns discovery on or off: announcing this Mac's presence and looking for others.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn discovery on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--announce-name <on\|off>` | enum | `off` | whether the beacon includes this Mac's display name. Off either way, the beacon never carries a key, a peer id, or any other identity material |

**This verb writes a flag; the running server does the announcing.** It re-reads the flag
about every twenty seconds, so `on` starts the beacon within that and `off` stops it within a
minute, neither one needing a restart. The beacon carries the announcing process's per-boot
instance id, which is why the CLI cannot announce on the server's behalf: a neighbour's knock
names the id it saw, and an id from a CLI that has already exited matches nothing.

### `tcr peer name [name]`

Sets, or with no argument prints, this Mac's display name, what another Mac shows for it.
Refused if it carries an `@`, a UUID shape, or an organization name, because a name reaches
other machines. With nothing set, the name shown is the host name.

| flag | type | default | effect |
|---|---|---|---|
| `[name]` | positional | prints current | the name to show |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer share <on\|off>`

Turns account sharing on or off for every currently trusted peer, at a default lend amount.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn sharing on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--window <5h\|7d\|7d_oi>` | enum | `7d` | which rate-limit window is shared. `5h` and `7d` are untiered; `7d_oi` is the only window upstream reports per model |
| `--fraction <f>` | float | `0.1` | ceiling on any one lease, as a fraction of the window. Clamped to `0.0..=0.5` |
| `--ttl <secs>` | int | `600` | how long a granted lease lives before it needs renewing |
| `--max-inflight <n>` | int | `2` | how many borrowed requests may be in flight at once against one lease. `0` mints leases that refuse every request, which is what writing `0` asks for |
| `--scope <scope>` | string | `all` | what the DEFAULT lease draws from: `all`, `group:<name>`, or `account:<label>[,<label>]`. Written onto every pinned Mac and recorded as `defaultLend`, which the Sharing defaults sheet shows |

`tcr peer share on` with no flags mints exactly one grant per trusted peer: 10% of the weekly
window (`7d`), a 600-second lease, at most 2 requests in flight, drawn from every account.

Run again over a grant that already exists for the same window and scope, it **keeps that
grant's mode, its end date and its daily hours** and changes only what you passed. Those three
are decisions taken per peer: turning a `hand` grant back into a `serve` grant is a different
disclosure than the one that was taken, and it used to happen silently on every re-run.

`--fraction 0` **removes** the grant for that window and scope rather than minting a zero one,
and prints how many it removed. Every Mac that still holds `inspect` may still open a serve
stream; each request on it is then refused for want of a grant, and `tcr peer share off`
closes the streams too.

### `tcr peer pair <addr> [code]`

Trusts a Mac interactively: both screens show six digits, both operators compare them and
confirm.

| flag | type | default | effect |
|---|---|---|---|
| `<addr>` | positional | | `host:port` of the Mac to pair with |
| `[code]` | positional | starts pairing | the six digits the other screen is showing. Omitted, this starts the pairing and prints this side's digits instead |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

The six digits are built from a **nonce contributed by each side**, and the answering Mac
commits to its nonce before it sees the other's, so neither end can steer the digits after it
knows what the other picked. A Mac on a build that predates this sends a shorter message and
is **refused by name**: the error says an older build reached here and to update it and pair
again, rather than falling back to digits one side could have chosen.

### `tcr peer invite`

Mints a one-line join key for a Mac with no screen to compare digits on: the headless path.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--label <name>` | string | none | a name for the joining Mac. Refused if it carries an `@`, a UUID shape, or an organization name |
| `--ttl <secs>` | int | `600` | how long the key stays usable. Short on purpose: anything that can read the peers file can use an outstanding key while it exists |
| `--uses <n>` | int | `1` | how many Macs may join with this one key |
| `--revoke <id>` | int | none | revoke an outstanding key by id instead of minting one |

The key carries **every address this Mac can be reached at**, best first
(`tcr-join:v2:<addr,addr,...>:...`), and the joining Mac tries them in order: the tailnet
address, then the external address a port mapping published, then each address a real
interface holds. A bind address is never one of them: a Mac listening on `0.0.0.0` used to
mint a key reading `0.0.0.0`, which sent the friend's `tcr peer join` at its own machine.
Under the key, one line per address says which kind it is:

```
peer invite: tailscale 100.64.0.1:7755
peer invite: lan 192.0.2.10:7755
```

When none of them is reachable from outside this network, one more line says so: over the
internet the friend needs this Mac's router to forward the port, and `tcr peer reach` reports
where that stands. When `listen` names one specific address rather than `0.0.0.0`, the key
carries that address alone, because somebody chose it.

### `tcr peer join [key]`

Joins another Mac using a key or link it printed.

| flag | type | default | effect |
|---|---|---|---|
| `[key]` | positional | | the `tcr-join:…` key or the `tcr://peer/join?…` link the other Mac printed. **Visible in `ps` and shell history**: use `--stdin` to avoid that |
| `--stdin` | bool | `false` | read the key or link from standard input instead of argv. The only path the panel and the `tcr://` URL handler use, and the one that never leaks the secret to another process |
| `--label <name>` | string | none | this Mac's name, as the other one will show it |
| `--replace` | bool | `false` | accept a link's network key when this Mac already has one |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

A link that carries a network key is **refused when this Mac already has one**, and the
refusal names what replacing it would cut this Mac off from. It is the same refusal
`tcr peer network-key join` gives, for the same reason: a second office's key pasted over the
first is the commonest way a Mac disappears from its own mesh. `--replace` means it.

### `tcr peer forget <peer>`

Stops trusting a Mac. One deleted line; the next handshake from it fails.

| flag | type | default | effect |
|---|---|---|---|
| `<peer>` | positional | | the peer id to forget, as `tcr peer ls` prints it |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

Does **not** revoke egress still reachable through a peer that holds the `forward` grant; the
output says so whenever one does.

### `tcr peer allow <peer> <grant> <on\|off>`

Grants or revokes one thing for one peer.

| flag | type | default | effect |
|---|---|---|---|
| `<peer>` | positional | | the peer id, in its full wire form: the `node` field of `tcr peer ls --json`, never the short `tcr-…` form the text output prints |
| `<grant>` | enum | | `gateway` (carry this peer's bytes out, blind), `forward` (relay to peers this Mac has pinned, transitive), `inspect` (accept this peer's requests and serve them here, reading them in full), `disclose` (send requests to this peer, letting it read them in full), `accept-move` (accept an account this peer moves here, the only grant under which a credential crosses a host boundary), `control-briefs` (tell this peer about this Mac's other peers, one hop out), `control-lendable` (tell this peer how much this Mac could lend), `control-diag` (tell this peer this Mac's build and boot id) |
| `<on\|off>` | positional | | grant or revoke |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer lend <peer>`

Sets what one peer may borrow, on one window and scope, or takes it away. A Mac may hold
several leases at once, one per scope: lending a new scope adds a lease rather than replacing
the ones already granted. `--list`, `--revoke` and `--relend` manage that set; on the panel
this is the per-Mac sheet's **Lend from** list.

| flag | type | default | effect |
|---|---|---|---|
| `<peer>` | positional | | the peer id in full wire form, same rule as `tcr peer allow` |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--window <5h\|7d\|7d_oi>` | enum | `7d` | which window this grant is against |
| `--fraction <f>` | float | `0.1` | ceiling on any one lease, as a fraction of the SCOPE's headroom. Clamped to `0.0..=0.5`. `0` removes the grant |
| `--ttl <secs>` | int | `600` | how long a granted lease lives before it needs renewing |
| `--max-inflight <n>` | int | `2` | how many borrowed requests may be in flight against one lease |
| `--scope <scope>` | string | `all` | what this lease draws from: `all`, `group:<name>` (one `tcr group` group, pooled), or `account:<label>[,<label>]` (one or more accounts, by the sanitized label `tcr status` prints, never an email or a uuid) |
| `--for <duration>` | string | no end | lend for a duration (`2h`, `90m`, `3d`), after which this Mac stops renewing the lease. `none` clears an end |
| `--until <time>` | string | no end | lend until a time of day (`18:00`), today or tomorrow, whichever comes next |
| `--between <HH:MM-HH:MM>` | string | no restriction | only open the lease inside this daily window, in this Mac's local time. `22:00-08:00` crosses midnight and is charged to the day it starts, so a Friday-only schedule with that window is open Friday 22:00 through Saturday 08:00 |
| `--days <mon,tue,...>` | string | every day | only open the lease on these days of the week, read against the day the window starts |
| `--mode <serve\|hand>` | enum | keeps the replaced grant's mode, or `serve` for a new one | `serve` sends the borrower's requests over this Mac and out on this Mac's IP; this Mac reads them. `hand` gives the borrower a short-lived access token instead, over the paired session, so the borrower sends the request on its own IP and this Mac never reads it. Omitted on a replace keeps the mode already in force, so editing a hand grant's fraction cannot quietly turn it back into `serve` |
| `--list` | bool | `false` | list this peer's leases instead of changing them, one greppable line each, with the lease id `--revoke` and `--relend` take |
| `--revoke <lease-id>` | string | none | take one lease away, by the id `--list` printed. The other leases this Mac holds are untouched |
| `--relend <lease-id>` | string | none | put an ended lease back to work, with a new `--for`/`--until` or with no end at all |

A lease asked for outside its own `--between`/`--days` window is refused: the borrower gets
`OutsideSchedule` back instead of a grant, and nothing is served. A lease with no schedule set
behaves exactly as it does today, open at every hour.

**What `--mode hand` changes.** A `serve` grant is a proxy: Mac B's request travels to Mac A,
Mac A sends it to the account's real destination and hands the reply back, so Mac A's `tcr`
sees every prompt. A `hand` grant is different in kind: Mac A mints the account's own
short-lived access token and sends it to Mac B once, over the already-paired, already-encrypted
session. From then on Mac B talks to the account directly, on Mac B's own IP, and Mac A reads
nothing. Mac A still holds the refresh token and still decides when the lease ends; letting it
expire, or `--revoke`, is the only way to take a hand grant back, since there is no request
passing through Mac A to refuse.

A `hand` grant is refused outright, before anything is written, when every account the scope
covers has `egressStrict` on: that pin says the account's requests leave through one named Mac
or not at all, and a borrower sending on its OWN IP can never be that Mac. Lend it as
`--mode serve` instead, or clear the pin on an account the scope covers first.

### `tcr peer status`

Asks the RUNNING proxy what it holds for each pinned Mac: what is in flight, what each lease
has spent, and which paths have been measured. Nothing here is read off the peers file, so a
row appears only while a server is up.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/teamclaude.json` | main config to read the port and api-key from |
| `--json` | bool | `false` | emit the peers block as JSON instead of greppable text |

Each row carries the pinned key twice: **`id`** is the full wire form, the only spelling
`tcr peer lend --revoke` and `--relend` read back, and **`display`** is the short `tcr-…` form
for a person reading one row aloud. Nothing parses `display` back. A row's `name` is the
operator's label through the same sanitizer `tcr peer ls` uses, so a label that is an email or
a UUID comes out `[masked]`.

### `tcr peer via <target>`

Chooses how this Mac reaches the internet when its own connection is down.

| flag | type | default | effect |
|---|---|---|---|
| `<target>` | positional | | `auto` to fall back to a trusted, willing Mac automatically, `off` to never route out through a peer, or a peer id to pin one specific Mac |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer internet <on\|off>`

Opts this Mac into being reached from off its own network. Off by default: a fresh install
answers nobody outside its LAN, the same as it answers nobody at all before `tcr peer listen`
is set.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn internet reachability on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

Turning it on asks the router for a mapping on the peer listener's port and keeps it open,
renewed every 30 minutes on a 2-hour lifetime; turning it off, or shutting down, deletes the
mapping instead of leaving it on the router. See [peers.md](peers.md) § "Reaching a Mac off
your network" for the fallback path when the mapping itself is unreachable.

**A running server honours both settings without a restart.** It re-reads the flag about every
five seconds, the way it re-reads `peer.find`: `off` ends the keeper and deletes through
whichever protocol granted the mapping, and `on` starts a keeper and asks the router again. The
flag used to be read once at boot, which meant `off` ran in the CLI, deleted over NAT-PMP, and
the server's next renewal simply created the mapping again; `on` then reached nothing at all
until the next restart.

### `tcr peer pending`

Shows the Macs asking to pair with this one, and nothing else about them: a request is a
row, never a trust decision.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file (and the runtime-state file beside it) to use |
| `--json` | bool | `false` | machine-readable output |

### `tcr peer accept <target>`, `tcr peer ignore <target>`, `tcr peer block <target>`

Answer one pairing request. `<target>` is the instance id or address, as `tcr peer pending`
prints it, on all three.

| verb | effect |
|---|---|
| `accept` | opens a two-minute window during which the six-digit compare can complete for that one Mac |
| `ignore` | turns the request down and stays quiet to that address for an hour |
| `block` | refuses permanently: bans the address, and the peer's static key too once the handshake has learned one, so a new address does not help it |

Both take `--peers <path>` (default: the peers file in the operator's config directory).

### `tcr peer unblock <addr>`

Lifts a block.

| flag | type | default | effect |
|---|---|---|---|
| `<addr>` | positional | | the address to unblock, as `tcr peer ls --json` lists it under `blocked` |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer network-key <set\|join\|clear\|show> [key]`

The opt-in office network key (52 characters, 32 bytes, Crockford base32): mint one, paste one
in, clear it, or check whether one is set.

| flag | type | default | effect |
|---|---|---|---|
| `<action>` | enum | | `set` mints a key and prints it once; `join` pastes in a key another Mac printed; `clear` forgets it, making this Mac visible and reachable to every `tcr` on the network again; `show` says whether one is set, without printing it |
| `[key]` | positional | | for `join`: the key string. Omit with `--stdin` |
| `--stdin` | bool | `false` | for `join`: read the key from standard input, so it never enters argv or shell history |
| `--replace` | bool | `false` | required by `set` and `join` when a key is already set, because replacing it cuts this Mac off from every Mac still holding the old one. Without it the verb refuses and prints what it would have cut off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer link`

Prints one link to share, which brings another Mac onto this mesh. Refuses if no network key
is set yet (`tcr peer network-key set` first): a link with nothing to carry names `tcr peer
invite` as the headless alternative instead.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--invite` | bool | `false` | also mint a one-use join key and carry it in the link, so opening it also completes pairing. **Turns the link into a live bearer secret with ten minutes on it**: without this flag it carries only the network key |
| `--label <name>` | string | none | a name for the joining Mac, when `--invite` is given |

### `tcr peer moved <mint\|open> [peer\|link]`

The link a Mac sends one friend after it changed networks: it carries this Mac's current
addresses, sealed so only that one friend's Mac can read them. It joins nothing, grants nothing
and pairs nothing. See [peers.md](peers.md) § "When both Macs moved".

| flag | type | default | effect |
|---|---|---|---|
| `<action>` | enum | | `mint` prints one link for one already-trusted Mac; `open` reads a link somebody sent and says which of this Mac's peers it is from and where that Mac now is |
| `[peer\|link]` | positional | | for `mint`: the peer id to seal for, in its full wire form, the `node` field of `tcr peer ls --json`. For `open`: the `tcr://peer/moved?…` link. **A link typed here is visible in `ps` and in shell history**, and `open` says so on stderr: use `--stdin` |
| `--stdin` | bool | `false` | for `open`: read the link from standard input, one line, so it never enters argv. The only path the panel and the `tcr://` URL handler use |
| `--yes` | bool | `false` | for `open`: actually write the addresses. Without it, `open` reads the link, says what it would add, and changes nothing. No effect on `mint` |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use (and the runtime-state file beside it, which is where the router mapping `mint` advertises is read from) |

`mint` seals for one Mac at a time, under a key derived from the secret that pair already
shares. It refuses a Mac this one has never completed a session with since that secret existed,
and the refusal carries the fix: the two have to talk once, over any address that works, before
either can seal for the other. The link carries at most this Mac's listen socket and the router
mapping a serving process holds; it never carries the address that peer last said it sees this
Mac at, which is the address this Mac had *before* it moved.

`open` without `--yes` writes nothing, the same split `tcr peer id --regenerate` makes with its
own `--yes`. What it can add is bounded: at most two addresses, on a row this Mac has already
pinned, at the lowest confidence band there is. It never creates a row, never un-forgets a Mac,
never touches a network key and never turns a switch on. Pasting the same link twice writes the
file once.

```
$ tcr peer moved mint 0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G
tcr://peer/moved?v=1&r=6YFN2TQ8ZKW...
peer moved: sealed for studio-mac; only that Mac can read it, and it goes stale in 24 hours
peer moved: it carries 2 address(es) and nothing else: it joins nothing, grants nothing and pairs nothing

$ tcr peer moved open --stdin < link.txt
peer moved: from studio-mac (tcr-0W3GE1R70W), sealed 372s ago
peer moved: would add 192.0.2.7:41234
peer moved: would add [2001:db8::1]:41234
peer moved: nothing written; pass --yes to keep these

$ tcr peer moved open --stdin --yes < link.txt
peer moved: added 2 address(es) to studio-mac; nothing else changed
```

Every refusal exits non-zero except `already-known`, which is the ordinary outcome of pasting a
link twice. A refusal about a link that arrived names nothing: not the peer it might have been
for, not an address, not any part of what was pasted, because a link forwarded into the wrong
group chat must teach its reader nothing about who this Mac knows.

### `tcr peer reach`

Prints what this Mac can be reached on from off the local network. Read-only: nothing here
dials a peer or touches the running proxy.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--map` | bool | `false` | also ask the router for a NAT-PMP mapping on the peer listener's port, held for two minutes |
| `--json` | bool | `false` | machine-readable output |

It prints, in order: this Mac's global IPv6 addresses (none, if it has none); the router's
NAT-PMP answer, meaning the gateway found, the external address, and the mapping outcome when
`--map` was passed (a refusal is an ordinary outcome, not an error); the local listen port;
the current time-derived port slot; and, per pinned peer, that peer's derived port for this
slot. A refusal from the router, or a peer row with no derived port yet, still exits 0.

### `tcr peer graph <--json\|--serve>`

Prints, or serves, the mesh as this Mac currently sees it: pinned peers, the paths to each,
and each path's RTT and loss. Read-only, same as `tcr peer reach`.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--json` | bool | `false` | print the graph once, machine-readable, and exit |
| `--serve` | bool | `false` | bind loopback only and serve one page redrawing the graph from the same data every 5 seconds, nodes and edges coloured by RTT and loss. Refuses to bind anything but loopback: this is a page for the Mac it runs on, not the mesh. See [peers.md](peers.md) § "Seeing the mesh's paths (not in this release)" for why there is no mesh-wide version yet |

`--json` and `--serve` are mutually exclusive; one of the two is required.
