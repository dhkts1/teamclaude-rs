# Peers: sharing accounts with other Macs on your network

`tcr` can find other Macs running `tcr` on the same network, trust them, and let them share
Claude accounts with each other. Nothing here is on by default. Back to
[the README](../README.md); the `tcr peer` commands are documented flag-by-flag in
[cli.md](cli.md), and the `tcr-peers.json` file this all lives in is documented in
[configuration.md](configuration.md).

## Find: seeing other Macs

```
tcr peer find on
```

turns on discovery. Other Macs running `tcr peer find on` on the same network start showing
up as rows you can Trust. Discovery announces this Mac's presence and port to the network so
it can be found back; see "what leaves this Mac" below for exactly what that announcement
does and does not carry. Up to 12 discovered rows are shown at once, newest first, and at
most 2 from the same address; a Mac that stops announcing drops off the list after a minute.

```
tcr peer find off
```

turns discovery back off. This Mac still answers a pairing request from a Mac that already
has its address, but it stops announcing itself and stops looking for others.

By default a discovered row shows only presence, not this Mac's name. Turn the name on with:

```
tcr peer find on --announce-name on
```

Either way, the announcement never carries this Mac's identity: no key, no peer id. A name
is the only non-random thing it can carry, and it is opt-in, off unless asked for.

The switch is a setting, not an act: `tcr peer find` writes it and the running `tcr` does the
announcing, checking the setting about every twenty seconds. So turning it on takes effect
within that, turning it off stops the announcement within a minute, and neither needs a
restart. With no `tcr` running, nothing announces however the switch reads.

## Trust: pairing with a Mac

Trusting a Mac is a two-sided act: nothing happens until both operators agree.

1. On your Mac, ask to pair:

   ```
   tcr peer pair <address>
   ```

   This sends a pairing request ("wants to pair") to the Mac at that address and prints your
   six digits.

2. On the other Mac, see the request:

   ```
   tcr peer pending
   ```

   lists every Mac asking to pair, by name (if it sent one) and address. A request sitting
   here is not a trust decision; it is just a request waiting for an answer.

3. The other operator answers it:

   ```
   tcr peer accept <target>
   ```

   opens a two-minute window during which both screens show the same six digits. Compare
   them, and if they match, both operators run:

   ```
   tcr peer pair <address> <the six digits>
   ```

   on the asking side, and the other side confirms. Once both sides confirm, the peer is
   pinned: `tcr peer ls` will show it, and it stays trusted until someone runs
   `tcr peer forget`.

The six digits are built from a random value contributed by each Mac, and the answering Mac
commits to its value before it is shown the other's. That is what makes comparing them worth
doing: neither side, and nothing in the middle, can steer the digits after seeing what the
other picked. A Mac running a version from before this change is told so by name and asked to
update, rather than being paired with on digits one side could have chosen.

A pairing request can also be turned down (`tcr peer ignore <target>`, quiet for an hour) or
refused permanently (`tcr peer block <target>`, which also bans the peer's key once one has
been learned). `tcr peer unblock <addr>` lifts a block.

Requests are rate-limited so one Mac cannot flood another: at most 8 outstanding requests are
kept at once, and no more than one arrives from the same address every 10 seconds (a short
burst of 3 is allowed).

## Share: letting a trusted Mac use your accounts

Trusting a Mac lets it reach you; it grants nothing on its own. Sharing is separate and it is
the one place a trusted Mac gets to read your requests, prompts included:

```
tcr peer share on
```

turns sharing on for every currently trusted peer, with a default lend of one grant: 10% of
the weekly window, drawn from every account (`all`), each lease living 600 seconds before it
needs renewing and at most 2 borrowed requests in flight at once. Adjust the flags to change
those defaults, or grant one peer a different amount:

```
tcr peer lend <peer> --window 7d --fraction 0.20 --ttl 600 --max-inflight 2
```

A Mac may hold several leases at once, one per scope, so lending "20% of the `work` group's
weekly window" alongside "all of account A's Fable weekly" is two leases on one peer, not one
replacing the other. `--scope` picks what one grant draws from:

```
tcr peer lend <peer> --scope group:work --window 7d --fraction 0.20
tcr peer lend <peer> --scope account:studio-mac --window 7d_oi --fraction 0.5
```

