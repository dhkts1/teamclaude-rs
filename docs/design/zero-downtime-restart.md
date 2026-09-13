# Design: hand the listening socket to the successor

Written 2026-09-13, before any implementation. Companion to the change that made
`takeover_port`'s grace windows ceilings rather than floors (#255).

## The problem, measured

A restart unbinds the port. Measured 2026-09-13 across three restarts, from the
incumbent's `no longer accepting` to the successor's `server started` in
`~/.cache/teamclaude/logs/`:

| restart | gap |
|---|---|
| 09:17 | 1.124 s |
| 09:18 | 0.976 s |
| 10:38 | 1.020 s |

Roughly 0.8 s of each was a flat `sleep` that #255 removed. What remains is the
irreducible part: the successor cannot bind until the incumbent has released the
port, so there is always a window where connections are refused.

Restart frequency is low. Boots per day from the retained logs: 0, 0, 6, 3. So
this is a two-to-three-second-per-day problem, and that is the honest budget
against which the work below should be judged.

## What survives a restart today

Not the problem, and worth stating so nobody re-solves it:

- session→account pins, via `affinity.rs` (5 s debounced flush, restored before the bind)
- the usage ledger, replayed at boot
- rotated OAuth tokens, persisted incrementally
- in-flight connections: `ServerHandle::shutdown_within` cancels only the accept
  loop, and each connection is a detached task, so streams are not cut

## The constraint that shapes everything

**Two live proxies must never both mutate account state.** OAuth refresh tokens
are single-use, so two processes each holding an in-memory copy will invalidate
each other's — the "token war" that the whole of `singleton.rs` exists to
prevent. Any design where the old and new process overlap has to answer, at a
precise instant, which one owns mutation.

This is why `SO_REUSEPORT` is the wrong tool here despite being less code. Two
independent accept loops on the same port have no such instant: both processes
are simply running, and the ownership question is answered by timing rather than
by a protocol.

## The sequence

The predecessor keeps the bound socket alive across the swap by handing a
duplicate of it to the successor. The port's listener refcount never reaches
zero, so it is never unbound, and connections that arrive mid-swap wait in the
kernel backlog instead of being refused.

1. Successor starts, finds the incumbent's handoff socket beside its existing
   `proxy-owner-<port>.json`, and connects.
2. Predecessor stops accepting, and stops every background loop (the existing
   shutdown watch channel already does both).
3. Predecessor flushes affinity pins and persists config. This is its **final**
   write, made while it is still the owner, so the successor reads a current
   file rather than one up to 5 s stale.
4. **Predecessor releases mutation ownership.** From here it refuses to refresh
   a token and refuses to persist config, whatever an in-flight request or its
   own shutdown path asks of it.
5. Predecessor sends the listening fd over `SCM_RIGHTS`.
6. Successor restores pins from the just-flushed file and begins accepting on
   the received fd. It never binds.
7. Predecessor drains in-flight connections and exits.

Ownership transfers at step 4, before the successor is serving anything, so
there is no instant at which both processes believe they may mutate.

### Why the order is flush-then-release, and not the reverse

Both neighbouring orderings are broken, in opposite directions, which is why
this is spelled out rather than left to the implementer:

- **Release before the flush** and the final pin and token write never happens.
  The successor then starts from a stale file, which is the cold-cache cost this
  whole design exists to avoid.
- **Never release at all** and the predecessor clobbers the successor.
  `ServerHandle::shutdown_within` ends by calling `Manager::persist_now`, so the
  predecessor's ordinary shutdown path fires a config write *during the drain*,
  after the successor is already serving and rotating its own tokens. That write
  carries the predecessor's now-stale in-memory copy. It is exactly the
  "boot-time tokens written back over the fresh ones" failure that
  `singleton.rs` documents, arriving through a new door.

So the release has to sit strictly between the last write the predecessor is
entitled to make and the first one the successor will make.

## What enforces step 4

A flag on `Manager`, checked at the single point where each mutation is
decided: `refresh_plan` (which `ensure_fresh_inner` already returns early on),
`persist_now`, and `persist_tokens`. Released state is terminal; nothing sets it
back.

