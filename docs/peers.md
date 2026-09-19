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
- **`hand`** (arriving): the owner hands the borrower a short-lived access token over the
  paired, authenticated session. The borrower sends the request on its own IP, so the owner
  never reads it. The owner keeps the refresh token and renews it; to revoke, the owner just
  stops renewing.

Only one machine ever holds the refresh token, whichever mode is in use.

## Reaching the internet through a peer

```
tcr peer via auto
```

lets this Mac fall back to a trusted, willing Mac when its own connection to the internet is
down. `tcr peer via off` turns that off; `tcr peer via <peer>` pins one specific Mac instead
of picking automatically.

## Exit lock: keeping an account on one IP (arriving)

Each account can be set to `egress: local` or `egress: via <peer>`. Local is today's
behaviour: the account's requests leave from this Mac. `via <peer>` sends them out through one
pinned peer instead, always the same one, so that account's traffic keeps a single IP no
matter which Mac in the mesh happens to be running it. This matters because some accounts need
to look like they always come from the same place.

The peer carrying the traffic sees only where the bytes are going, never what is inside them:
it carries them blind. The exit lock itself lives only in the local peers file and is never
sent over the wire.

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

`tcr peer invite` mints the same kind of one-use join key on its own (`tcr-join:v1:...`), for
a Mac with no browser to open a link in; `tcr peer join --stdin` (or `--stdin` on
`network-key join`) reads a key from standard input instead of the command line, so it never
lands in shell history or in another process's view of this one's arguments.

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
- **A stable router mapping (arriving).** If not, and the internet switch (`tcr peer internet
  on`) is on, `tcr` asks your router to open one fixed port for the peer listener (NAT-PMP;
  UPnP is arriving) and keeps it open: the mapping is renewed every 30 minutes on a 2-hour
  lifetime, so a single missed renewal does not drop it, and it is deleted both when the
  switch goes off and at shutdown, so nothing is left holding a port open on your router.
  `tcr peer reach` (arriving) prints what your router agreed to.
- **A fallback port that changes with the clock, for the dialling side only (arriving).** If
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