`--fraction` is clamped to `0.0..=0.5`, the same ceiling the main config applies to its own
reserve; a higher value is accepted, clamped down, and the clamp is printed so you can see it
happened rather than believing the file holds the number you typed.

`--scope all` (the default) draws from every account; `--scope group:<name>` from one
`tcr group` group, pooled; `--scope account:<label>[,<label>]` from one or more named
accounts, by the sanitized label `tcr status` prints, never an email or a uuid. `tcr peer lend
<peer> --list` prints every lease this peer holds, one greppable line each, with the lease id
`--revoke` and `--relend` take; this is the same list the panel's per-Mac sheet shows as
**Lend from**.

A lease can also be time-bounded: `--for 2h` (or `--until 18:00`) stops renewing it at that
time, after which the peer's row shows "ended"; the default is no end. An ended lease stays in
the list, greyed, so it can be re-lent with `--relend <lease-id>` instead of typed again from
scratch.

`--fraction 0` removes the grant for that window and scope, on `tcr peer lend` for one peer
and on `tcr peer share on` for every trusted Mac at once; it never mints a grant of nothing.
`tcr peer share off` turns sharing off for every peer at once.

Run `tcr peer share on` again over a Mac that already has a grant for the same window and
scope and it keeps that grant's sharing mode, its end date and its daily hours, changing only
what you passed. Those are decisions taken per Mac, and a re-run used to quietly reset all
three: a `hand` grant would turn back into a `serve` one, which is a different decision about
who reads the requests.

When your Mac is lending, a request that peer sends is served on YOUR account, by YOUR `tcr`:
the borrowing Mac's own credential never crosses the network. `tcr peer ls` shows what is
currently lent, to whom, and what is in flight. An account that is lent out gets its own
read-only line in the Accounts tab too, "Lent to attic-nuc 20 %", pointing back at the peer
holding the lease; a full fraction shows the window's name instead of "100 %" (`Lent to
studio-mac Fable weekly`), and an account lent to more than one peer shows the first two,
"and N more".

### Sharing modes

A lease can work one of two ways:

- **`serve`** (today): the borrower's request goes over the owner's Mac and out on the owner's
  IP. The owner's `tcr` reads it, prompts included.
- **`hand`** (`tcr peer lend <peer> --mode hand`): the owner hands the borrower a short-lived
  access token over the paired, authenticated session. The borrower sends the request on its
  own IP, so the owner never reads it. The owner keeps the refresh token and renews it; to
  revoke, the owner just stops renewing.

Only one machine ever holds the refresh token, whichever mode is in use.

## Reaching the internet through a peer

```
tcr peer via auto
```

lets this Mac fall back to a trusted, willing Mac when its own connection to the internet is
down. `tcr peer via off` turns that off; `tcr peer via <peer>` pins one specific Mac instead
of picking automatically.

## Exit lock: keeping an account on one IP

```
tcr peer account <account> --exits-from local|<peer>
```

sets the account to `egress: local` or `egress: via <peer>`. Local is today's behaviour: the
account's requests leave from this Mac. `via <peer>` sends them out through one pinned peer
instead, always the same one, so that account's traffic keeps a single IP no matter which Mac
in the mesh happens to be running it. This matters because some accounts need to look like they
always come from the same place. `--must` (or `--no-must`) sets `egressStrict`: with it on, a
request is refused by name rather than falling back to this Mac when the pinned peer cannot be
reached.

The peer carrying the traffic sees only where the bytes are going, never what is inside them:
it carries them blind. The exit lock itself lives on the account, in the main config file, and
is never sent over the wire.

## The network key: an opt-in password for the mesh

On an open office network, discovery and pairing requests are visible to every Mac that can
reach you, not just the ones you'd choose. The network key closes that:

```
tcr peer network-key set
```

mints a 52-character key (32 bytes, Crockford base32) and prints it once. Every other Mac
in the group pastes it in:

```
tcr peer network-key join <the key>
```

Once set, a Mac without the key sees no discovery rows from this mesh and cannot get a
pairing request through at all: it is a password for being seen and heard, not an identity.
`tcr peer network-key clear` removes it, which puts this Mac back to being visible and
reachable to every `tcr` on the network. `tcr peer network-key show` says whether one is set
without printing it.

Setting or joining a key while one is already set is refused, because replacing it cuts this
Mac off from every other Mac still holding the old one; pass `--replace` to mean it, or clear
the key first.

## Share link: onboarding a Mac in one paste

A share link carries the network key, so mint one first (`tcr peer network-key set`). Without
one, `tcr peer link` refuses and names `tcr peer invite` as the headless alternative.

```
tcr peer link
```

prints one link (`tcr://peer/join?...`) that carries the network key. Pasting it into another
Mac's browser, or running `tcr peer join <link>`, sets that Mac's network key with no typing.