It has to be a flag rather than an assumption, because step 7 is a drain, and a
draining request can still reach `ensure_fresh` and rotate a single-use token.
That is the token war in miniature, and the only thing standing between the
design and it is this check.

## What the test must catch

Not "the handoff works". The test that matters is the one that fails if step 4
is missing: drive a request through a predecessor that has already handed off,
and assert no refresh was attempted. Watch it fail against a build with the flag
removed before trusting it.

Second test: the port is never unbound. Hold a connection open across a handoff
and assert it is served, and that a connection opened mid-swap is accepted
rather than refused.

## Who is allowed to receive the socket

The handoff gives the peer the live listening socket of the proxy that holds
every account's OAuth credentials. Whoever holds it serves Claude Code traffic,
and every request carries a bearer token. So the question is not rhetorical: a
handoff socket with no peer check turns "can run code as this user" into "owns
the Anthropic credentials", which is an escalation the rest of this system
takes some trouble to prevent.

File permissions are the floor and not the answer. The socket lives `0600` in a
`0700` directory, which stops other *users* and does nothing about other
processes belonging to this one — and this one is exactly the account every
locally-running tool already has.

So the predecessor verifies the peer's **code signature** before sending the
descriptor, and the successor verifies the predecessor's before adopting it.
Both binaries already carry one:

```
$ codesign -dv --verbose=2 /Applications/TcrBar.app
Authority=Developer ID Application: Gil Portnoy (UJQ3GQF56Y)
TeamIdentifier=UJQ3GQF56Y
```

The requirement to check is team-scoped rather than bundle-scoped:

```
anchor apple generic and certificate leaf[subject.OU] = "UJQ3GQF56Y"
```

Bundle-scoped would be wrong here, because a CLI `tcr` handing over to a TcrBar
(or the reverse) is a supported and ordinary case, and those have different
identifiers.

### Use the audit token, never the pid

`getsockopt` offers `LOCAL_PEERPID` (`sys/un.h:0x002`) and it is the wrong one.
A pid can be reused, and a peer can `exec` between the moment it is checked and
the moment it is trusted, so a pid-keyed check validates one program and then
hands the socket to another. This is the classic form of the bug and Apple
documents it as such.

`LOCAL_PEERTOKEN` (`sys/un.h:0x006`) returns the peer's **audit token**, which
identifies the process instance rather than a reusable number. Feed it to
`SecCodeCopyGuestWithAttributes` as `kSecGuestAttributeAudit`, then
`SecCodeCheckValidity` against the requirement above. Verified present on the
macOS SDK this repo builds against.

### Unsigned builds do not get a handoff, and that is correct

A `cargo build` binary is not Developer ID signed, so a development proxy fails
the requirement. It therefore does not receive the socket, stands down, and says
why — which is the same path an older or wedged incumbent takes, already
described below. An operator who genuinely wants a dev build to take the port
uses `--replace`, explicitly, and pays the sub-second window. The alternative
(an escape hatch that waives the check when the peer is unsigned) would waive it
for an attacker too, since an attacker's binary is also unsigned.

## Fallback: explicit, never automatic

The handoff needs both sides to speak it, so it cannot be the only path:

- an incumbent older than the protocol has no handoff socket
- the legacy JS proxy will never have one
- a wedged incumbent will not answer
- the first upgrade onto this protocol, by construction

When the handoff is unavailable the successor **stands down and says why**. It
does not silently fall back to signalling. `--replace` remains the explicit
escape hatch, and #255 is what makes that path fast. This matches how
`singleton.rs` already treats the kill: default to standing down, put the
signal behind an explicit flag.

## Dependency

`SCM_RIGHTS` is not in `std`. Two options, both already present in `Cargo.lock`
so neither adds a package to the build graph:

- **`nix` with `socket` + `uio`** — `ControlMessage::ScmRights`, safe wrapper,
  `x86_64-apple-darwin` supported. **Recommended.**
- raw `libc` `sendmsg`/`recvmsg` — no new dep line, considerably more `unsafe`
  for something a maintained crate already gets right.

Taking `nix`, on the standing rule not to hand-roll what a library already does.
Flagged here rather than buried in a diff, because a new direct dependency is a
decision and not an implementation detail.
