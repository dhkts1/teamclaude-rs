# Can two proxies hold port 3456 at once? Platform-dependent. (σ4, measured 2026-08-08)

Reproduce: `rustc scripts/bind-overlap-probe.rs -o /tmp/bp && /tmp/bp`, and for Linux
`docker run --rm -v $PWD/scripts/bind-overlap-probe.rs:/probe.rs:ro rust:alpine \
  sh -c "rustc /probe.rs -o /tmp/bp && /tmp/bp"`.

The probe calls `std::net::TcpListener::bind` — the same constructor as `server.rs:784` —
and prints the socket options rather than assuming them.

| first bind | second bind | macOS | Linux (musl) |
|---|---|---|---|
| `127.0.0.1` | `127.0.0.1` | REFUSED `AddrInUse` | REFUSED `AddrInUse` |
| `0.0.0.0` | `127.0.0.1` | **SUCCEEDED** | REFUSED `AddrInUse` |
| `[::]` | `127.0.0.1` | **SUCCEEDED** | REFUSED `AddrInUse` |
| `127.0.0.1` | `0.0.0.0` | **SUCCEEDED** | REFUSED `AddrInUse` |

`std TcpListener::bind -> SO_REUSEADDR=4 SO_REUSEPORT=0` on macOS;
`SO_REUSEADDR=1 SO_REUSEPORT=0` on Linux. Both platforms **set** `SO_REUSEADDR`; they
disagree on what it licenses. BSD honours it for wildcard/specific overlap on a live
listener. Linux scopes it to `TIME_WAIT` and requires `SO_REUSEPORT` — which nothing in
this tree sets — for overlapping live listeners.

## What this means for `singleton.rs`

Row 1 is the only case two `tcr` instances can produce, since tcr hard-codes loopback. So
**two tcr servers can never both be live, on either platform.**

A *wildcard*-bound incumbent is different. On macOS it coexists silently with tcr's
loopback bind: both are live, and which one a client reaches depends on the kernel's
most-specific-match, not on anything we control. That is the two-proxies/token-war
outcome, and on macOS it is reachable. On Linux it is not — the second bind fails.

The remaining premise is about node, not about the kernel: this only matters if the legacy
JS proxy binds a wildcard. `server.listen(port)` with no host binds `::` dual-stack — that
is a claim about node's default and should be stated as one, not folded into the measured
rows above.

Disposition is unchanged either way: neither the old `lsof` scan nor the `listeners` scan
filters on address, so a *working* scan kills a wildcard listener on both platforms. Only
a scan that fails silently lets it survive, and the old code failed silently in exactly the
same way.

## Why the first probe was wrong, which is the more useful finding

`bind-overlap-probe.py` deliberately set no socket options and reported REFUSED for all
four pairings on **both** platforms — a clean, symmetric, entirely wrong table. Rust's std
sets `SO_REUSEADDR` on Unix, so the probe measured a socket tcr never creates.

Its positive control passed. It could not have done otherwise: same-address refuses with or
without `SO_REUSEADDR`, so the control was blind to the single variable under test. A
control that shares the probe's defect certifies nothing — and a symmetric result across
two platforms reads as *more* trustworthy than the truth, which is asymmetric.

The fix was not a better control. It was to stop reconstructing the call and make it.