```
tcr peer link --invite --label "new-mac"
```

mints a one-use, ten-minute join key and rides it inside the same link, so opening it also
completes pairing with this Mac, with no six-digit compare needed. Because that link is a live
secret for as long as it is valid, send it somewhere as private as you would a password.

`tcr peer invite` mints the same kind of one-use join key on its own
(`tcr-join:v2:<addr,addr,...>:...`), for a Mac with no browser to open a link in. The key
carries every address this Mac answers at, best first, and the joining Mac works down the
list, so the same key reaches a friend on the tailnet and a friend in the next room. A key
from an older build carried one address, and this build still reads one of those.

`tcr peer join --stdin` (or `--stdin` on `network-key join`) reads a key from standard input
instead of the command line, so it never lands in shell history or in another process's view
of this one's arguments.

A Mac that already has a network key **refuses a link that carries a different one**, and says
what accepting it would cut this Mac off from. Pasting a second office's key over the first is
the commonest way a Mac vanishes from its own mesh, and it used to happen without a word. Pass
`--replace` to mean it.

## Reaching a Mac off your network

At pairing, and again at every handshake after, each Mac writes down the address it saw the
other one connect from: "I saw you from `ip:port`". That address is refreshed every time you
talk, so it is what lets you find a peer again once it has left your network for a new one.

Reaching it tries a few things, in order:

- **IPv6 first.** If both Macs have a public IPv6 address, they reach each other directly,
  with nothing in between.
- **A stable router mapping.** If not, and the internet switch (`tcr peer internet on`) is on,
  `tcr` asks your router to open one fixed port for the peer listener, over NAT-PMP or, for a
  router that only speaks the older protocol, UPnP, and keeps it open: the mapping is renewed
  every 30 minutes on a 2-hour lifetime, so a single missed renewal does not drop it, and it is
  deleted both when the switch goes off and at shutdown, so nothing is left holding a port open
  on your router. Like `find`, the switch is a setting rather than an act: the running `tcr`
  re-reads it every few seconds, so turning it on asks the router within that and turning it
  off deletes the mapping, neither needing a restart.
  `tcr peer reach` prints what your router agreed to.

  Routers turn NAT-PMP down in two ways: most say nothing at all, and some refuse the request
  outright. Both mean the same thing, that there is no NAT-PMP service there to ask, so both
  are followed by the UPnP attempt. A router that will not open a port over either protocol is
  asked again more and more slowly, after 5 seconds, then 30, then 2 minutes, then every 10
  minutes, rather than every few seconds all day; the wait goes back to 5 seconds the moment
  anything could have changed, when you turn the switch off and on again, when this Mac moves
  to another router, or when a request is granted. What you see in the log is one line when
  the answer changes and nothing more until it changes again.

  If neither protocol will open a port, two things still work: forward the peer listener's port
  to this Mac by hand in your router's own settings, or put both Macs on one private network
  (Tailscale, or any VPN they both join) and pair over that.
- **A fallback port that changes with the clock, for the dialling side only.** If
  the address this Mac last saw the peer at stops answering, both sides already know, without
  saying so to each other, a small set of ports that are "accepted" for right now: they are
  computed from the pair's own Noise handshake and the current half-minute, the same idea
  behind a 2FA code that changes on its own. The Mac doing the dialling tries the pair's three
  accepted ports for the current 30-second slot before giving up. Nothing here opens a second
  listener and nothing invents a second secret: the port comes straight out of material both
  sides already hold from pairing, so it is a rendezvous hint for the dialler, not a new way
  in. Each Mac keeps its own copy of that pair's secret in `tcr-peers.json`, so a restart
  does not lose the fallback for every peer at once; it is a value derived from the
  handshake and never the handshake material the pairing code is built from, and it is
  never sent to anybody, since both sides work it out for themselves.
- **A friend forwards for you.** If both Macs moved, or one sits behind carrier-grade NAT that
  no router mapping can open, a third Mac you have both pinned can pass the encrypted bytes
  between you. It cannot read them, only carry them, and only for one hop.

All of this is opt-in per Mac, off by default, and only for a Mac you have already paired with.
Over the internet, this Mac never answers a bare knock: only a handshake proving the peer holds
the key it was pinned with gets a reply. There is no server anywhere in this. `tcr` never asks a
Mac neither of you trusts to hold or relay anything.

Expect this to fail sometimes: if both Macs are on cellular data with no public address and no
shared friend to forward through, there is nothing left to try, because there was never a server
standing by to fall back on.

### When both Macs moved

Sometimes there is nothing left to try. Both Macs changed networks, neither has a public
address, and the addresses on each other's rows are where the other one used to be. Nothing on
this list can fix that by itself: every mechanism above needs one end to be findable.

What fixes it is you, over the chat you already use. On the Mac that moved:

```
tcr peer moved mint <the other Mac's peer id>
```

prints one line, `tcr://peer/moved?v=1&r=…`, and you send it to that one friend. It says where
your Mac is now, sealed so that only their Mac can read it: to anyone else, including whatever
service carries the message, it is a couple of hundred characters that say nothing. It is not a
share link. It brings nobody onto your mesh, hands over no key, grants nothing, and pairs
nothing. Send it to the wrong person and their Mac refuses it, in a sentence that tells them
nothing about you.

On their side:

```
tcr peer moved open --stdin < link.txt
```

reads it and says which of their Macs it is about and where that Mac now is. It writes nothing
until they pass `--yes`, and even then all it can do is add up to two addresses to a Mac they
had already paired with, at the lowest confidence there is, dialled after everything their own
Mac worked out for itself. It cannot create a row, bring back a Mac they told `tcr` to forget,
or turn anything on. Pasting it twice changes nothing the second time.

One link repairs both directions: once their Mac dials yours, yours learns where theirs is from
the connection itself. A link goes stale after a day, because by then you have probably moved
again; if it does, send another one.

What goes into the link is the same set `tcr peer invite` puts in a key: your tailnet address if
you have one, the external socket your router mapped if it mapped one, and the addresses your
own interfaces hold. The port your listener is configured with is not itself an address, so a
Mac listening on every interface publishes the interfaces rather than the wildcard: `0.0.0.0` is
where a listener binds, never somewhere a friend can dial.

Two cases leave nothing to send. A pair that has not completed a single session since this
build's shared secret existed has no key only the two of them hold, so `mint` refuses and says
so; reach that Mac once on any address that works, and then it will seal. And a Mac with no
address anyone could dial, on no network and with no router mapping, gets a refusal naming
`tcr peer reach` and `tcr peer internet on` rather than a link that would fail on the far side.

### Friends of friends: refreshed, never introduced

The rule: "friends through friends are not supported unless you both
support the same friend." In practice: if a Mac you trust tells you where a Mac you *also*
already trust was last reached, that refreshes the endpoint on the row you already have for
it. It cannot hand you a Mac you have never pinned yourself: any key in that brief that is not
already one of yours is dropped the moment it arrives, never shown to you and never written
anywhere.

### Seeing the mesh's paths (not in this release)

The plan is a live graph: `tcr status --json` gaining RTT, loss and a bytes/tokens rate per
path, and one nominated Mac serving that graph as a page so the whole mesh can watch it. That
serving page is not in this release. Handing socket addresses and lease ids to every Mac in
the mesh is a disclosure surface of its own, on top of everything else here, and it still
needs a frame the path-probing work owns. The per-Mac numbers themselves, once they land, show
up first in the Peers tab.

One of those two numbers is going to read zero for a while, and that is not a broken counter.
A share you grant today is a **share of a window**: "a fifth of my weekly quota". Nothing on
either Mac can say how many tokens a fifth of a window buys, because that depends on what the
requests turn out to be, so a per-path token figure for such a share is honestly zero rather
than a guess. Only a share written **in tokens** would carry one, and this release refuses
those outright: ask for one and the answer is a refusal, not a smaller lease. The bytes figure
is real today for every share. A `hand` share does get the borrower's own report of what it
spent, sent back over the paired session, but that lands on the share's remaining balance and
is still a slice of a window, so it does not put a token number on a path either.

## What leaves this Mac

| act | what it sends | who can see it |
|---|---|---|
| announcing (`find on`) | a random per-boot id, this Mac's port, the wire version, and its display name (only if `--announce-name` is on) | anyone on the local network; never a key or a peer id |
| pairing request (`pair`) | this Mac's address reaching out over a real TCP connection, the six-digit compare | only the Mac you are pairing with |
| being trusted (pinned) | this Mac's public key, permanently, to the peer that pinned it | that one peer |
| sharing (`share on` / `lend`) | the full content of a borrowing peer's requests, served on this Mac's account | only a peer you granted `inspect`/lend to |
| carrying (`via`) | encrypted bytes this Mac cannot read, on the way to the address you allow-listed | the gateway peer sees which host, not what is inside |

Nothing above is sent before the matching switch is turned on, and every one of them can be
undone: `tcr peer find off` stops announcing, `tcr peer share off` (or `tcr peer lend <peer>
--fraction 0`) stops lending, and `tcr peer forget <peer>` removes a pin entirely.

## Trying it on two Macs

This is the page to type from with a friend, one command at a time, nobody to ask. Every
line below says which Mac types it. Mac A and Mac B stand for the two machines; swap in
your own names.

Both Macs need a peer port: `listen`, a `host:port` in `tcr-peers.json` (see
[configuration.md](configuration.md) for the key). The first opt-in command below writes it
for you, `0.0.0.0:7755`, if nothing has set it already; it only takes effect the next time the
proxy starts, so quit and reopen it once after that first command, before going any further.

### On one LAN

1. Both Macs, same network:

   ```
   tcr peer find on
   ```

   If this Mac had no peer port yet, it prints `peer.listen: 0.0.0.0:7755 written into <path>
   (this Mac had no peer port, and turning this on needs one)` followed by `peer.listen:
   nothing is listening on it yet. The port opens the next time the proxy starts, so quit
   TcrBar and open it again to finish turning this on`. Do that before the next step. On a Mac
   that already had `listen` set, neither line prints and `peer.find: on` takes effect right
   away, followed by `a running server starts announcing within a minute`.

2. Mac A asks to pair with Mac B:

   ```
   tcr peer pair <Mac B's address>
   ```

   Mac A's screen shows `peer pair: this Mac shows` followed by six digits.

3. Mac B sees the request and opens the window:

   ```
   tcr peer pending
   ```

   lists a row containing `wants to pair`.

   ```
   tcr peer accept <target>
   ```

   (`<target>` is the instance id or address `tcr peer pending` printed.) Mac B's screen shows
   `peer accept: ok` followed by the instance id, address and the window's end time.

4. Compare the six digits on both screens. If they match, both operators confirm, each typing
   the address of the OTHER Mac:

   ```
   tcr peer pair <Mac B's address> <the six digits>     # on Mac A
   tcr peer pair <Mac A's address> <the six digits>     # on Mac B
   ```

   Full details of this exchange, including why the digits cannot be steered, are in
   [Trust: pairing with a Mac](#trust-pairing-with-a-mac) above; the four commands above are
   the whole of what to type.

5. Either Mac:

   ```
   tcr peer ls
   ```

   prints a row for the other Mac's peer id and name once both sides confirmed, followed by a
   line `peer ls: pending=0 blocked=0 muted=0`.

6. Mac A lends Mac B one hour of account access. `tcr peer lend` takes the peer id in its
   full wire form, not the short `tcr-…` one `tcr peer ls` just printed, so read it off:

   ```
   tcr peer ls --json
   ```

   and take Mac B's `node` field from there. Then, on Mac A:

   ```
   tcr peer lend <Mac B's node id> --for 1h
   ```

   prints `peer lend: ok lease=` followed by the lease id, peer, scope, window, fraction,
   ttl, max-inflight and end time.

7. Mac B borrows once. There is no separate "borrow" command: once Mac A has lent, a normal
   request Mac B sends through its own `tcr` is served on Mac A's account whenever Mac B needs
   it (see [Share: letting a trusted Mac use your accounts](#share-letting-a-trusted-mac-use-your-accounts)
   above for what that means for who reads the request).

8. See the borrow. The plain `tcr peer status` line only carries the peer's name, address,
   last-seen time and whether it is trusted, not lease detail, so ask for the JSON on Mac A:

   ```
   tcr peer status --json
   ```

   Mac B's row there carries `inFlight`, `leaseSpent` and `leaseTtlSeconds` for the request
   just served.

### Off the LAN

1. On both Macs:

   ```
   tcr peer internet on
   ```

   If this Mac had no peer port yet, it first writes one the same way `tcr peer find on` does
   above (the same two `peer.listen:` lines; quit and reopen once for it to take effect), then
   prints `peer.internet: on (the listener on port 7755 is mapped at boot and renewed every 30
   minutes)`. On a Mac that already had `listen` set, only that last line prints, with
   whichever port it was already set to.

2. If both Macs have a public IPv6 address, they reach each other directly and the same four
   pairing commands from "On one LAN" above work unchanged, typed against the address each Mac
   last saw the other at (see [Reaching a Mac off your network](#reaching-a-mac-off-your-network)
   above). Either Mac checks whether it has one first:

   ```
   tcr peer reach
   ```

   prints `reach: ipv6: <addr>` for each global address it has, or `reach: ipv6: none: this
   Mac has no globally routable IPv6 address, so a peer cannot reach it over IPv6` if it has
   none.

3. If neither Mac has IPv6, the router mapping from step 1 is the fallback: it opens a fixed
   external port on your router pointing at the peer listener, so the four pairing commands
   work against `<your router's external address>:<that port>` instead. `tcr peer reach` shows
   what your router agreed to:

   ```
   tcr peer reach
   ```

   prints `reach: gateway: <addr>`, `reach: external-address: <addr>`, `reach: mapping: not
   asked for (pass --map)` unless `--map` is given, and `reach: listen-port: <port>`, or
   `reach: listen-port: none (the peer listener is off)`. `tcr peer reach --map` asks the
   router directly instead of reading what a running `tcr` already holds.

   The same command also prints the current clock-derived fallback slot: `reach: slot: <n>
   (30s each, the slot before and after also accepted)`. That slot is the port both sides
   already fall back to dialling, without telling each other, if the address they last saw
   each other at stops answering. A standalone `tcr peer reach` cannot show you the derived
   port itself, because it has not run the pair's Noise handshake and has nothing to derive
   one from, so it prints `derived-port: unavailable: the pair's handshake secret is not
   stored, and deriving a port from public key material instead would give every scanner the
   same number` for every pinned peer. The fallback itself runs inside the already-paired
   `tcr` process, not from this command.

### A hand-mode lend

Mac A lends Mac B the same hour of account access as step 6 in "On one LAN" above, but with
`--mode hand`:

```
tcr peer lend <Mac B's node id> --for 1h --mode hand
```

prints the same `peer lend: ok lease=` line as that step, unchanged in shape; the mode itself
is not one of the printed fields. From here, a request Mac B sends is served by Mac B's own
`tcr`, on a short-lived access token Mac A handed over the paired session: Mac A never reads
it (see [Sharing modes](#sharing-modes) above).

If every account the scope covers has `--must` set (an exit lock that refuses rather than
falls back), this refuses instead of lending, before anything is written:

```
peer lend: every account this scope covers has egressStrict on, so a hand-mode grant would
hand over a bearer the exit lock refuses to send with: the borrower sends from ITS own Mac,
which a strict pin forbids whether it names this Mac or another. Lend it as `--mode serve`,
or clear the pin on an account in <scope> first
```

Seeing the borrow is different from a `serve` lease: no request passes through Mac A's proxy,
so `tcr peer status --json` on Mac A will not show `inFlight` for it. Mac B's own `tcr` still
reports what it spent back to Mac A over the paired session, which is what keeps the lease's
remaining balance accurate (see [Seeing the mesh's paths](#seeing-the-meshs-paths-not-in-this-release)
above for what that report can and cannot say about it).
