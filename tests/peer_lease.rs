//! Phase 4's gates: what a borrowed request may never carry, and the guard a
//! lease can never spend.
//!
//! Every test here RUNS, and the SERVE stream runs on the REAL listener: a
//! real lender and a real borrower in this one binary, on kernel ports, with a
//! fake upstream behind the lender's own proxy. Nothing here reads the
//! operator's config directory, touches the live proxy on `127.0.0.1:3456`,
//! or reaches the network.
//!
//! **This file no longer stands in for a missing line.** It used to say so:
//! `listener::serve_stream` matched `StreamKind::Serve` to a `bail!`, so
//! `mod mesh` did what that arm had to do and the two halves of the feature
//! were tested against each other with nothing joining them. This change landed
//! the arm, and `mesh::spawn_lender` hands a socket to
//! `listener::serve_on_with`: the shipped accept loop. The one harness left
//! (`mesh::spawn_capturing_lender`) exists to see the DECRYPTED request frame,
//! which the real listener deliberately exposes to nobody.
//!
//! # The credential assertions are the point of this file
//!
//! A SERVE is built at the picker's dry-fleet arm (`src/proxy.rs:2311`), and
//! `build_upstream_headers`: the ONLY code in this tree that removes a
//! client's own `authorization` and substitutes a pooled token, is 215 lines
//! downstream of it (definition `:3455`, sole call site `:2526`). So a relay
//! built at that seam forwards the client's credential to another host BY
//! DEFAULT, and nothing in between would stop it. These tests are what stands
//! where the existing code does not reach.

/// What the real API refuses a request without, shared by every fake upstream
/// in this tree. See that module's docs for why a fake upstream is strict.
#[path = "tools/api_contract.rs"]
mod api_contract;

use std::path::{Path, PathBuf};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;
use tcr_peer_wire::{Lease, LeaseRefusal, LeaseRequest, LeaseUnit, PeerId, Window};
use teamclaude_rs::fallback::{self, Ask, FallbackProvider};
use teamclaude_rs::peer::config::{
    Allow, ControlGrants, Endpoint, EndpointSource, LendGrant, PeerFile, PeerRow, PeerStore,
};
use teamclaude_rs::peer::lease::{self, Ledger, PeerLeaseProvider, RelayRefusal};
use teamclaude_rs::peer::serve;

/// **The scrub.** A SERVE frame carries neither `authorization` nor
/// `x-api-key`, because the borrower removes both before the frame is written.
///
/// On the borrower, not the lender: a lender-side refusal happens after the
/// bytes are already in the lender's process, its memory and its request log.
///
/// Watch it fail by deleting the scrub.
#[test]
fn relay_frame_carries_no_client_credential() {
    let mut headers = HeaderMap::new();
    // Obviously fake, and shaped like the real thing so a prefix match cannot
    // pass this test by accident.
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer not-a-real-token"),
    );
    headers.insert("x-api-key", HeaderValue::from_static("not-a-real-key"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));

    let removed = serve::scrub_client_credentials(&mut headers);

    assert_eq!(removed, 2, "both credential headers must be removed");
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("x-api-key").is_none());
    assert!(
        headers.get("content-type").is_some(),
        "the scrub removes credentials, not the request"
    );
}

/// **The refusal.** A request to one of the client-credential paths never opens
/// a relay at all: it falls through to this machine's own local path.
///
/// Six of the seven are prefixes in `CLIENT_CREDENTIAL_PREFIXES`
/// (`src/proxy.rs:820-827`). Falling through is the right answer, not an error:
/// the request is perfectly servable here.
///
/// Watch it fail by emptying the refusal list.
#[test]
fn a_client_credential_path_never_opens_a_relay() {
    for path in [
        "/v1/code",
        "/v1/code/",
        "/api/oauth/files",
        "/api/oauth/file_upload",
        "/api/oauth/organizations",
        "/v1/mcp_servers",
        "/v1/sessions",
    ] {
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} is a client-credential path and must never be relayed"
        );
    }

    assert!(
        serve::serve_is_allowed_for_path("/v1/messages"),
        "an ordinary inference request is exactly what a lease is for"
    );
}

/// **The seventh path**, which is the one a coder who reads only the array will
/// miss.
///
/// `/v1/oauth/token` lives in its own constant (`CLIENT_TOKEN_REFRESH_PATH`,
/// `src/proxy.rs:839`), not in the six-element array, and its own doc records
/// the live defect that omitting it caused: an exact compare let the
/// trailing-slash spelling fall through to the pooled path, putting our Bearer
/// on a client's token exchange. So both spellings are asserted here, and the
/// match is `path_is_under` rather than an equality test.
///
/// It is also the one path a BLIND tunnel carries for free: that is how an
/// offline machine refreshes its own tokens, so "free to carry, forbidden to
/// relay" is one fact about two stream kinds, not a contradiction.
///
/// Watch it fail by swapping `path_is_under` for an exact compare, which is
/// precisely the shape of the recorded defect.
#[test]
fn oauth_token_exchange_never_relays() {
    for spelling in ["/v1/oauth/token", "/v1/oauth/token/"] {
        assert!(
            !serve::serve_is_allowed_for_path(spelling),
            "{spelling} is a client's own credential exchange and must never be relayed"
        );
    }
}

/// **The guard band.** A lease with budget left is still refused when the
/// lender's own window crosses its guard, and the refusal is
/// `owner-guard`: its own case, not a variant of "spent".
///
/// It has to be able to fire alone: the headers this arithmetic reads lag, and
/// upstream answers 200s for accounts it is about to bench, which is why a
/// guard exists rather than the raw threshold.
///
/// Watch it fail at lender utilization just under the guard, which is the
/// control that passes vacuously if the ordering is wrong.
#[test]
fn a_lease_can_never_spend_the_owners_guard() {
    let mut ledger = Ledger::new();
    ledger.record(lease_with(LEASE, 0.500, 0.002));

    // The lender's own window has crossed its guard, so `lendable_fraction`
    // answers 0.0: the figure the default config produces at a utilization of
    // 0.91 against a guard of 0.95 - 0.05.
    ledger.note_owner_headroom(Window::SevenDay, 0.0);
    assert_eq!(
        ledger.may_relay(LEASE, NOW),
        Err(LeaseRefusal::OwnerGuard),
        "0.498 of budget is left and the deadline is ahead, so this refusal fired ALONE"
    );

    // The control that makes the assertion above non-vacuous: just UNDER the
    // guard, 0.89 against 0.90, the same lease proceeds. If the ordering were
    // wrong (owner-guard folded into spent, or checked before expiry), this is
    // the arm that would break.
    ledger.note_owner_headroom(Window::SevenDay, 0.90 - 0.89);
    assert_eq!(
        ledger.may_relay(LEASE, NOW),
        Ok(()),
        "a lender at 0.89 against a 0.90 guard still has headroom to lend"
    );

    // ... and the guard is not the only refusal that can fire, which is what
    // makes the ORDER a contract rather than a coincidence.
    let mut spent = Ledger::new();
    spent.record(lease_with(LEASE, 0.500, 0.500));
    spent.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(spent.may_relay(LEASE, NOW), Err(LeaseRefusal::LeaseSpent));

    let mut expired = Ledger::new();
    expired.record(lease_with(LEASE, 0.500, 0.500));
    expired.note_owner_headroom(Window::SevenDay, 0.0);
    assert_eq!(
        expired.may_relay(LEASE, NOW + 1_000_000),
        Err(LeaseRefusal::LeaseExpired),
        "expired outranks both spent and the owner's guard, which are also true here"
    );
}

/// An unmeasured window lends nothing.
///
/// A ledger that was never told the owner's headroom refuses, rather than
/// treating silence as room. This is the direction where a wrong guess costs the
/// OWNER their quota, so it is the direction that fails closed.
#[test]
fn a_window_with_no_measurement_refuses() {
    let mut ledger = Ledger::new();
    ledger.record(lease_with(LEASE, 0.500, 0.0));
    assert_eq!(
        ledger.may_relay(LEASE, NOW),
        Err(LeaseRefusal::OwnerGuard),
        "no headroom measurement is a refusal, never unlimited room"
    );
}

/// One relayed request, one debit, even when two disjoint paths deliver it.
///
/// The `via` stamp cannot catch a diamond: two paths converging on one
/// terminal, so the request id is what makes the second delivery free.
#[test]
fn one_request_id_debits_exactly_once() {
    let mut ledger = Ledger::new();
    ledger.record(lease_with(LEASE, 0.500, 0.0));

    let first = ledger.debit(LEASE, REQUEST, 0.031);
    let diamond = ledger.debit(LEASE, REQUEST, 0.031);

    assert!(
        (first - 0.031).abs() < f64::EPSILON,
        "the first delivery charges the observed rise, got {first}"
    );
    assert_eq!(
        diamond, 0.0,
        "the second delivery of the SAME request id charges nothing"
    );
    let spent = ledger
        .live(NOW)
        .first()
        .copied()
        .expect("the lease is live at NOW")
        .spent;
    assert!(
        (spent - 0.031).abs() < f64::EPSILON,
        "one request, one debit on the ledger's own total, got {spent}"
    );

    // A DIFFERENT request under the same lease is a second, real debit.
    let second = ledger.debit(LEASE, REQUEST + 1, 0.0);
    assert!(
        (second - lease::MIN_DEBIT).abs() < f64::EPSILON,
        "a zero observed rise still charges MIN_DEBIT, got {second}"
    );
}

/// **The cap is a refusal, not a queue.** The (n+1)th concurrent borrowed
/// request is told no while n are running, and a slot freeing lets the next one
/// in.
///
/// It exists because the debit reads headers this tree documents as lagging: an
/// unbounded pipe can overdraw a small lease inside the lag window, and the cap
/// bounds the overdraft to a known number of requests rather than to however
/// many the borrower can open.
#[test]
fn the_n_plus_first_concurrent_borrowed_request_is_refused() {
    let grantee = PeerId([41_u8; 32]);
    let mut ledger = Ledger::new();
    // `record_scoped` and a FRESH request id per entry, both added together:
    // the review's M1 binds a lease to its grantee, and its H2 refuses a
    // `request_id` this lease has already relayed. The old body recorded the
    // lease with no grantee and entered three times under no id at all, which
    // is exactly the two shapes now refused: so it is rewritten rather than
    // relaxed, and what it measures (the cap, and the cap alone) is unchanged.
    ledger.record_scoped(
        lease_with(LEASE, 0.500, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, NOW), Ok(()));
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 1, NOW),
        Ok(())
    );
    assert_eq!(ledger.inflight(LEASE), 2);

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 2, NOW),
        Err(RelayRefusal::TooManyInflight { max: 2 }),
        "the third request is refused with the cap named, never queued"
    );

    ledger.leave_relay(LEASE);
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 3, NOW),
        Ok(()),
        "a freed slot admits the next borrowed request"
    );

    // The lease's own refusals still come first: a full pipe must not mask an
    // expired lease, or a borrower would retry a lease that is gone.
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 4, NOW + 1_000_000),
        Err(RelayRefusal::Lease(LeaseRefusal::LeaseExpired))
    );
}

/// **A cap refusal forgets nothing.** At the cap, the refusal leaves the
/// counter at the cap and the next request is refused too: the cap keeps
/// binding for as long as the requests it counted are still running.
///
/// Testing found this: `enter_relay` answered a cap refusal
/// with `self.inflight.remove(&lease_id)`, which dropped the count of the
/// `max` requests that were in flight at that instant. The very next
/// `enter_relay` then read zero and admitted, so the cap bound exactly once
/// per lease and a borrower that simply retried on `InFlightFull`: which is
/// the one refusal `LeaseRefusal::InFlightFull` tells it to retry after, got
/// an unbounded pipe. `the_n_plus_first_concurrent_borrowed_request_is_refused`
/// above cannot see it, because it calls `leave_relay` before testing
/// admission again and a missing key makes that call a no-op.
///
/// Watched red: restore the `self.inflight.remove(&lease_id)` line in
/// `Ledger::enter_relay` (`src/peer/lease.rs`) and the counter reads 0 rather
/// than 2 right after the refusal, while the fourth request is admitted.
#[test]
fn a_cap_refusal_leaves_the_in_flight_counter_where_it_found_it() {
    let grantee = PeerId([42_u8; 32]);
    let mut ledger = Ledger::new();
    ledger.record_scoped(
        lease_with(LEASE, 0.500, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, NOW), Ok(()));
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 1, NOW),
        Ok(())
    );
    assert_eq!(ledger.inflight(LEASE), 2, "the cap's worth is in flight");

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 2, NOW),
        Err(RelayRefusal::TooManyInflight { max: 2 })
    );
    assert_eq!(
        ledger.inflight(LEASE),
        2,
        "the two requests still running were not forgotten by the refusal"
    );
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 3, NOW),
        Err(RelayRefusal::TooManyInflight { max: 2 }),
        "so the next one is refused too: the cap binds every time, not once"
    );
    assert_eq!(ledger.inflight(LEASE), 2);

    // And only when a slot is genuinely released does the next one get in,
    // leaving exactly the cap in flight again.
    ledger.leave_relay(LEASE);
    assert_eq!(ledger.inflight(LEASE), 1);
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 4, NOW),
        Ok(())
    );
    assert_eq!(ledger.inflight(LEASE), 2);
}

/// **A lease that may relay nothing parks no counter.** `max_inflight` zero
/// refuses the first request, and the refusal leaves the in-flight map without
/// an entry at all: `Ledger::leave_relay` documents zero and absent as one
/// fact, and an `entry(..).or_insert(0)` on the refusal path broke that for
/// every lease that could never relay.
///
/// Watched red: change the `self.inflight(lease_id)` read in
/// `Ledger::enter_relay` back to `self.inflight.entry(lease_id).or_insert(0)`
/// with the `remove` line deleted, and the map holds a zero for this lease.
#[test]
fn a_zero_in_flight_cap_refuses_and_parks_no_counter() {
    let grantee = PeerId([43_u8; 32]);
    let mut ledger = Ledger::new();
    let mut lease = lease_with(LEASE, 0.500, 0.0);
    lease.max_inflight = 0;
    ledger.record_scoped(lease, grantee, tcr_peer_wire::LendScope::All);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Err(RelayRefusal::TooManyInflight { max: 0 }),
        "a lease that may relay nothing refuses its first request"
    );
    assert_eq!(ledger.inflight(LEASE), 0);
    assert_eq!(
        ledger.inflight_tracked(),
        0,
        "and no entry is parked for it: zero and absent are the same fact"
    );
}

/// **The review's H2, as an arithmetic fact about the ceiling.** One
/// `request_id` replayed is refused, and the lease's budget therefore actually
/// runs out.
///
/// The defect: `debit` was idempotent per `(lease_id, request_id)` and the SERVE
/// was not, so every copy of one request id was served on the lender's account
/// while `Lease::spent` moved once. `may_relay`'s `spent >= budget` could then
/// never become true and the lease was unbounded in the one dimension it exists
/// to bound.
///
/// Watched red: delete the `admit_served` refusal from
/// `Ledger::enter_relay` (`src/peer/lease.rs`) and the second `enter_relay`
/// below answers `Ok(())`.
#[test]
fn one_request_id_is_relayed_once_and_the_ceiling_then_binds() {
    let grantee = PeerId([42_u8; 32]);
    let mut ledger = Ledger::new();
    // A budget of two MIN_DEBITs, so a bounded lease spends out in two
    // requests and an unbounded one never does.
    ledger.record_scoped(
        lease_with(LEASE, lease::MIN_DEBIT * 2.0, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Ok(()),
        "the first copy of a request id is served"
    );
    ledger.leave_relay(LEASE);
    assert!(ledger.debit(LEASE, REQUEST, 0.0) > 0.0, "and it is charged");

    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Err(RelayRefusal::Replayed),
        "the SECOND copy of the same request id is REFUSED, never served again          for free: this is the whole of H2"
    );

    // And the ceiling binds: a fresh id spends the rest of the budget, and the
    // one after it is out of money rather than out of luck.
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 1, NOW),
        Ok(())
    );
    ledger.leave_relay(LEASE);
    ledger.debit(LEASE, REQUEST + 1, 0.0);
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST + 2, NOW),
        Err(RelayRefusal::Lease(LeaseRefusal::LeaseSpent)),
        "two MIN_DEBITs out of a two-MIN_DEBIT lease is spent"
    );
}

/// **The review's M1**: a lease is not a bearer token. Peer B presenting peer
/// A's lease id is refused, and told nothing that distinguishes "not yours"
/// from "no such lease".
///
/// Watched red: delete the `grantees` comparison at the top of
/// `Ledger::enter_relay` and the stranger's relay answers `Ok(())`.
#[test]
fn another_peers_lease_id_is_refused_and_leaks_nothing() {
    let grantee = PeerId([43_u8; 32]);
    let stranger = PeerId([44_u8; 32]);
    let mut ledger = Ledger::new();
    ledger.record_scoped(
        lease_with(LEASE, 0.500, 0.0),
        grantee,
        tcr_peer_wire::LendScope::All,
    );
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    assert_eq!(
        ledger.enter_relay(LEASE, &stranger, REQUEST, NOW),
        Err(RelayRefusal::NotTheGrantee),
        "a pinned peer holding `inspect` may not spend a lease another peer          negotiated"
    );
    assert_eq!(
        RelayRefusal::NotTheGrantee.to_wire(),
        LeaseRefusal::LeaseExpired,
        "and the wire word is the SAME one an unknown lease id gets, so the          refusal is not an oracle for lease ids"
    );
    assert_eq!(
        ledger.enter_relay(LEASE, &grantee, REQUEST, NOW),
        Ok(()),
        "the grantee's own relay is unaffected: the stranger's attempt must not          have burned this request id"
    );
}

/// **The review's M4**: none of the ledger's three peer-driven collections
/// grows without bound.
///
/// The bar is the feature's own, stated twice next door in `listener.rs`: "a map
/// a stranger can grow without bound is a memory bug with a security label".
///
/// Watched red: delete the prune from `Ledger::record` and `leases()` stays at
/// 300; drop the `inflight.remove` at zero in `leave_relay` and the in-flight
/// map keeps a key per lease; raise `SERVED_CAPACITY` past the loop count and
/// the pair cache grows past it.
#[test]
fn the_ledger_does_not_grow_without_bound_on_peer_driven_input() {
    let grantee = PeerId([45_u8; 32]);
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    // 300 leases asked for and expired, one after another: which is what a
    // peer that asks once a second for five minutes leaves behind.
    for nth in 0..300_u128 {
        let mut expired = lease_with(LEASE + 1 + nth, 0.500, 0.0);
        expired.expires_at_ms = teamclaude_rs::now_ms() - 1;
        ledger.record_scoped(expired, grantee, tcr_peer_wire::LendScope::All);
    }
    // One live lease last, so the vector is not empty for a reason unrelated to
    // the prune.
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.expires_at_ms = teamclaude_rs::now_ms() + 300_000;
    ledger.record_scoped(live, grantee, tcr_peer_wire::LendScope::All);
    assert!(
        ledger.live(teamclaude_rs::now_ms()).len() == 1,
        "one live lease"
    );

    // In flight, then out: the key goes with it.
    let now = teamclaude_rs::now_ms();
    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
    assert_eq!(ledger.inflight(LEASE), 1);
    ledger.leave_relay(LEASE);
    assert_eq!(
        ledger.inflight(LEASE),
        0,
        "a released slot reads zero, whether the key is there or not"
    );

    // The pair cache is bounded by its own constant, not by how many requests
    // a borrower can send.
    for nth in 0..(lease::SERVED_CAPACITY as u128 + 64) {
        ledger.leave_relay(LEASE);
        let _ = ledger.enter_relay(LEASE, &grantee, REQUEST + 1 + nth, now);
    }
    assert!(
        ledger.served_len() <= lease::SERVED_CAPACITY,
        "the served-pair cache is bounded by SERVED_CAPACITY ({}), got {}",
        lease::SERVED_CAPACITY,
        ledger.served_len()
    );
}

/// **The review's H1**: a borrower-chosen path that the upstream URL parser
/// would read differently than these prefix compares do is refused outright.
///
/// Each shape below was measured to normalize somewhere else: the dot segments
/// onto the lender's own privileged `/_tcr/` routes, the unrooted one onto a
/// host the borrower named (an SSRF with the lender's network position), the
/// backslash onto a `CLIENT_CREDENTIAL_PREFIXES` path.
///
/// Watched red: delete the `relay_path_is_routable` call from
/// `serve::serve_is_allowed_for_path` and every shape below is allowed.
#[test]
fn a_path_the_url_parser_would_reroute_never_opens_a_relay() {
    for path in [
        // Dot segments, which the parser collapses away.
        "/x/../_tcr/accounts/control",
        "/x/../_tcr/accounts",
        "/v1/messages/../_tcr/status",
        "/v1/messages/%2e%2e/_tcr/status",
        // Not rooted: the authority becomes userinfo and the request leaves
        // the machine.
        "@169.254.169.254/latest/meta-data",
        "v1/messages",
        // Protocol-relative: rooted, and still names a host.
        "//169.254.169.254/latest/meta-data",
        // A backslash, which WHATWG treats as a path separator.
        "/v1/code\\foo",
        "/v1\\messages",
    ] {
        assert!(
            !serve::relay_path_is_routable(path),
            "{path} must not be routable: the parser reads it as something else"
        );
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} must never open a relay"
        );
        let ask = ask_for(path);
        assert!(
            serve::serve_request_from(&ask, &HeaderMap::new(), LEASE, REQUEST).is_err(),
            "{path} must not even build a frame"
        );
    }

    // The positive control, without which every assertion above could be
    // passing because the function refuses everything.
    for ordinary in ["/v1/messages", "/v1/messages/count_tokens", "/"] {
        assert!(
            serve::relay_path_is_routable(ordinary),
            "{ordinary} is an ordinary relayable path and must stay routable"
        );
    }
    assert!(
        serve::serve_is_allowed_for_path("/v1/messages"),
        "and the ordinary path still opens a relay"
    );
}

/// **An escaped separator is refused too.** `/v1/messages/..%2F_tcr` is not a
/// dot segment to `Url::parse` (it does not decode `%2F`), so it reaches the
/// lender intact, and a hop that DOES decode before routing reads it as
/// `/v1/messages/../_tcr`: the same escape `relay_path_is_routable` exists to
/// refuse, arriving one decode later. Both cases of both spellings.
///
/// Watched red: delete the `%2f`/`%5c` refusal from
/// `serve::relay_path_is_routable` and every shape below is routable.
#[test]
fn an_escaped_path_separator_never_opens_a_relay() {
    for path in [
        "/v1/messages/..%2F_tcr",
        "/v1/messages/..%2f_tcr",
        "/v1/messages/..%5C_tcr",
        "/v1/messages/..%5c_tcr",
        "/v1%2f..%2f_tcr/accounts/control",
        "/v1/code%5Cfoo",
    ] {
        assert!(
            !serve::relay_path_is_routable(path),
            "{path} carries an escaped separator and must not be routable: a hop that decodes \
             it before routing reads a dot segment"
        );
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} must never open a relay"
        );
        let ask = ask_for(path);
        assert!(
            serve::serve_request_from(&ask, &HeaderMap::new(), LEASE, REQUEST).is_err(),
            "{path} must not even build a frame"
        );
    }

    // The positive control: an ordinary escape that is NOT a separator stays
    // routable, so this refusal is about `/` and `\` and not about `%`.
    assert!(
        serve::relay_path_is_routable("/v1/messages%20count"),
        "an escaped space is not a path separator and must stay routable"
    );
}

/// **What `Url::set_path` plus the under-the-base check actually refuse: none
/// of the H1 shapes.**
///
/// `serve_on_own_account`'s comment used to say this pair was what stopped
/// `//169.254.169.254/latest/meta-data` if `relay_path_is_routable` were
/// deleted. It is not. `set_path` cannot move the authority: which is what
/// makes the borrower's host harmless, and every shape below therefore lands
/// UNDER this Mac's own base, including the dot-segment path that collapses
/// straight onto the privileged local route. The refusal is
/// `relay_path_is_routable`'s alone, and this test is what keeps the comment
/// honest about that.
///
/// The one thing the check does catch is its last case: an `upstream` carrying
/// a path, whose path `set_path` replaces.
#[test]
fn set_path_keeps_the_authority_and_the_under_base_check_is_not_what_refuses() {
    let base = "http://127.0.0.1:3456";
    for path in [
        "//169.254.169.254/latest/meta-data",
        "/x/../_tcr/accounts/control",
        "@169.254.169.254/latest/meta-data",
        "/v1/code\\foo",
        "/v1/messages/..%2F_tcr",
    ] {
        let mut url = reqwest::Url::parse(base).expect("the base parses");
        url.set_path(path);
        assert_eq!(
            url.host_str(),
            Some("127.0.0.1"),
            "set_path must never move the authority, which is the real defence: {path} -> {url}"
        );
        assert!(
            url.as_str().starts_with(base),
            "and every one of these therefore passes the under-the-base check, so that check \
             refuses none of them: {path} -> {url}"
        );
    }

    // The dot segment collapses onto this Mac's own privileged route and is
    // still under the base: the case the old comment claimed this line caught.
    let mut collapsed = reqwest::Url::parse(base).expect("the base parses");
    collapsed.set_path("/x/../_tcr/accounts/control");
    assert_eq!(collapsed.path(), "/_tcr/accounts/control");
    assert!(
        !serve::relay_path_is_routable("/x/../_tcr/accounts/control"),
        "which is why the refusal has to happen before the URL is built"
    );

    // And the case the check is genuinely for: a base with a path of its own.
    let mut moved = reqwest::Url::parse("http://127.0.0.1:3456/api").expect("the base parses");
    moved.set_path("/v1/messages");
    assert!(
        !moved
            .as_str()
            .starts_with("http://127.0.0.1:3456/api".trim_end_matches('/')),
        "a base carrying a path has it replaced, and the result is outside that base: {moved}"
    );
}

/// **The three clamps, narrowest wins**, and which refusal each miss produces.
///
/// The measured term (`lendable`) is last, so an operator who granted a generous
/// fraction still cannot lend quota the owner's guard band does not leave.
#[test]
fn a_grant_is_clamped_by_the_narrowest_of_the_three() {
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 600,
        max_inflight: 8,
    };
    let granted = LendGrant::new(Window::SevenDay, 0.20, 300, 2);

    // The operator's grant is the narrowest of the three.
    let grant = lease::clamp_to_grant(&ask, Some(granted.clone()), true, 0.40, NOW, LEASE, None);
    let minted = grant.lease.expect("0.20 is lendable, so a lease is minted");
    assert_eq!(minted.unit, LeaseUnit::Fraction(0.20));
    assert_eq!(minted.max_inflight, 2, "the grant's cap, not the ask's");
    assert_eq!(
        minted.expires_at_ms,
        NOW + 300_000,
        "the grant's ttl, not the ask's"
    );

    // The MEASURED headroom is the narrowest of the three.
    let tight = lease::clamp_to_grant(&ask, Some(granted.clone()), true, 0.03, NOW, LEASE, None);
    assert_eq!(
        tight.lease.map(|lease| lease.unit),
        Some(LeaseUnit::Fraction(0.03)),
        "the owner's own headroom clamps a grant it cannot back"
    );

    // Below MIN_DEBIT there is no lease to mint: the first request would
    // overdraw it. Nothing is spent, so the refusal names the owner's guard.
    let none_left =
        lease::clamp_to_grant(&ask, Some(granted.clone()), true, 0.0005, NOW, LEASE, None);
    assert_eq!(none_left.lease, None);
    assert_eq!(none_left.refusal, Some(LeaseRefusal::OwnerGuard));

    // The lender's own opt-in is off: SERVE is not on offer at all, whatever the
    // arithmetic would have said.
    let blind = lease::clamp_to_grant(&ask, Some(granted.clone()), false, 0.40, NOW, LEASE, None);
    assert_eq!(blind.lease, None);
    assert_eq!(blind.refusal, Some(LeaseRefusal::InspectNotGranted));

    // A grant for a DIFFERENT window is not a grant for this one.
    let other_window = LendGrant {
        window: Window::FiveHour,
        ..granted.clone()
    };
    let mismatch = lease::clamp_to_grant(&ask, Some(other_window), true, 0.40, NOW, LEASE, None);
    assert_eq!(mismatch.lease, None);
    assert_eq!(mismatch.refusal, Some(LeaseRefusal::InspectNotGranted));

    // A window, or a unit, this build does not know is refused rather than
    // guessed at.
    let unknown_window = LeaseRequest {
        window: Window::Unknown,
        ..ask
    };
    assert_eq!(
        lease::clamp_to_grant(
            &unknown_window,
            Some(granted.clone()),
            true,
            0.40,
            NOW,
            LEASE,
            None
        )
        .refusal,
        Some(LeaseRefusal::Unsupported)
    );
    let token_unit = LeaseRequest {
        unit: LeaseUnit::Tokens(1_000),
        ..ask
    };
    assert_eq!(
        lease::clamp_to_grant(
            &token_unit,
            Some(granted.clone()),
            true,
            0.40,
            NOW,
            LEASE,
            None
        )
        .refusal,
        Some(LeaseRefusal::Unsupported)
    );
}

/// The fallback provider refuses a client-credential path BEFORE it looks at a
/// lease, so the refusal cannot be reached around by configuring one.
///
/// It answers `None` ("not me"), which costs the caller only the next rung of
/// the ladder, and the last rung is the honest 429 the proxy already had.
///
/// The provider is pointed at a peers file that does not exist, which is the
/// control that makes this non-vacuous in the other direction: it proves the
/// path refusal runs BEFORE the peers file is read, because a provider that
/// read the file first would have logged a read failure and answered `None` for
/// the wrong reason.
#[test]
fn the_provider_declines_a_credential_path_without_consulting_a_lease() {
    let provider = PeerLeaseProvider::new(PathBuf::from("/nonexistent/tcr-peers.json"));
    assert_eq!(provider.name(), "peer-lease");

    for path in [
        "/v1/oauth/token/",
        "/api/oauth/organizations",
        "/v1/code",
        "/_tcr/accounts",
        "/_tcr/status",
    ] {
        let ask = ask_for(path);
        let answered = futures::executor::block_on(provider.try_serve(&ask));
        assert!(
            answered.is_none(),
            "{path} must never be served through a peer's account"
        );
    }
}

/// **The local control surface is never relayed** (hole C).
///
/// `/_tcr/…` is not an upstream path at all: behind it sits this proxy's own
/// control route, whose entire authorization is that the caller reached
/// loopback (`local_endpoint_gate`, `src/proxy.rs:1137`): and one of its
/// endpoints ADDS A LIVE CREDENTIAL. The lender serves a relayed request
/// through its OWN proxy on loopback, so a borrower that put `/_tcr/accounts`
/// in a frame would be handed that gate, having satisfied it by construction.
///
/// This is the one refusal in the list whose absence is REACHABLE end to end,
/// which is why `a_lender_refuses_a_local_control_path_in_a_frame` below drives
/// it over a real stream as well as asserting the predicate here.
///
/// Watch it fail by deleting the `LOCAL_PREFIX` arm of
/// `serve_is_allowed_for_path`.
#[test]
fn a_local_control_path_is_never_relayed() {
    for path in [
        "/_tcr",
        "/_tcr/status",
        "/_tcr/accounts",
        "/_tcr/accounts/add",
    ] {
        assert!(
            !serve::serve_is_allowed_for_path(path),
            "{path} is this proxy's own control surface and must never be relayed"
        );
    }
    // The control, and it is the same matcher the proxy's own guard uses on
    // this prefix: `/_tcrother` is NOT under `/_tcr`, and the proxy forwards it
    // upstream rather than answering it locally (`src/proxy.rs:2001`,
    // `path_is_under`). Refusing it here would make the two lists disagree
    // about what "local" means, which is the drift this file exists to avoid.
    assert!(serve::serve_is_allowed_for_path("/v1/messages"));
    assert!(serve::serve_is_allowed_for_path("/_tcrother"));
}

/// **A built SERVE frame carries no credential of any shape** (hole A).
///
/// The four credential names and every hop-by-hop request header are gone from
/// the frame, and the hop-by-hop list is `src/proxy.rs`'s own
/// (`is_request_hop_by_hop`) rather than a second copy here.
///
/// Asserted on the SERIALIZED frame as well as on the header list: a name that
/// survived under a different case, or a value that came along inside another
/// field, is invisible to a per-name check and visible in the bytes.
///
/// Watch it fail by deleting the `scrub_client_credentials` call in
/// `serve_request_from`.
#[test]
fn a_built_serve_frame_carries_no_client_credential() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer not-a-real-token"),
        ("x-api-key", "not-a-real-key"),
        ("proxy-authorization", "Bearer not-a-real-proxy-key"),
        ("cookie", "sessionKey=not-a-real-cookie"),
        ("connection", "keep-alive"),
        ("content-type", "application/json"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("a header value"),
        );
    }

    let ask = ask_for("/v1/messages");
    let frame = serve::serve_request_from(&ask, &headers, LEASE, REQUEST)
        .expect("an ordinary inference path builds a frame");

    let names: Vec<&str> = frame
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["content-type"],
        "only the request survives the scrub, not one credential and not one \
         hop-by-hop header"
    );

    let bytes = serde_json::to_vec(&frame).expect("the frame serializes");
    let wire = String::from_utf8(bytes).expect("the frame is utf-8");
    for secret in [
        "not-a-real-token",
        "not-a-real-key",
        "not-a-real-proxy-key",
        "not-a-real-cookie",
    ] {
        assert!(
            !wire.contains(secret),
            "{secret} reached the wire inside a SERVE frame"
        );
    }
    // The positive control: the frame is not empty, so the absences above are
    // about the scrub and not about a frame that never got built.
    assert!(wire.contains("/v1/messages"));
}

/// A frame is never built for a path a relay must not carry, and never for a
/// body one frame cannot hold.
///
/// Both are refusals rather than a truncation or a silent split: chunking a
/// body needs a sequence number, an end marker and a debit that survives a
/// half-delivered body, and inventing half of that is worse than saying no.
#[test]
fn a_frame_is_refused_for_a_forbidden_path_and_an_oversized_body() {
    let credential_path = Ask {
        path: "/v1/code",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: Bytes::from_static(b"{}"),
        method: "POST",
        headers: HeaderMap::new(),
    };
    assert!(
        serve::serve_request_from(&credential_path, &HeaderMap::new(), LEASE, REQUEST).is_err()
    );

    // A body over ONE FRAME is carried now, in as many frames as it takes: a
    // request that size is the routine shape of a long conversation, and
    // refusing it declined exactly the traffic this mesh exists for.
    let over_one_frame = Ask {
        path: "/v1/messages",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: Bytes::from(vec![b'x'; serve::MAX_BODY_BYTES + 1]),
        method: "POST",
        headers: HeaderMap::new(),
    };
    let built = serve::serve_request_from(&over_one_frame, &HeaderMap::new(), LEASE, REQUEST)
        .expect("a body over one frame is chunked, not refused");
    assert_eq!(built.body_bytes, serve::MAX_BODY_BYTES + 1);
    assert_eq!(
        built.flow,
        serve::SERVE_FLOW,
        "a frame this build writes says which flow it is going to use"
    );

    // What is still refused is a body over what the whole stream carries.
    let huge = Ask {
        path: "/v1/messages",
        model: None,
        group: None,
        affinity: None,
        tried_local: 1,
        body: Bytes::from(vec![b'x'; serve::MAX_RELAYED_BODY_BYTES + 1]),
        method: "POST",
        headers: HeaderMap::new(),
    };
    let refused = serve::serve_request_from(&huge, &HeaderMap::new(), LEASE, REQUEST)
        .expect_err("a body over the whole relay's ceiling is refused");
    assert!(
        refused
            .to_string()
            .contains(&serve::MAX_RELAYED_BODY_BYTES.to_string()),
        "the refusal names the ceiling it hit: {refused}"
    );
}

/// **A full pipe has its own wire word**: `in-flight-full`, not `lease-spent`.
///
/// The two tell a borrower opposite things: spent means stop asking, a full
/// pipe means retry in a moment. The variant was added to
/// `crates/tcr-peer-wire` under the decision granting it.
#[test]
fn a_full_pipe_refuses_with_its_own_wire_word() {
    assert_eq!(
        RelayRefusal::TooManyInflight { max: 2 }.to_wire(),
        LeaseRefusal::InFlightFull
    );
    // Every lease refusal passes through unchanged: the lender does not
    // re-interpret its own ledger's answer on the way to the wire.
    for refusal in [
        LeaseRefusal::LeaseExpired,
        LeaseRefusal::LeaseSpent,
        LeaseRefusal::OwnerGuard,
        LeaseRefusal::InspectNotGranted,
        LeaseRefusal::Unsupported,
    ] {
        assert_eq!(RelayRefusal::Lease(refusal).to_wire(), refusal);
    }
}

/// **`Ledger::grant` reads the row, mints an unguessable id, and records what it
/// minted.**
///
/// Against a real peers file, because the row lookup and its hot reload are half
/// of what this function does. The refusal arms are `clamp_to_grant`'s and are
/// tested above without a file.
#[test]
fn a_grant_reads_the_peers_file_and_records_what_it_minted() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let borrower = PeerId([7_u8; 32]);
    write_peers(
        &peers_path,
        vec![lender_row_for(
            borrower,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );
    let store = PeerStore::open(&peers_path).expect("the peers file opens");

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 600,
        max_inflight: 8,
    };

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let first = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    let lease = first.lease.expect("0.20 is granted and 0.40 is lendable");
    assert_eq!(lease.unit, LeaseUnit::Fraction(0.20));
    assert_eq!(lease.max_inflight, 2);
    assert_eq!(
        ledger.live(teamclaude_rs::now_ms()).len(),
        1,
        "a minted lease is in the ledger, or the lender cannot honour it"
    );

    // A second grant is a DIFFERENT lease id. 128 bits from the platform
    // CSPRNG, so a collision here is not a flake: it is the random source
    // having stopped being random, which is the failure that matters.
    let second = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("a second lease");
    assert_ne!(lease.lease_id, second.lease_id);

    // An unpinned peer gets no lease at all, whatever it asks for.
    let stranger = PeerId([9_u8; 32]);
    assert_eq!(
        ledger
            .grant(&stranger, &ask, &store, &serve::NoFleetUtilization)
            .answer
            .refusal,
        Some(LeaseRefusal::InspectNotGranted)
    );
}

/// **The provider is configured from the peers file, on the BORROWING grant.**
///
/// A lease needs an explicit act on both machines and the act on this one is
/// `disclose`. A row with `inspect` says the opposite thing: that this Mac
/// LENDS, and must not turn the borrowing seam on, which is the confusion this
/// test exists to pin.
///
/// Reads through a `PeerStore` since the filter became `serve::has_a_way_back`:
/// answering "is there a Mac that could carry to this lender" needs the other
/// rows, not just this one.
#[test]
fn the_provider_is_configured_by_a_disclose_grant_and_an_address() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let lender = PeerId([3_u8; 32]);

    let mut file = PeerFile {
        peers: vec![borrower_row_for(lender, false, Vec::new())],
        ..PeerFile::default()
    };
    let store_of = |file: &PeerFile| {
        teamclaude_rs::peer::config::save(&peers_path, file).expect("write the peers file");
        PeerStore::open(&peers_path).expect("open the peers file")
    };
    assert!(
        fallback::peer_lease_provider(&store_of(&file)).is_none(),
        "a bare pin borrows nothing"
    );

    // `inspect` is the LENDING direction and does not configure this seam.
    file.peers[0].allow.inspect = true;
    file.peers[0].observe_endpoint(Endpoint::direct(
        "127.0.0.1:1"
            .parse()
            .expect("a fixture address is a socket address"),
        0,
        EndpointSource::Paired,
    ));
    assert!(
        fallback::peer_lease_provider(&store_of(&file)).is_none(),
        "lending to a peer is not permission to disclose to it"
    );

    // With `disclose` and an address there is something to borrow from.
    file.peers[0].allow.allow_disclose = true;
    assert!(fallback::peer_lease_provider(&store_of(&file)).is_some());

    // A WAY BACK is part of it: a row with nothing to dial and no Mac that
    // could carry to it is a row that answers nothing, and the provider would
    // walk it on every dry-fleet request forever. An address is one way back;
    // `a_lender_with_no_address_still_installs_the_provider_through_a_carrier`
    // (`tests/peer_forward.rs`) holds the other.
    file.peers[0].endpoints.clear();
    assert!(fallback::peer_lease_provider(&store_of(&file)).is_none());
}

/// **A `Control::LeaseGrant` round-trips.** Measured, because it did not: the
/// whole lease arm was unreachable for a reason neither half of it could see.
///
/// [`tcr_peer_wire::Control`] is internally tagged, so serde deserializes it by
/// buffering the frame into its private `Content` tree first: and that tree has
/// no 128-bit arm, so a `u128` field inside any variant answers
/// `Err("u128 is not supported")` on the way back IN while serializing
/// perfectly on the way out. A lender that granted a lease and a borrower that
/// reported "a frame did not parse" were both telling the truth.
///
/// Watched red: revert `#[serde(with = "id_hex")]` on
/// `tcr_peer_wire::Lease::lease_id` and this test fails with that exact
/// message, while the assertion below it (the value survives) still passes :
/// which is the shape of the defect: the send side never noticed.
#[test]
fn a_lease_grant_survives_the_control_envelope() {
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: NOW,
        expires_at_ms: NOW + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };
    let message = tcr_peer_wire::Control::LeaseGrant(tcr_peer_wire::LeaseGrant {
        lease: Some(lease),
        refusal: None,
    });

    let bytes = serde_json::to_vec(&message).expect("a grant serializes");
    let back: tcr_peer_wire::Control =
        serde_json::from_slice(&bytes).expect("and a grant must PARSE BACK");
    assert_eq!(
        back, message,
        "the lease id must survive the envelope exactly, not approximately"
    );

    // The id is a STRING on the wire, which is the second reason for the shape:
    // a `u128` as a JSON number is a number no double can hold, so any reader
    // that parses JSON into doubles rounds it and then revokes a lease nobody
    // minted.
    let json = String::from_utf8(bytes).expect("the frame is utf-8");
    assert!(
        json.contains(&format!("\"leaseId\":\"{LEASE:032x}\"")),
        "the lease id goes on the wire as 32 hex characters: {json}"
    );

    // And a receipt, which has the identical shape and had the identical
    // defect.
    let receipt = tcr_peer_wire::Control::LeaseReceipt(tcr_peer_wire::LeaseReceipt {
        lease_id: LEASE,
        request_id: REQUEST,
        spent: 0.031,
    });
    let bytes = serde_json::to_vec(&receipt).expect("a receipt serializes");
    let back: tcr_peer_wire::Control =
        serde_json::from_slice(&bytes).expect("and a receipt must parse back");
    assert_eq!(back, receipt);
}

/// **The lender's ledger survives a restart**: the review's M2, which promised
/// exactly this in two doc-comments while nothing serialized anything.
///
/// The grantee and the scope are restored with the lease, because a lease
/// restored without them is a bearer token drawing on the whole fleet: the two
/// defects fixed together (M1 and the lease-scope rule) would both come back at the
/// first reboot.
///
/// Watched red: return early from `Ledger::persist` and `restored` is 0.
#[test]
fn the_ledger_is_persisted_and_restored_with_its_grantee_and_scope() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let grantee = PeerId([46_u8; 32]);
    let scope = tcr_peer_wire::LendScope::Group("work".to_string());

    let now = teamclaude_rs::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;
    // And one already dead, so the `expired=M` half of the log line has
    // something to count and the restore has something to drop.
    let mut dead = lease_with(LEASE + 1, 0.500, 0.0);
    dead.expires_at_ms = now - 1;

    {
        let mut ledger = Ledger::restored_from(&state);
        ledger.record_scoped(live, grantee, scope.clone());
        ledger.record_scoped(dead, grantee, tcr_peer_wire::LendScope::All);
        ledger.note_owner_headroom(Window::SevenDay, 0.30);
        // The charge has to survive too: `spent` is the ceiling H2 exists to
        // keep binding, and a restart that forgot it would reopen that hole one
        // reboot wide.
        assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
        ledger.leave_relay(LEASE);
        assert!(ledger.debit(LEASE, REQUEST, 0.031) > 0.0);
        // The flush is what writes a CHARGE now: `debit` used to persist on
        // the relay path, holding the ledger mutex through a locked file round
        // trip per request. `listener::flush_ledger_periodically` is this call
        // in production, and `a_charge_is_written_by_the_flush_and_not_by_the_relay`
        // is the gate on the split. A grant is still durable without it, which
        // is why the two rows above need no flush.
        ledger.flush();
    }

    let restored = Ledger::restored_from(&state);
    let rows = restored.live(now);
    assert_eq!(
        rows.len(),
        1,
        "the live lease is restored and the expired one is dropped: {rows:?}"
    );
    assert_eq!(rows[0].lease_id, LEASE);
    assert!(
        (rows[0].spent - 0.031).abs() < 1e-9,
        "the spend survives, so the ceiling still binds: {}",
        rows[0].spent
    );
    assert_eq!(
        restored.grantee_of(LEASE),
        Some(grantee),
        "the grantee survives, so the lease is not restored as a bearer token"
    );
    assert_eq!(
        restored.scope_of(LEASE),
        scope,
        "and the scope survives, so it is not restored drawing on the whole fleet"
    );

    // A restored lease is spendable by its grantee and by nobody else, which is
    // the property the restore exists to keep rather than merely the row.
    let mut restored = restored;
    restored.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(
        restored.enter_relay(LEASE, &PeerId([47_u8; 32]), REQUEST + 9, now),
        Err(RelayRefusal::NotTheGrantee)
    );
    assert_eq!(
        restored.enter_relay(LEASE, &grantee, REQUEST + 9, now),
        Ok(())
    );
}

/// **A lease with no grantee is never persisted and never restored.**
///
/// `Ledger::persist` used to write the all-zero peer id for a lease it held no
/// grantee for, and `state::LeaseRow` called that "the honest round-trip of a
/// ledger row that had none". It is not honest: `enter_relay` answers
/// `NotTheGrantee` to every peer asking against such a lease, so nobody can
/// spend it, while `live` and `committed_fraction` still count it: a restart would
/// bring back a row that holds this lender's headroom for its whole TTL on
/// nobody's behalf.
///
/// Both ends are measured, because the file is hand-editable JSON and an older
/// build really did write the zero id: the writer drops the lease, and the
/// reader drops a row that carries one.
///
/// Watched red: restore the `.unwrap_or(PeerId([0_u8; 32]))` fallback in
/// `Ledger::persist` and the first half fails with one row on disk; drop the
/// all-zero check from `Ledger::restored_from` and the second half restores the
/// unspendable lease.
#[test]
fn a_lease_with_no_grantee_is_neither_persisted_nor_restored() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");

    let now = teamclaude_rs::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;

    // The writer. `record` is the entry point that takes no grantee, which is
    // exactly the row this rule is about.
    {
        let mut ledger = Ledger::restored_from(&state);
        ledger.record(live);
        ledger.note_owner_headroom(Window::SevenDay, 0.30);
        assert_eq!(
            ledger.live(now).len(),
            1,
            "in memory it is a lease; the question is what a restart is told"
        );
        // `record` does not persist on its own: `record_scoped` and `debit`
        // do: so the write is forced through the same path a granted lease
        // takes, which is `record_scoped` for a DIFFERENT, granted lease.
        let mut second = lease_with(LEASE + 1, 0.500, 0.0);
        second.granted_at_ms = now;
        second.expires_at_ms = now + 300_000;
        ledger.record_scoped(second, PeerId([48_u8; 32]), tcr_peer_wire::LendScope::All);
    }

    let on_disk = teamclaude_rs::peer::state::load(&state, now).expect("the state file reads");
    assert_eq!(
        on_disk.leases.len(),
        1,
        "only the granted lease is written: {:?}",
        on_disk.leases
    );
    assert_eq!(on_disk.leases[0].lease.lease_id, LEASE + 1);
    assert_eq!(on_disk.leases[0].peer, PeerId([48_u8; 32]));

    // The reader, against a file an older build could have left behind.
    let mut planted = on_disk.clone();
    planted.leases.push(teamclaude_rs::peer::state::LeaseRow {
        lease: live,
        peer: PeerId([0_u8; 32]),
        scope: tcr_peer_wire::LendScope::All,
    });
    teamclaude_rs::peer::state::save(&state, &planted).expect("plant the zero-grantee row");

    let restored = Ledger::restored_from(&state);
    let ids: Vec<u128> = restored
        .live(now)
        .iter()
        .map(|lease| lease.lease_id)
        .collect();
    assert_eq!(
        ids,
        vec![LEASE + 1],
        "a row with no grantee is dropped at restore, not brought back to hold headroom"
    );
    assert_eq!(
        restored.grantee_of(LEASE),
        None,
        "and nothing about it is remembered"
    );
}

/// **A relay charges without writing a file; the flush writes it.**
///
/// `Ledger::debit` called `persist`, which takes `config::FileLock`, loads the
/// whole state file and saves it: and `serve::handle_serve_on` calls `debit`
/// with the ledger's own mutex held, once per relayed request. So the relay
/// path paid a locked file round trip per request while holding the lock every
/// other relay, `tcr peer ls` and the panel row needs.
///
/// A grant still writes immediately: it is rare, nothing is in flight against
/// it, and it is the row a restart cannot reconstruct. A charge marks the
/// ledger dirty and the lender's debounced flusher
/// (`listener::flush_ledger_periodically`) writes it.
///
/// Watched red: put `self.persist();` back in `Ledger::debit` in place of
/// `self.dirty = true;`, the on-disk `spent` reads 0.031 before any flush and
/// `is_dirty` is false, so both halves of this fail.
#[test]
fn a_charge_is_written_by_the_flush_and_not_by_the_relay() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let grantee = PeerId([49_u8; 32]);

    let now = teamclaude_rs::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;

    let mut ledger = Ledger::restored_from(&state);
    ledger.record_scoped(live, grantee, tcr_peer_wire::LendScope::All);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);
    assert!(
        !ledger.is_dirty(),
        "the grant itself wrote, so nothing is pending"
    );
    let granted = teamclaude_rs::peer::state::load(&state, now).expect("the state file reads");
    assert_eq!(
        granted.leases.len(),
        1,
        "a grant is durable the instant it is made: {:?}",
        granted.leases
    );

    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
    ledger.leave_relay(LEASE);
    assert!(ledger.debit(LEASE, REQUEST, 0.031) > 0.0);

    let after_charge = teamclaude_rs::peer::state::load(&state, now).expect("the state reads");
    assert!(
        (after_charge.leases[0].lease.spent - 0.0).abs() < 1e-9,
        "the charge must not have gone to disk on the relay path: {}",
        after_charge.leases[0].lease.spent
    );
    assert!(ledger.is_dirty(), "it is pending instead");

    assert!(ledger.flush(), "the flush has something to write");
    let after_flush = teamclaude_rs::peer::state::load(&state, now).expect("the state reads");
    assert!(
        (after_flush.leases[0].lease.spent - 0.031).abs() < 1e-9,
        "and the flush writes it, so a restart still sees the ceiling: {}",
        after_flush.leases[0].lease.spent
    );
    assert!(
        !ledger.flush(),
        "a second flush with nothing pending takes no lock and writes nothing"
    );
}

/// **A relay returns while the state file's lock is held by somebody else.**
///
/// This is the cost the last test's defect actually had. `config::FileLock`
/// waits out `LOCK_STALE_MS` before it breaks a lockfile whose holder may
/// still be alive, so with `persist` on the relay path an operator running
/// `tcr peer accept` (one locked read-modify-write of the same file), stalled
/// every relayed request on this Mac for seconds, each one holding the ledger
/// mutex while it waited.
///
/// Watched red: put `self.persist();` back in `Ledger::debit` and the charge
/// takes 2 s (`LOCK_STALE_MS`, the point at which the held lock is broken),
/// well past the bound below.
#[test]
fn a_charge_returns_while_the_state_file_lock_is_held() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = dir.path().join("peer-state.json");
    let grantee = PeerId([50_u8; 32]);

    let now = teamclaude_rs::now_ms();
    let mut live = lease_with(LEASE, 0.500, 0.0);
    live.granted_at_ms = now;
    live.expires_at_ms = now + 300_000;

    let mut ledger = Ledger::restored_from(&state);
    ledger.record_scoped(live, grantee, tcr_peer_wire::LendScope::All);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(ledger.enter_relay(LEASE, &grantee, REQUEST, now), Ok(()));
    ledger.leave_relay(LEASE);

    // Somebody else's read-modify-write of the same file, mid-flight.
    let held =
        teamclaude_rs::peer::config::FileLock::acquire(&state).expect("take the file lock first");

    let started = std::time::Instant::now();
    let charge = ledger.debit(LEASE, REQUEST, 0.031);
    let elapsed = started.elapsed();
    drop(held);

    assert!(charge > 0.0, "the charge is still made");
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "a relay must not wait on another process's lock: the charge took {elapsed:?}, and \
         the lock it used to queue behind is only broken after {}ms",
        teamclaude_rs::peer::config::LOCK_STALE_MS
    );
    assert!(
        ledger.is_dirty(),
        "the charge is pending for the debounced flusher, which is the one thing that waits"
    );
}

/// **A write of the lease rows over a QUARANTINED state file is refused.**
///
/// `state::save_leases` is a locked read-modify-write, and its read is
/// `state::load`: which answers an untrusted file with an empty state, because
/// for a READER this file is a cache and losing it costs a cold start. For this
/// writer that was data loss dressed as a healthy file: the lease rows were
/// written on top of an empty knock queue, no mutes, no bans and no accepted
/// pairing windows, which this file's own docs call "the whole of the
/// authorization for a first pairing".
///
/// So it refuses, naming the file and where the evidence went, and the next
/// boot starts cold on its own terms.
///
/// Watched red: change `save_leases` back to `let mut state = load(path,
/// crate::now_ms())?;` with no origin check, it returns `Ok(())`, a fresh
/// `peer-state.json` appears carrying the lease row and an empty `banned`, and
/// the operator's block list is gone.
#[test]
fn writing_lease_rows_over_a_quarantined_state_file_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("peer-state.json");

    // An operator's own decisions, written by the one writer that creates this
    // file at 0600.
    let mut value = teamclaude_rs::peer::state::PeerState::default();
    value.banned.push(teamclaude_rs::peer::state::Ban {
        addr: "192.0.2.61".to_string(),
        key: None,
        since_ms: NOW,
        reason: teamclaude_rs::peer::state::BanReason::Blocked,
    });
    teamclaude_rs::peer::state::save(&path, &value).expect("the state file writes");

    // Something widened it, so `load` will not trust it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("widen it by hand");

    let row = teamclaude_rs::peer::state::LeaseRow {
        lease: lease_with(LEASE, 0.500, 0.0),
        peer: PeerId([51_u8; 32]),
        scope: tcr_peer_wire::LendScope::All,
    };
    let refusal = teamclaude_rs::peer::state::save_leases(&path, std::slice::from_ref(&row))
        .expect_err("a quarantined read must refuse the write");
    let refusal = format!("{refusal:#}");
    assert!(
        refusal.contains(&path.display().to_string()),
        "the refusal names the file: {refusal}"
    );
    assert!(
        refusal.contains(".corrupt-"),
        "and where the evidence went: {refusal}"
    );
    assert!(
        !path.exists(),
        "no partial state is published in its place: the next boot starts cold and says so"
    );

    // And the ordinary cases still write. A MISSING file is not this case:
    // there is nothing to lose and a first grant has to create it.
    teamclaude_rs::peer::state::save_leases(&path, std::slice::from_ref(&row))
        .expect("a missing state file is created by the first write of a lease row");
    let created = teamclaude_rs::peer::state::load(&path, NOW).expect("it reads back");
    assert_eq!(created.leases.len(), 1);

    // A TRUSTED file keeps every other key through the same write.
    let mut trusted = created;
    trusted.banned.push(teamclaude_rs::peer::state::Ban {
        addr: "192.0.2.62".to_string(),
        key: None,
        since_ms: NOW,
        reason: teamclaude_rs::peer::state::BanReason::Blocked,
    });
    teamclaude_rs::peer::state::save(&path, &trusted).expect("write the ban");
    teamclaude_rs::peer::state::save_leases(&path, &[]).expect("clearing the lease rows writes");
    let after = teamclaude_rs::peer::state::load(&path, NOW).expect("it reads back");
    assert!(
        after.leases.is_empty(),
        "the lease rows are the ones replaced"
    );
    assert_eq!(
        after.banned.len(),
        1,
        "and the operator's block list survives a lease write, as it always did"
    );
}

// ---------------------------------------------------------------------------
// Fixtures. Obviously fake, and no account, org or workspace identity anywhere:
// a lease carries none by construction and neither does a fixture of one.
// ---------------------------------------------------------------------------

/// A fixed clock, so nothing here depends on when it ran.
const NOW: i64 = 1_700_000_000_000;
const LEASE: u128 = 0x1111_1111_1111_1111_1111_1111_1111_1111;
const REQUEST: u128 = 0x2222_2222_2222_2222_2222_2222_2222_2222;

/// A live lease on `7d` with `budget` to spend and `spent` already gone, two
/// requests at a time, expiring five minutes after [`NOW`].
/// A fleet that can hold ANY scope, for a test whose subject is not
/// enforceability.
///
/// `Manager` answers this in production and `NoFleetUtilization` fails closed
/// (every scope but `All` is unenforceable), so a ledger test about a `Group`
/// scope needs a third answer: yes, this Mac could serve inside that scope.
/// Named rather than inlined per test, because "the scope is holdable" is the
/// premise those tests share.
struct AnyScope;

impl serve::WindowUtilization for AnyScope {
    fn read(&self, _window: Window) -> Vec<Option<f64>> {
        Vec::new()
    }

    fn scope_restriction(&self, scope: &tcr_peer_wire::LendScope) -> serve::ScopeRestriction {
        match scope {
            tcr_peer_wire::LendScope::All => serve::ScopeRestriction::Unrestricted,
            tcr_peer_wire::LendScope::Group(name) => serve::ScopeRestriction::Group(name.clone()),
            tcr_peer_wire::LendScope::Accounts(labels) => {
                serve::ScopeRestriction::Accounts(labels.clone())
            }
        }
    }
}

fn lease_with(lease_id: u128, budget: f64, spent: f64) -> Lease {
    Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(budget),
        granted_at_ms: NOW,
        expires_at_ms: NOW + 300_000,
        spent,
        max_inflight: 2,
        until: None,
    }
}

/// A live lease measured in TOKENS, on a clock the ledger reads as now.
///
/// Every other fixture here grants a `Fraction`, and for most gates that is the
/// right unit. It is the wrong one for anything about a token CHARGE:
/// `Ledger::tokens_of` answers `None` for a fraction by design, because the
/// tokens a share of a window buys is a fact about upstream's pricing that
/// nothing on this Mac holds. So a fraction lease charges nothing whatever the
/// meter is told, and a gate written on one passes with its own subject
/// deleted.
///
/// `now_ms` is a parameter rather than [`NOW`] for the same reason: a served
/// lease is debited against the ledger's own clock, and the fixed fixture clock
/// is two years in the past, so a lease built on it is expired before the test
/// starts.
fn token_lease_at(lease_id: u128, tokens: u64, now_ms: i64) -> Lease {
    Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Tokens(tokens),
        granted_at_ms: now_ms,
        expires_at_ms: now_ms + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    }
}

fn ask_for(path: &str) -> Ask<'_> {
    Ask {
        path,
        method: "POST",
        model: Some("claude-sonnet-4-5"),
        group: None,
        affinity: None,
        tried_local: 3,
        body: Bytes::from_static(b"{}"),
        headers: Ask::scrubbed(&client_headers()),
    }
}

/// The headers a real client sends with a message request, credentials
/// included.
///
/// `Ask::scrubbed` removes the credentials, which is the point: a borrow built from
/// this map carries `anthropic-version` and `content-type` (without which the
/// API answers 400) and carries no bearer of the client's.
fn client_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("anthropic-version", "2023-06-01"),
        ("authorization", "Bearer not-a-real-client-token"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    headers
}

/// One pinned row in a LENDER's file: a peer it will serve for.
fn lender_row_for(peer: PeerId, lend: Vec<LendGrant>) -> PeerRow {
    PeerRow {
        node: peer,
        label: "borrowing-mac".to_string(),
        endpoints: Vec::new(),
        added_at: NOW,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay: false,
            gateway: false,
            carry: false,
            inspect: true,
            allow_disclose: false,
            accept_move: false,
            control: ControlGrants::default(),
        },
        lend,
    }
}

/// One pinned row in a BORROWER's file: a peer it may disclose to.
fn borrower_row_for(peer: PeerId, disclose: bool, addrs: Vec<String>) -> PeerRow {
    PeerRow {
        node: peer,
        label: "lending-mac".to_string(),
        endpoints: addrs
            .iter()
            .map(|addr| {
                Endpoint::direct(
                    addr.parse().expect("a fixture address is a socket address"),
                    0,
                    EndpointSource::Paired,
                )
            })
            .collect(),
        added_at: NOW,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay: false,
            gateway: false,
            carry: false,
            inspect: false,
            allow_disclose: disclose,
            accept_move: false,
            control: ControlGrants::default(),
        },
        lend: Vec::new(),
    }
}

/// Write a peers file through the program's own writer, so the mode is the one
/// `read_or_default` insists on rather than whatever a test happened to set.
fn write_peers(path: &Path, peers: Vec<PeerRow>) {
    let file = PeerFile {
        peers,
        ..PeerFile::default()
    };
    teamclaude_rs::peer::config::save(path, &file).expect("the peers file writes");
}

// ---------------------------------------------------------------------------
// The fleet harness: a real `mitm::serve` in front of a real `Manager`, the
// same shape `tests/throttle_exempt.rs` uses. Nothing here reaches the network
// and nothing here touches the operator's config directory: every listener is `127.0.0.1:0` and
// every account is an obviously fake fixture.
// ---------------------------------------------------------------------------

mod fleet {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::any;
    use axum::Router;
    use teamclaude_rs::config::{Account, Config, PacingConfig, ProxyConfig, ThrottleConfig};
    use teamclaude_rs::manager::Manager;
    use teamclaude_rs::oauth::{OAuthError, RefreshFuture, TokenRefresher};
    use teamclaude_rs::probe::{ProbeError, ProbeFuture, UsageProber};
    use teamclaude_rs::warmer::{AccountWarmer, WarmError, WarmFuture};

    /// The canned body the fake upstream answers with. A relayed request that
    /// reached a real account comes back as exactly these bytes.
    pub const CANNED: &[u8] = br#"{"type":"message","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;

    /// The lender's one account. Its access token is the instrument: the fake
    /// upstream records the Bearer it was called with, so "served on the
    /// LENDER's account" is measured rather than assumed.
    pub const LENDER_TOKEN: &str = "at-fake-lender";

    struct NeverRefreshes;
    impl TokenRefresher for NeverRefreshes {
        fn refresh(&self, _refresh_token: String) -> RefreshFuture {
            Box::pin(async { Err(OAuthError::Transient("no refresher in lease tests".into())) })
        }
    }
    struct NeverProbes;
    impl UsageProber for NeverProbes {
        fn probe(&self, _access_token: String) -> ProbeFuture {
            Box::pin(async {
                Err(ProbeError {
                    status: None,
                    message: "no prober in lease tests".into(),
                    retry_after_secs: None,
                })
            })
        }
    }
    struct NeverWarms;
    impl AccountWarmer for NeverWarms {
        fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
            Box::pin(async {
                Err(WarmError {
                    status: None,
                    message: "no warmer in lease tests".into(),
                })
            })
        }
    }

    fn account() -> Account {
        Account {
            name: "lender-fake".to_string(),
            account_type: "oauth".to_string(),
            account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            // One org, so every borrowed request lands in ONE per-org GCRA
            // bucket: which is the bucket the pacing gate measures.
            org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
            org_name: None,
            access_token: LENDER_TOKEN.to_string(),
            refresh_token: Some("rt-fake-lender".to_string()),
            expires_at: Some(teamclaude_rs::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: teamclaude_rs::config::Egress::Local,
            egress_strict: false,
            extra: serde_json::Map::new(),
        }
    }

    fn config(upstream: &str, accounts: Vec<Account>, account_throttle: ThrottleConfig) -> Config {
        Config {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            renamed_accounts: Vec::new(),
            rename_write_error: None,
            proxy: ProxyConfig {
                port: 0,
                api_key: None,
                extra: serde_json::Map::new(),
            },
            upstream: upstream.to_string(),
            switch_threshold: 0.98,
            fable_weekly_threshold: None,
            pacing: PacingConfig::default(),
            account_throttle,
            fleet_throttle: ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            control_pooled: false,
            reset_urgency_tier_hours: 24,
            http1_only: false,
            accounts,
            group_settings: std::collections::HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
            extra: serde_json::Map::new(),
        }
    }

    /// A manager with ONE account: the lender's fleet.
    pub fn lending_manager(upstream: &str, account_throttle: ThrottleConfig) -> Arc<Manager> {
        Manager::new(
            config(upstream, vec![account()], account_throttle),
            Arc::new(NeverRefreshes),
            Arc::new(NeverProbes),
            Arc::new(NeverWarms),
            None,
        )
    }

    /// A fake upstream whose answer is `bytes` long: more than one relayed
    /// frame carries, which is any long completion.
    pub async fn spawn_huge_upstream(bytes: usize) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().fallback(any(move |req: axum::extract::Request| {
            let counter = counter.clone();
            async move {
                let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
                counter.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .header("anthropic-ratelimit-unified-status", "allowed")
                    .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                    .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                    .body(Body::from(vec![b'x'; bytes]))
                    .expect("build the oversized answer")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the oversized upstream");
        let addr = listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    /// A fake upstream that holds every answer for `delay` before sending it,
    /// so a borrow whose deadline is shorter runs out of time AFTER its body
    /// has crossed to the lender and reached the origin.
    pub async fn spawn_slow_upstream(delay: std::time::Duration) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().fallback(any(move |req: axum::extract::Request| {
            let counter = counter.clone();
            async move {
                let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
                // Counted on ARRIVAL, before the delay: the question this
                // upstream answers is how many times the fleet sent one
                // request, and a request still in flight has already been paid
                // for.
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .header("anthropic-ratelimit-unified-status", "allowed")
                    .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                    .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                    .body(Body::from(CANNED.to_vec()))
                    .expect("build the canned answer")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the slow upstream");
        let addr = listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    /// A fake upstream that rejects every request with a long 429, so the one
    /// account that tried it is put on a hold the picker will not come back to
    /// inside this test.
    pub async fn spawn_rejecting_upstream() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().fallback(any(move |req: axum::extract::Request| {
            let counter = counter.clone();
            async move {
                let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
                counter.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(429)
                    .header("content-type", "application/json")
                    .header("retry-after", "3600")
                    .header("anthropic-ratelimit-unified-status", "rejected")
                    .header("anthropic-ratelimit-unified-5h-status", "rejected")
                    .body(Body::from(
                        br#"{"type":"error","error":{"type":"rate_limit_error"}}"#.to_vec(),
                    ))
                    .expect("build the canned 429")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the rejecting upstream");
        let addr = listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    /// A fleet of ONE account, LOCKED to that account by name.
    ///
    /// `lock_account` is the operator saying "this account or nothing", so the
    /// picker answers `None` and the request reaches the same terminal a dry
    /// fleet does. The difference is what may happen there: a dry fleet may
    /// borrow, and a locked one may not, because a peer's credential is not
    /// the account the operator pinned this fleet to.
    pub fn locked_manager(upstream: &str) -> Arc<Manager> {
        let locked = account();
        let name = locked.name.clone();
        let mut config = config(upstream, vec![locked], ThrottleConfig::default());
        config.lock_account = Some(name);
        Manager::new(
            config,
            Arc::new(NeverRefreshes),
            Arc::new(NeverProbes),
            Arc::new(NeverWarms),
            None,
        )
    }

    /// A manager with NO accounts at all: the dry fleet, which is the only
    /// condition under which the fallback seam is consulted.
    pub fn dry_manager() -> Arc<Manager> {
        Manager::new(
            config("http://127.0.0.1:1", Vec::new(), ThrottleConfig::default()),
            Arc::new(NeverRefreshes),
            Arc::new(NeverProbes),
            Arc::new(NeverWarms),
            None,
        )
    }

    /// One request the fake upstream answered, as the LENDER's own proxy
    /// actually sent it.
    ///
    /// Grew from a bare Bearer string: the header ALLOWLIST gate
    /// asks what the lender's outbound request carried, and a recorder that
    /// keeps one header cannot answer a question about the others.
    #[derive(Debug, Clone)]
    pub struct SeenRequest {
        /// The Bearer, with `Bearer ` stripped, or `<none>`.
        pub token: String,
        /// Every header name and value, lower-cased names.
        pub headers: Vec<(String, String)>,
    }

    impl SeenRequest {
        /// The first value under `name`, or `None`.
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(seen, _)| seen == name)
                .map(|(_, value)| value.as_str())
        }

        /// Every header value, for an assertion about a SECRET rather than
        /// about a name: a borrower's cookie arriving under a name nobody
        /// thought to check is invisible to a per-name assertion.
        pub fn values(&self) -> Vec<&str> {
            self.headers
                .iter()
                .map(|(_, value)| value.as_str())
                .collect()
        }
    }

    /// Every request the fake upstream was called with, in order.
    pub type ServedBy = Arc<Mutex<Vec<SeenRequest>>>;

    /// A fake upstream on a kernel port that answers [`CANNED`] and records who
    /// asked.
    pub async fn spawn_upstream() -> (String, Arc<AtomicUsize>, ServedBy) {
        let hits = Arc::new(AtomicUsize::new(0));
        let served: ServedBy = Arc::new(Mutex::new(Vec::new()));
        let counter = hits.clone();
        let recorder = served.clone();
        let app = Router::new().fallback(any(move |req: axum::extract::Request| {
            let counter = counter.clone();
            let recorder = recorder.clone();
            async move {
                let token = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("<none>")
                    .trim_start_matches("Bearer ")
                    .to_string();
                // The origin's own refusal. Decided BEFORE the body is drained
                // and answered after the request has been recorded: the
                // request really did arrive, so an instrument that counts
                // arrivals must see it, and a request the real API answers 400
                // to is not one this fake upstream may answer 200 to. See
                // `tests/tools/api_contract.rs`.
                let refusal =
                    super::api_contract::refuse_if_incomplete(req.method(), req.headers());
                let headers = req
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_ascii_lowercase(),
                            String::from_utf8_lossy(value.as_bytes()).to_string(),
                        )
                    })
                    .collect();
                let _ = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
                counter.fetch_add(1, Ordering::SeqCst);
                recorder
                    .lock()
                    .expect("served-by lock")
                    .push(SeenRequest { token, headers });
                if let Some(refusal) = refusal {
                    return refusal;
                }
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .header("anthropic-ratelimit-unified-status", "allowed")
                    .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                    .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                    .body(Body::from(CANNED.to_vec()))
                    .expect("build the canned answer")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake upstream");
        let addr = listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits, served)
    }

    /// A real proxy on a kernel port in front of `manager`.
    pub async fn spawn_proxy(manager: Arc<Manager>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let addr = listener.local_addr().expect("proxy addr");
        tokio::spawn(async move {
            teamclaude_rs::mitm::serve(listener, manager, None).await;
        });
        format!("http://{addr}")
    }

    pub fn tight_throttle() -> ThrottleConfig {
        ThrottleConfig {
            min_spacing_ms: Some(350),
            burst: Some(1),
        }
    }
}

// ---------------------------------------------------------------------------
// The mesh harness: one lender and one borrower in this one test binary, on
// kernel ports.
//
// **The lender is the REAL listener now.** A harness used to stand in for the
// one line that joins the two halves, because `listener::serve_stream` matched
// `StreamKind::Serve` to a `bail!`. That arm is now landed, so
// [`spawn_lender`] hands a kernel-port `TcpListener` to
// `listener::serve_on_with` and the production dispatch is what answers: the
// gate, the per-frame row re-read, the refusal log and the handler are all the
// shipped code, not a copy of it here.
//
// [`spawn_capturing_lender`] is the one harness that remains, and it exists to
// see something the real listener deliberately never exposes: the DECRYPTED
// request frame. See its doc.
// ---------------------------------------------------------------------------

mod mesh {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use tcr_peer_wire::{Control, Lease, LeaseGrant, LeaseUnit, StreamHeader, StreamKind, Window};
    use teamclaude_rs::manager::Manager;
    use teamclaude_rs::peer::config::PeerStore;
    use teamclaude_rs::peer::id::NodeKey;
    use teamclaude_rs::peer::lease::Ledger;
    use teamclaude_rs::peer::listener::{self, LeaseServing, SessionContext};
    use teamclaude_rs::peer::noise;
    use teamclaude_rs::peer::serve::{self, WindowUtilization};
    use tokio::net::TcpListener;

    use super::fleet;

    /// A utilization reader whose answers are scripted rather than measured.
    ///
    /// `handle_serve_on` reads the lease's window twice around one relay, so a
    /// two-element script is one request's before and after, and the debit that
    /// results is a figure the test chose. Past the end of the script the last
    /// answer repeats, so a harness written for one relay does not panic on a
    /// second.
    ///
    /// The real reader is `Manager`, which reads its accounts' quota; measuring
    /// a real rise needs the fake upstream to move the lender's utilization
    /// between two probes, which is a test of `update_from_headers` rather than
    /// of the debit.
    pub struct ScriptedUtilization {
        script: Vec<Vec<Option<f64>>>,
        taken: Mutex<usize>,
    }

    impl ScriptedUtilization {
        /// A reader that answers `script[0]`, then `script[1]`, and so on.
        pub fn new(script: Vec<Vec<Option<f64>>>) -> Self {
            assert!(!script.is_empty(), "a script with no reads answers nothing");
            Self {
                script,
                taken: Mutex::new(0),
            }
        }

        /// How many reads happened. The positive control on the instrument: a
        /// reader nobody called would satisfy a debit assertion by accident.
        pub fn reads_taken(&self) -> usize {
            *self.taken.lock().expect("scripted-utilization lock")
        }
    }

    impl WindowUtilization for ScriptedUtilization {
        fn read(&self, _window: Window) -> Vec<Option<f64>> {
            let mut taken = self.taken.lock().expect("scripted-utilization lock");
            let at = (*taken).min(self.script.len() - 1);
            *taken += 1;
            self.script[at].clone()
        }
    }

    /// Stand up the lender's REAL peer listener and serve SERVE streams on it.
    ///
    /// `listener::serve_on_with` is the shipped accept loop, so what answers a
    /// borrower here is the production handshake, the production stream gate,
    /// the production per-frame row re-read and the production dispatch arm.
    /// The only thing this function chooses is what a listener cannot derive:
    /// the ledger, this node's own proxy base and the quota reader
    /// ([`LeaseServing`]).
    ///
    /// The pairing-window file is inside `key_dir` and does not exist, so this
    /// test reads nothing under the operator's cache directory and cannot be perturbed by an
    /// operator who has `tcr peer pair` open while the suite runs. A missing
    /// file reads as a closed window, which is what a pinned-peer `IK` return
    /// needs anyway.
    pub async fn spawn_lender(
        key_dir: PathBuf,
        peers_path: PathBuf,
        ledger: Arc<Mutex<Ledger>>,
        upstream: String,
        utilization: Arc<dyn WindowUtilization>,
        manager: Arc<Manager>,
    ) -> SocketAddr {
        let listening = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the lender's peer listener");
        let addr = listening.local_addr().expect("lender peer addr");

        let key = NodeKey::load_or_mint(&key_dir).expect("the lender's node key");
        let store = PeerStore::open(&peers_path).expect("the lender's peers file");
        let state_path = key_dir.join("peer-state.json");
        let context = SessionContext::new(&key, store.path(), &state_path).with_lease_serving(
            Some(LeaseServing {
                ledger,
                upstream,
                utilization,
                manager,
            }),
        );

        tokio::spawn(async move {
            // The loop only ever ends by the listener failing, which in this
            // binary means the test finished and the socket was dropped.
            let _ = listener::serve_on_with(listening, context).await;
        });
        addr
    }

    /// A lender that reads a borrow and answers nothing at all.
    ///
    /// The shape of EVERY lender-side refusal that happens before an upstream
    /// is reached: the header gate (`listener.rs`, `inspect` turned off while a
    /// borrower still holds a lease), and each `bail!` in
    /// `serve::handle_serve_on` (a wire version, a path, a method or a request
    /// id this build will not serve). All of them close the stream with no
    /// frame written back.
    ///
    /// It reads the frames a lender reads and then goes silent, rather than
    /// closing the socket, because a close is a RACE on loopback: the
    /// borrower's own write can fail with the connection already gone, which
    /// hides the defect behind the kernel's timing. A lender that simply never
    /// answers is the same fact to the borrower and it is the same every run.
    pub async fn spawn_lender_that_takes_nothing(
        key_dir: PathBuf,
        peers_path: PathBuf,
    ) -> SocketAddr {
        let listening = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the silent lender");
        let addr = listening.local_addr().expect("silent lender addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listening.accept().await else {
                    return;
                };
                let key_dir = key_dir.clone();
                let peers_path = peers_path.clone();
                tokio::spawn(async move {
                    let key = NodeKey::load_or_mint(&key_dir).expect("the lender's node key");
                    let store = PeerStore::open(&peers_path).expect("the lender's peers file");
                    let rows = store.peers();
                    let Ok(mut session) = noise::accept_handshake(
                        &mut stream,
                        key.secret_bytes(),
                        noise::Handshake::Return,
                        &[],
                        move |remote| noise::pin_check_rows(remote, &rows),
                    )
                    .await
                    else {
                        return;
                    };
                    let Ok(header) =
                        serve::recv_control::<_, StreamHeader>(&mut stream, &mut session).await
                    else {
                        return;
                    };
                    assert_eq!(header.kind, StreamKind::Serve);
                    // The request frame, read exactly as a lender reads it, and
                    // then nothing: no ack, no reply, and the stream held open
                    // so the borrower's own deadline is what ends this.
                    let _ = noise::recv_encrypted(&mut stream, &mut session.transport).await;
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                });
            }
        });
        addr
    }

    /// Every DECRYPTED request frame the lender read, in order.
    pub type CapturedFrames = Arc<Mutex<Vec<Vec<u8>>>>;

    /// A lender that captures the decrypted [`serve::ServeRequest`] frame and
    /// answers a canned reply, serving nothing.
    ///
    /// # Why this is not the real listener
    ///
    /// The borrower's scrub is the check that matters, and the only place its
    /// effect is VISIBLE is the frame the lender decrypted. On the wire that
    /// frame is Noise ciphertext, so an assertion there is an assertion about
    /// randomness; inside the real listener it is a local variable nothing
    /// exposes, and exposing it would mean a production seam that hands a
    /// borrower's plaintext request to a caller, which is the opposite of what
    /// this module is for.
    ///
    /// So this harness stands exactly where `handle_serve_on` stands, reads the
    /// same two frames off the same session, and keeps the first one's bytes.
    /// That is what makes `a_relayed_frame_the_lender_receives_holds_no_
    /// credential` a measurement: delete the scrub and the borrower's token is
    /// in these bytes.
    pub async fn spawn_capturing_lender(
        key_dir: PathBuf,
        peers_path: PathBuf,
        captured: CapturedFrames,
    ) -> SocketAddr {
        let listening = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the capturing lender");
        let addr = listening.local_addr().expect("capturing lender addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listening.accept().await else {
                    return;
                };
                let key_dir = key_dir.clone();
                let peers_path = peers_path.clone();
                let captured = captured.clone();
                tokio::spawn(async move {
                    let key = NodeKey::load_or_mint(&key_dir).expect("the lender's node key");
                    let store = PeerStore::open(&peers_path).expect("the lender's peers file");
                    // `noise::accept_handshake` and NOT
                    // `listener::accept_peer_session`, which this was the last
                    // caller of. That function is the pre-decision-10 seam: it
                    // admits an `XX` message 1 from ANY address while a
                    // node-wide pairing window is open, which is exactly what
                    // was replaced with a window keyed to ONE accepted
                    // instance id. Nothing in production reached it: the
                    // listener calls `accept_pairing_or_return`: so it was a
                    // retired shape kept alive by one test, and this caller
                    // had to go before that function could be deleted.
                    //
                    // Nothing is lost here. A borrower dials a lender with
                    // `Handshake::Return` (`serve::open_serve`), the pattern
                    // this harness only ever exercised, and the authorization
                    // is the SAME one the production responder applies:
                    // `pin_check_rows` against the peers file's own rows,
                    // between message 1 and message 2, with nothing written
                    // until it has answered.
                    let rows = store.peers();
                    let mut session = match noise::accept_handshake(
                        &mut stream,
                        key.secret_bytes(),
                        noise::Handshake::Return,
                        &[],
                        move |remote| noise::pin_check_rows(remote, &rows),
                    )
                    .await
                    {
                        Ok(session) => session,
                        Err(err) => {
                            eprintln!("capturing lender: the handshake failed: {err:#}");
                            return;
                        }
                    };
                    let header: StreamHeader =
                        match serve::recv_control(&mut stream, &mut session).await {
                            Ok(header) => header,
                            Err(err) => {
                                eprintln!("capturing lender: no stream header: {err:#}");
                                return;
                            }
                        };
                    assert_eq!(header.kind, StreamKind::Serve);

                    // The request frame, kept as the bytes that came out of the
                    // Noise transport: before `serde_json` has had a chance to
                    // drop a field this build does not know about.
                    let frame = noise::recv_encrypted(&mut stream, &mut session.transport)
                        .await
                        .expect("the request frame");
                    captured
                        .lock()
                        .expect("captured-frames lock")
                        .push(frame.clone());
                    let request: serve::ServeRequest =
                        serde_json::from_slice(&frame).expect("the request frame parses");
                    // The ack, where a lender writes it: the borrower holds its
                    // body until this frame arrives.
                    serve::send_control(&mut stream, &mut session, &serve::ServeAck::Accepted)
                        .await
                        .expect("the ack frame");
                    let _body = noise::recv_encrypted(&mut stream, &mut session.transport)
                        .await
                        .expect("the body frame");
                    let end = noise::recv_encrypted(&mut stream, &mut session.transport)
                        .await
                        .expect("the body terminator");
                    assert!(end.is_empty(), "a body ends with an empty frame");
                    assert_eq!(request.body_bytes, 2, "the fixture ask carries `{{}}`");

                    let reply = serve::ServeReply::Served {
                        status: 200,
                        headers: vec![("content-type".to_string(), "application/json".to_string())],
                        body_bytes: fleet::CANNED.len(),
                    };
                    serve::send_control(&mut stream, &mut session, &reply)
                        .await
                        .expect("the reply frame");
                    noise::send_encrypted(&mut stream, &mut session.transport, fleet::CANNED)
                        .await
                        .expect("the reply body");
                    noise::send_encrypted(&mut stream, &mut session.transport, b"")
                        .await
                        .expect("the reply body terminator");
                });
            }
        });
        addr
    }

    /// How many `Control::LeaseRequest` frames a lender has read.
    pub type LeaseAsks = Arc<std::sync::atomic::AtomicUsize>;

    /// A lender that COUNTS lease asks, grants every one of them, and serves
    /// whatever is then relayed to it.
    ///
    /// # Why not `spawn_lender`
    ///
    /// The real listener is the right harness for what a lender DECIDES, and it
    /// is the wrong one for how many times it was ASKED: the ask count lives in
    /// the listener's own control arm as a local, and the nearest readable proxy
    /// (how many leases the ledger minted), is one layer past the question. A
    /// lender that refused an ask, or minted one lease for two asks, would read
    /// identically there. So this stands where the control arm stands, counts
    /// the decrypted `Control::LeaseRequest` frames, and answers a canned grant.
    ///
    /// `answer_delay` is the whole instrument: it holds the first ask open long
    /// enough that four more land inside it, which is the race
    /// `PeerLeaseProvider::lease_for`'s ask gate exists to close. Without the
    /// gate this counter reads five.
    pub async fn spawn_counting_lease_lender(
        key_dir: PathBuf,
        peers_path: PathBuf,
        asks: LeaseAsks,
        answer_delay: std::time::Duration,
    ) -> SocketAddr {
        let listening = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the counting lender");
        let addr = listening.local_addr().expect("counting lender addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listening.accept().await else {
                    return;
                };
                let key_dir = key_dir.clone();
                let peers_path = peers_path.clone();
                let asks = asks.clone();
                tokio::spawn(async move {
                    let key = NodeKey::load_or_mint(&key_dir).expect("the lender's node key");
                    let store = PeerStore::open(&peers_path).expect("the lender's peers file");
                    let rows = store.peers();
                    let mut session = match noise::accept_handshake(
                        &mut stream,
                        key.secret_bytes(),
                        noise::Handshake::Return,
                        &[],
                        move |remote| noise::pin_check_rows(remote, &rows),
                    )
                    .await
                    {
                        Ok(session) => session,
                        Err(err) => {
                            eprintln!("counting lender: the handshake failed: {err:#}");
                            return;
                        }
                    };
                    let header: StreamHeader =
                        match serve::recv_control(&mut stream, &mut session).await {
                            Ok(header) => header,
                            Err(err) => {
                                eprintln!("counting lender: no stream header: {err:#}");
                                return;
                            }
                        };
                    match header.kind {
                        StreamKind::Control => {
                            let control: Control = serve::recv_control(&mut stream, &mut session)
                                .await
                                .expect("the control frame");
                            assert!(
                                matches!(control, Control::LeaseRequest(_)),
                                "this lender answers lease asks and nothing else, got {control:?}"
                            );
                            asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            tokio::time::sleep(answer_delay).await;
                            let now_ms = teamclaude_rs::now_ms();
                            let grant = LeaseGrant {
                                lease: Some(Lease {
                                    lease_id: 0xc0_ffee,
                                    window: Window::SevenDay,
                                    unit: LeaseUnit::Fraction(0.20),
                                    granted_at_ms: now_ms,
                                    expires_at_ms: now_ms + 300_000,
                                    spent: 0.0,
                                    max_inflight: 2,
                                    until: None,
                                }),
                                refusal: None,
                            };
                            serve::send_control(
                                &mut stream,
                                &mut session,
                                &Control::LeaseGrant(grant),
                            )
                            .await
                            .expect("the grant frame");
                        }
                        StreamKind::Serve => {
                            let frame = noise::recv_encrypted(&mut stream, &mut session.transport)
                                .await
                                .expect("the request frame");
                            let _request: serve::ServeRequest =
                                serde_json::from_slice(&frame).expect("the request frame parses");
                            serve::send_control(
                                &mut stream,
                                &mut session,
                                &serve::ServeAck::Accepted,
                            )
                            .await
                            .expect("the ack frame");
                            let _body = noise::recv_encrypted(&mut stream, &mut session.transport)
                                .await
                                .expect("the body frame");
                            let end = noise::recv_encrypted(&mut stream, &mut session.transport)
                                .await
                                .expect("the body terminator");
                            assert!(end.is_empty(), "a body ends with an empty frame");
                            let reply = serve::ServeReply::Served {
                                status: 200,
                                headers: vec![(
                                    "content-type".to_string(),
                                    "application/json".to_string(),
                                )],
                                body_bytes: fleet::CANNED.len(),
                            };
                            serve::send_control(&mut stream, &mut session, &reply)
                                .await
                                .expect("the reply frame");
                            noise::send_encrypted(
                                &mut stream,
                                &mut session.transport,
                                fleet::CANNED,
                            )
                            .await
                            .expect("the reply body");
                            noise::send_encrypted(&mut stream, &mut session.transport, b"")
                                .await
                                .expect("the reply body terminator");
                        }
                        other => panic!("this lender takes no {other:?} stream"),
                    }
                });
            }
        });
        addr
    }
}

/// **The dry-fleet arm reaches the configured provider, and returns what it
/// answered.**
///
/// This is the wiring gate: a real `mitm::serve` in front of a `Manager` with
/// no account that can serve, one POST, and a provider that counts. Before
/// `configured_provider` read an installed provider it returned `None`
/// unconditionally, and this test could not distinguish "the seam is wired" from
/// "the seam exists".
///
/// It is the only test in this binary that installs a provider, because the
/// install is process-wide by design (see `fallback::PROVIDER`): a second
/// installer in the same binary would race this one.
///
/// Watch it fail by making `configured_provider` return `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_dry_fleet_reaches_the_configured_provider() {
    /// What the provider answers with, so the body the client reads proves
    /// WHERE the answer came from rather than merely that one arrived.
    const SENTINEL: &[u8] = b"{\"served_by\":\"the-test-provider\"}";

    struct Counting {
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FallbackProvider for Counting {
        fn name(&self) -> &'static str {
            "test-counting"
        }
        fn try_serve<'a>(
            &'a self,
            ask: &'a Ask<'a>,
        ) -> futures::future::BoxFuture<'a, Option<axum::response::Response>> {
            Box::pin(async move {
                self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.paths
                    .lock()
                    .expect("paths lock")
                    .push(ask.path.to_string());
                Some(
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(SENTINEL.to_vec()))
                        .expect("build the provider's answer"),
                )
            })
        }
    }

    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        fallback::install_provider(Box::new(Counting {
            asked: asked.clone(),
            paths: paths.clone(),
        })),
        "nothing else in this binary may install a provider"
    );

    let proxy = fleet::spawn_proxy(fleet::dry_manager()).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a loopback client");
    let response = client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the proxy answered");

    assert_eq!(response.status().as_u16(), 200);
    let body = response.bytes().await.expect("read the body");
    assert_eq!(
        body.as_ref(),
        SENTINEL,
        "the client reads the PROVIDER's answer, not the exhausted 429"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the dry-fleet arm consulted the provider exactly once"
    );
    assert_eq!(
        paths.lock().expect("paths lock").as_slice(),
        ["/v1/messages".to_string()],
        "the ask carries the request's own path"
    );

    // ---------------------------------------------------------------------
    // **Hole B, executable**: the dry-fleet arm REFUSES TO BUILD an `Ask` for a
    // client-credential path, so no provider is ever offered one: not even a
    // provider that would have said yes to anything.
    //
    // This is the half of the defence that lives at the seam rather than in the
    // provider (`src/proxy.rs:2352`). It was prose and a `.then()` with nothing
    // exercising it: the provider's OWN refusal
    // (`the_provider_declines_a_credential_path_without_consulting_a_lease`)
    // passes whether or not the seam refuses, because the provider refuses
    // again. The instrument that can tell them apart is a provider that refuses
    // NOTHING (`Counting` above), which is why this assertion has to live in
    // the one test allowed to install one.
    //
    // # Which of these paths the seam is actually the ENFORCEMENT point for
    //
    // Measured, because a loop over seven paths reads as seven gates and is
    // not: `relay_mode` (`src/proxy.rs:1052`) intercepts a request before
    // rotation ever runs, so the six `CLIENT_CREDENTIAL_PREFIXES` never reach
    // the picker at all, whatever their method, and neither does a `POST
    // /v1/oauth/token`. For those, the seam's refusal is a backstop against
    // `relay_mode` and this list drifting apart: real, and not the thing this
    // assertion can see.
    //
    // **`GET /v1/oauth/token` is the one shape that reaches the seam.**
    // `relay_mode`'s first arm is `POST`-only, so a non-POST spelling of the
    // seventh path falls through to the picker, comes up dry, and arrives here
    //: where `serve_is_allowed_for_path` is the only thing between a client's
    // own credential exchange and another Mac's process. Mutating the `.then()`
    // to build the `Ask` unconditionally leaves every POST in this loop green
    // and turns this one red, which is how the distinction above was measured
    // rather than reasoned.
    // ---------------------------------------------------------------------
    for path in ["/v1/oauth/token", "/v1/code", "/_tcr/accounts"] {
        let refused = client
            .post(format!("{proxy}{path}"))
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
            .send()
            .await
            .expect("the proxy answered");
        assert_ne!(
            refused.bytes().await.expect("read the body").as_ref(),
            SENTINEL,
            "{path} was served by a fallback provider; the seam must not build an Ask for it"
        );
    }
    let slipped = client
        .get(format!("{proxy}/v1/oauth/token"))
        .send()
        .await
        .expect("the proxy answered");
    assert_ne!(
        slipped.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a GET to the token-exchange path slipped past `relay_mode` and was relayed to \
         another Mac: this is the shape the seam's own refusal is the enforcement point for"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider was consulted exactly once, for /v1/messages: a credential path \
         never reaches it, because the seam builds no Ask at all"
    );

    // ---------------------------------------------------------------------
    // **Item 3, the borrower side: a NON-POST never reaches a provider.**
    //
    // `ServeRequest::method` used to be written as the literal `"POST"` by the
    // borrower and read by nobody, so a client that sent `GET /v1/messages`
    // had it POSTed on somebody else's account: an answer to a question it
    // never asked, paid for out of a lease. `handle_serve_on` refuses that
    // frame now, and that refusal is the LENDER's: it happens after the bytes
    // have crossed a host boundary, on a machine whose operator did not ask
    // for them.
    //
    // So the refusal is at the seam, and this is the instrument that can see
    // it: a provider that refuses nothing. `GET /v1/messages` is a path the
    // seam is otherwise happy to relay: the loop above proves the path
    // refusal is about the PATH: so the only thing that can stop it here is
    // the method.
    //
    // The answer the client gets is whatever this handler answered before any
    // of this existed: `exhausted_or_fallback`'s terminal, with no provider
    // consulted. Not an error page invented for the occasion.
    //
    // Watch it fail by dropping `method == axum::http::Method::POST` from the
    // `.then()` in `src/proxy.rs`: `asked` goes to 2 and the body is the
    // provider's sentinel.
    // ---------------------------------------------------------------------
    let non_post = client
        .get(format!("{proxy}/v1/messages"))
        .send()
        .await
        .expect("the proxy answered");
    let status = non_post.status().as_u16();
    assert_ne!(
        non_post.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a GET was relayed to another Mac and would have been POSTed on its account"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider is still at one ask: a non-POST builds no `Ask` at all"
    );
    assert_eq!(
        status, 429,
        "a non-POST on a dry fleet gets the honest exhausted answer this handler has \
         always given, not something invented for the peer path"
    );

    // ---------------------------------------------------------------------
    // **A HARD ACCOUNT LOCK NEVER REACHES A PROVIDER.**
    //
    // `lock_account` is the operator saying this fleet is one account, and the
    // arm that answers a locked request already refuses the revalidation serve
    // for exactly that reason: it must not send the request through another
    // POOLED credential. It was then changed to call `exhausted_or_fallback`,
    // which consults a provider that never reads the lock, so a request pinned
    // to one account was served on a PEER's credential the moment its own
    // account could not serve: a wider escape than the one the line above it
    // refuses, because the credential is not even this Mac's.
    //
    // A SECOND proxy in this test rather than a second test, because the
    // provider install is process-wide and this is the one test allowed to do
    // it. The instrument is the same one: a provider that says yes to
    // anything, so the only thing that can produce a 429 here is the lock.
    //
    // Watch it fail by removing the `locked` conjunct from
    // `exhausted_or_fallback` (`src/proxy.rs`): the body is the sentinel, the
    // status is 200 and `asked` goes to 2.
    // ---------------------------------------------------------------------
    let (rejecting, rejections) = fleet::spawn_rejecting_upstream().await;
    let locked_proxy = fleet::spawn_proxy(fleet::locked_manager(&rejecting)).await;
    let refused = client
        .post(format!("{locked_proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the locked proxy answered");
    let locked_status = refused.status().as_u16();
    assert_ne!(
        refused.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a request pinned to ONE account was served on a peer's credential"
    );
    assert_eq!(
        locked_status, 429,
        "a locked account that cannot serve gets the honest exhausted answer, not a \
         borrow: the lock has no failover, local or remote"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider is still at one ask: a locked fleet never consults one"
    );
    assert!(
        rejections.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the locked account really did try and really was refused, which is the \
         condition this arm is about"
    );
}

/// **The two-process gate, on the REAL listener: a borrowed request is served
/// on the LENDER's account.**
///
/// One lender and one borrower in this binary, on kernel ports, with a fake
/// upstream behind the lender's own proxy. What answers the borrower is
/// `listener::serve_on_with`: the shipped accept loop, handshake, stream gate
/// and dispatch arm. That used not to be true: `StreamKind::Serve` was a
/// `bail!` and this test drove a harness shaped like the arm that was missing,
/// so it proved the two halves worked and not that anything joined them.
///
/// Three assertions, each a separate claim this design makes:
///
/// 1. the borrower's client reads the canned body: the relay works end to end,
///    through the production dispatch;
/// 2. the fake upstream was called with the LENDER's Bearer: the lender's own
///    picker and its own credential served it, which is also what pays the
///    lender's per-organization GCRA bucket (see the pacing gate below);
/// 3. the lease was debited exactly once and the in-flight slot was released.
///
/// # And a fourth: **this test goes red when the scrub is deleted**
///
/// It did not, and the brief's item 2 is that it must. The credential claim was
/// moved out to `a_relayed_frame_the_lender_receives_holds_no_credential`
/// because it used to live here as an absence in a CIPHERTEXT stream over a
/// request built from an EMPTY header map: an assertion that could not go red
/// however the scrub was broken. Moving it left this test named for the happy
/// path and silent about the thing that makes the happy path safe.
///
/// Two changes bring it back as a measurement. The borrower's own credentials
/// are in scope for the real leg above, so assertion 2 is about a request that
/// HAD a token to leak; and the same ask is then relayed a second time to a
/// CAPTURING lender, which keeps the frame it decrypted: the one place a
/// borrower's plaintext request exists on the lender's side. The real listener
/// deliberately exposes it nowhere, and exposing it would mean a production
/// seam that hands a borrower's plaintext to a caller.
///
/// Watch it fail by returning the dispatch arm to its `bail!`: assertion 1 goes
/// red with the lender closing the stream. Watch assertion 2 fail by pointing
/// `handle_serve_on` at anything other than the lender's own proxy: the Bearer
/// is the thing the lender's own path substitutes. Watch assertion 4 fail by
/// deleting the `scrub_client_credentials(&mut scrubbed)` line from
/// `serve_request_from`: all four secrets are then in the captured frame.
/// Measured red that way before this leg was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrowed_request_is_served_on_the_lenders_account() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");

    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );

    let now = teamclaude_rs::now_ms();
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };
    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        // `record_scoped` with the GRANTEE, not `record`: the review's M1 is
        // fixed, so a lease bound to nobody is a lease `enter_relay` refuses
        // (`RelayRefusal::NotTheGrantee`) and this test's borrow would be
        // refused before it reached the subject.
        held.record_scoped(lease, borrower_id, tcr_peer_wire::LendScope::All);
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }

    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        ledger.clone(),
        lender_proxy.clone(),
        // A lender whose own fleet has never been probed. The rise is then
        // unmeasurable and `Ledger::debit` charges MIN_DEBIT, which is what
        // assertion 3 reads: see `a_served_request_debits_the_measured_rise`
        // for the other half.
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );
    let borrower_store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

    // The borrower's own client's headers, credentials included. An EMPTY map
    // here is what made this test blind to the scrub: nothing could leak, so
    // nothing being leaked proved nothing.
    let headers = borrower_credentials();
    let ask = ask_for("/v1/messages");
    let response = serve::open_serve(&lender_id, &lease, &ask, &headers, &borrower_store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");

    assert_eq!(response.status().as_u16(), 200);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read the relayed body");
    assert_eq!(
        body.as_ref(),
        fleet::CANNED,
        "the borrower's client reads exactly what the lender's account answered"
    );

    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let seen = served.lock().expect("served lock").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].token,
        fleet::LENDER_TOKEN,
        "the request was served on the LENDER's own credential"
    );
    // The borrower's secrets were in scope for that request, so their absence
    // from what the lender sent upstream is a measurement. Over VALUES and not
    // over header names: a secret arriving under a name nobody thought to check
    // is invisible to a per-name assertion.
    for secret in BORROWER_SECRETS {
        assert!(
            !seen[0].values().iter().any(|value| value.contains(secret)),
            "{secret} reached the fake upstream on the lender's own TLS session: {:?}",
            seen[0].headers
        );
    }

    // Assertion 3 in a block of its own: the ledger guard must not be alive
    // across assertion 4's `.await`s below, and a `drop` at the end of a long
    // body is a line the next edit moves without noticing.
    {
        let held = ledger.lock().expect("ledger lock");
        let after = held
            .live(teamclaude_rs::now_ms())
            .first()
            .copied()
            .expect("the lease is still live");
        assert!(
            (after.spent - lease::MIN_DEBIT).abs() < f64::EPSILON,
            "one relayed request, one debit, got {}",
            after.spent
        );
        assert_eq!(
            held.inflight(LEASE),
            0,
            "the in-flight slot is released, or the lease is stranded for its whole TTL"
        );
    }

    // ASSERTION 4, and the one that makes this test go red when the scrub is
    // deleted: the SAME ask, the SAME headers, relayed to a lender that keeps
    // the frame it decrypted.
    //
    // Why a second lender rather than an assertion on the first: the lender
    // above copies only `LENDER_FORWARDED_HEADERS` onto its own outbound
    // request, so a borrower credential that survived the scrub is dropped one
    // hop later and the upstream assertion stays green. That allowlist is a
    // backstop and a good one: and it is exactly what MASKS a broken scrub
    // from every assertion that reads the far end. The frame is the only place
    // the scrub's effect is visible.
    let capture_home = tempfile::tempdir().expect("the capturing lender's temp home");
    let capture_peers = capture_home.path().join("tcr-peers.json");
    let capture_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(capture_home.path())
        .expect("the capturing lender's key")
        .id();
    write_peers(
        &capture_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );
    let captured: mesh::CapturedFrames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture_addr = mesh::spawn_capturing_lender(
        capture_home.path().to_path_buf(),
        capture_peers,
        captured.clone(),
    )
    .await;
    write_peers(
        &borrower_peers,
        vec![
            borrower_row_for(lender_id, true, vec![peer_addr.to_string()]),
            borrower_row_for(capture_id, true, vec![capture_addr.to_string()]),
        ],
    );
    let borrower_store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    serve::open_serve(&capture_id, &lease, &ask, &headers, &borrower_store)
        .await
        .expect("the second SERVE stream ran")
        .served()
        .expect("the capturing lender answered");

    let frames = captured.lock().expect("captured-frames lock").clone();
    assert_eq!(frames.len(), 1, "one request, one captured frame");
    let frame = String::from_utf8_lossy(&frames[0]).to_string();
    // The positive control on the instrument: this is the request's own frame,
    // and a non-credential header survived it: so every absence below is about
    // the scrub and not about an empty or truncated capture.
    assert!(
        frame.contains("/v1/messages"),
        "the captured frame is the request's own: {frame}"
    );
    assert!(
        frame.contains("content-type"),
        "a non-credential header survives, so the scrub removed credentials and not the \
         request: {frame}"
    );
    for secret in BORROWER_SECRETS {
        assert!(
            !frame.contains(secret),
            "{secret} reached the lender inside a relayed frame: {frame}"
        );
    }
    for name in CREDENTIAL_HEADER_NAMES {
        assert!(
            !frame.contains(name),
            "the header NAME {name} reached the lender: {frame}"
        );
    }
}

/// The borrower's own credential-shaped secrets, named once because two tests
/// assert their absence and a second copy is a second place for one of them to
/// go missing.
///
/// Obviously fake, because this repository is public.
const BORROWER_SECRETS: [&str; 4] = [
    "not-a-real-token",
    "not-a-real-key",
    "not-a-real-proxy-key",
    "not-a-real-cookie",
];

/// The header names the borrower's scrub removes. Read from the same place the
/// secrets above are, for the same reason.
const CREDENTIAL_HEADER_NAMES: [&str; 4] = [
    "authorization",
    "x-api-key",
    "proxy-authorization",
    "cookie",
];

/// A borrower's client's headers with every credential shape in them, plus one
/// header that is NOT a credential so a test can tell "the scrub worked" from
/// "the frame is empty".
fn borrower_credentials() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer not-a-real-token"),
        ("x-api-key", "not-a-real-key"),
        ("proxy-authorization", "Bearer not-a-real-proxy-key"),
        ("cookie", "sessionKey=not-a-real-cookie"),
        ("content-type", "application/json"),
        // The one the API refuses a message request without, so a borrow that
        // forwards nothing is answered 400 rather than silently served.
        ("anthropic-version", "2023-06-01"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("a header value"),
        );
    }
    headers
}

/// **The lender forwards an ALLOWLIST of headers and drops everything else** :
/// the hole testing proved, driven by a borrower that is malicious
/// on purpose.
///
/// The lender replayed every borrower-supplied header onto the request it makes
/// through its own proxy. `build_upstream_headers` (`src/proxy.rs:3502`) then
/// removes `authorization`, `x-api-key`, `accept-encoding` and the hop-by-hop
/// names on the way upstream: and `cookie` is none of those, so a
/// borrower-chosen session cookie went to Anthropic on the lender's own TLS
/// session, as did any header a borrower invented.
///
/// The instrument is the LENDER's outbound request, recorded by the fake
/// upstream: not the frame the borrower built (the borrower here is hostile and
/// builds it by hand), and not the lender's own logs.
///
/// Watch it fail by restoring the replay loop in `serve_on_own_account`
/// (`for (name, value) in &request.headers`): `cookie`, `x-borrower-invented`
/// and the borrower's `user-agent`-shaped secrets all appear upstream. Measured
/// red that way before this test was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_forwards_only_the_allowlisted_headers() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = mesh::spawn_lender(
        pair.key_dir.clone(),
        pair.peers_path.clone(),
        pair.ledger.clone(),
        lender_proxy,
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    // Every name the brief names, plus one nobody thought of: which is the
    // whole argument for an allowlist over a denylist.
    let hostile = vec![
        (
            "authorization".to_string(),
            "Bearer not-a-real-token".to_string(),
        ),
        (
            "cookie".to_string(),
            "sessionKey=not-a-real-cookie".to_string(),
        ),
        ("x-api-key".to_string(), "not-a-real-key".to_string()),
        (
            "proxy-authorization".to_string(),
            "Bearer not-a-real-proxy-key".to_string(),
        ),
        (
            "host".to_string(),
            "not-a-real-host.example.com".to_string(),
        ),
        (
            "x-borrower-invented".to_string(),
            "not-a-real-smuggle".to_string(),
        ),
        // The positive control: one allowlisted header, so the absences below
        // are about the filter and not about a request that carried nothing.
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        serve::ServeRequest {
            lease_id: LEASE,
            request_id: 1,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            headers: hostile,
            body_bytes: 2,
            proto: tcr_peer_wire::PROTO_VERSION,
            flow: serve::SERVE_FLOW,
        },
    )
    .await;
    assert!(
        answered.is_ok(),
        "the lender serves this request; it is the HEADERS it must drop, not the request"
    );

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the request really was served, so the assertions below are about the filter"
    );
    let seen = served.lock().expect("served lock").clone();
    let sent = &seen[0];

    // The positive control first: if this is absent the request went out bare
    // and every absence below is free.
    assert_eq!(
        sent.header("content-type"),
        Some("application/json"),
        "an allowlisted header IS forwarded: {:?}",
        sent.headers
    );
    assert_eq!(
        sent.token,
        fleet::LENDER_TOKEN,
        "the Bearer upstream is the LENDER's, never the borrower's"
    );

    for dropped in [
        "cookie",
        "x-api-key",
        "proxy-authorization",
        "x-borrower-invented",
    ] {
        assert_eq!(
            sent.header(dropped),
            None,
            "{dropped} reached Anthropic on the lender's session: {:?}",
            sent.headers
        );
    }
    // `host` is set by the lender's own HTTP client for the host it dials, so
    // the claim is not that the name is absent: it is that the borrower's
    // VALUE is not what it says.
    assert_ne!(
        sent.header("host"),
        Some("not-a-real-host.example.com"),
        "the borrower chose the lender's Host header"
    );
    // And the strong form, on values rather than names: a secret that arrived
    // under a name nobody thought to check is invisible to every assertion
    // above.
    for secret in [
        "not-a-real-token",
        "not-a-real-cookie",
        "not-a-real-key",
        "not-a-real-proxy-key",
        "not-a-real-smuggle",
        "not-a-real-host.example.com",
    ] {
        assert!(
            !sent.values().iter().any(|value| value.contains(secret)),
            "{secret} reached Anthropic inside a header value: {:?}",
            sent.headers
        );
    }
}

/// **A non-POST relayed request is refused, never silently POSTed.**
///
/// `ServeRequest::method` was written as the literal `"POST"` by the borrower
/// and read by nobody, so a borrower whose client sent `GET /v1/messages` had
/// it POSTed on the lender's account: an answer to a question it never asked,
/// paid for out of a lease. The field is honoured, this build serves exactly
/// one method and refuses the rest rather than rewriting them.
///
/// Watch it fail by deleting the method check in `handle_serve_on`: the lender
/// then answers 200 and the fake upstream's hit count goes to 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_post_relayed_request_is_refused() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = mesh::spawn_lender(
        pair.key_dir.clone(),
        pair.peers_path.clone(),
        pair.ledger.clone(),
        lender_proxy,
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        serve::ServeRequest {
            lease_id: LEASE,
            request_id: 1,
            method: "GET".to_string(),
            path: "/v1/messages".to_string(),
            headers: Vec::new(),
            body_bytes: 2,
            proto: tcr_peer_wire::PROTO_VERSION,
            flow: serve::SERVE_FLOW,
        },
    )
    .await;

    assert!(
        answered.is_err(),
        "the lender closes on a method it does not serve rather than answering it"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing was sent upstream: a GET is refused, not turned into a POST"
    );
}

/// **The relayed frame the LENDER decrypted carries no credential of the
/// borrower's**: with the borrower's token in scope, which is what makes the
/// absence a measurement.
///
/// This is the assertion `a_borrowed_request_is_served_on_the_lenders_account`
/// used to make and could not: `open_serve` built its frame from
/// `&HeaderMap::new()` inline, so the scrub ran over an empty map, and the
/// bytes it asserted on were Noise ciphertext. Deleting
/// `scrub_client_credentials` from `serve_request_from` left it green.
///
/// Both halves are fixed here. `open_serve` takes the borrower's headers, this
/// test puts four credential-shaped secrets in them, and the lender captures
/// the frame it DECRYPTED: the one place a borrower's plaintext request
/// exists on the lender's side.
///
/// Watch it fail by deleting the `scrub_client_credentials(&mut scrubbed)` line
/// in `serve_request_from` (`src/peer/serve.rs`): all four secrets are then in
/// the captured frame. Measured red that way before this test was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_frame_the_lender_receives_holds_no_credential() {
    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.50, 300, 5)],
        )],
    );

    let captured: mesh::CapturedFrames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let peer_addr = mesh::spawn_capturing_lender(
        lender_home.path().to_path_buf(),
        lender_peers,
        captured.clone(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

    // The borrower's own client's headers, credentials included, which is the
    // case `crate::fallback`'s `Ask` carries none of TODAY and the case the
    // scrub exists for. The same map
    // `a_borrowed_request_is_served_on_the_lenders_account` uses, from one
    // place: two lists of credential shapes is two places for one of them to
    // go missing.
    let headers = borrower_credentials();

    let now = teamclaude_rs::now_ms();
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 5,
        until: None,
    };
    let ask = ask_for("/v1/messages");
    let response = serve::open_serve(&lender_id, &lease, &ask, &headers, &store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the capturing lender answered");
    assert_eq!(response.status().as_u16(), 200);

    let frames = captured.lock().expect("captured-frames lock").clone();
    assert_eq!(frames.len(), 1, "one request, one captured frame");
    let frame = String::from_utf8_lossy(&frames[0]).to_string();

    // The positive control on the instrument: this is the request's own frame,
    // so every absence below is about the scrub.
    assert!(
        frame.contains("/v1/messages"),
        "the captured frame is the request's own: {frame}"
    );
    assert!(
        frame.contains("content-type"),
        "a non-credential header survives, so the scrub removed credentials and not the \
         request: {frame}"
    );

    for secret in BORROWER_SECRETS {
        assert!(
            !frame.contains(secret),
            "{secret} reached the lender inside a relayed frame: {frame}"
        );
    }
    for name in CREDENTIAL_HEADER_NAMES {
        assert!(
            !frame.contains(name),
            "the header NAME {name} reached the lender: {frame}"
        );
    }
}

/// **A served request moves the ledger by the MEASURED utilization rise**, not
/// by `MIN_DEBIT`.
///
/// `handle_serve_on` debited through `Ledger::debit(.., 0.0)`: always
/// `MIN_DEBIT`, a constant whose own doc calls itself a guess, because the
/// delta needs the serving account's utilization before and after and the
/// signature held no way to read one. It reads the window through
/// `WindowUtilization` either side of the serve now, and
/// `serve::utilization_rise` turns the pair into the charge.
///
/// The reader is scripted rather than a real fleet: a real rise needs the fake
/// upstream's headers to move the lender's quota between two probes, which
/// tests `update_from_headers` rather than the debit. What is asserted is that
/// the figure the reader reported is the figure the lease was charged.
///
/// Watch it fail by restoring `ledger.debit(request.lease_id,
/// request.request_id, 0.0)`: `spent` is then MIN_DEBIT (0.002) against the
/// 0.07 asserted here. Measured red that way before this test was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_served_request_debits_the_measured_utilization_rise() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.50, 300, 5)],
        )],
    );

    let now = teamclaude_rs::now_ms();
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 5,
        until: None,
    };
    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        // `record_scoped` with the GRANTEE, not `record`: the review's M1 is
        // fixed, so a lease bound to nobody is a lease `enter_relay` refuses
        // (`RelayRefusal::NotTheGrantee`) and this test's borrow would be
        // refused before it reached the subject.
        held.record_scoped(lease, borrower_id, tcr_peer_wire::LendScope::All);
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }

    // Two accounts, and the SECOND is the one that moved: 0.10 -> 0.17. The
    // first is flat, so a reader that returned a fleet maximum instead of an
    // elementwise rise would answer 0.40 - 0.40 = 0.0 and charge MIN_DEBIT.
    let utilization = std::sync::Arc::new(mesh::ScriptedUtilization::new(vec![
        vec![Some(0.40), Some(0.10)],
        vec![Some(0.40), Some(0.17)],
    ]));
    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers,
        ledger.clone(),
        lender_proxy,
        utilization.clone(),
        fleet::dry_manager(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

    let ask = ask_for("/v1/messages");
    serve::open_serve(&lender_id, &lease, &ask, &HeaderMap::new(), &store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");

    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        utilization.reads_taken(),
        2,
        "the window is read once before the serve and once after; a reader nobody \
         called would satisfy the charge assertion by accident"
    );

    let held = ledger.lock().expect("ledger lock");
    let after = held
        .live(teamclaude_rs::now_ms())
        .first()
        .copied()
        .expect("the lease is still live");
    assert!(
        (after.spent - 0.07).abs() < 1e-9,
        "the lease is charged the 0.07 the lender measured, not MIN_DEBIT ({}); got {}",
        lease::MIN_DEBIT,
        after.spent
    );
}

/// The arithmetic behind the debit above, without a socket.
///
/// Four claims, and each one is a direction a scalar figure would get wrong.
#[test]
fn a_utilization_rise_is_the_largest_single_accounts_and_never_a_credit() {
    // The largest SINGLE account's rise, never the sum: one request is served
    // by one account.
    assert!(
        (serve::utilization_rise(&[Some(0.10), Some(0.20)], &[Some(0.13), Some(0.24)]) - 0.04)
            .abs()
            < 1e-9
    );

    // A window that reset under the request is the OWNER's windfall, never the
    // borrower's credit.
    assert_eq!(
        serve::utilization_rise(&[Some(0.90)], &[Some(0.01)]),
        0.0,
        "a reset is not a credit"
    );

    // An index unmeasured on either side contributes nothing: a rise is a
    // difference between two measurements and there is only one here.
    assert_eq!(serve::utilization_rise(&[None], &[Some(0.40)]), 0.0);
    assert_eq!(serve::utilization_rise(&[Some(0.40)], &[None]), 0.0);

    // A fleet that answered nothing at all: `NoFleetUtilization`, and a
    // lender whose accounts vector is empty.
    assert_eq!(serve::utilization_rise(&[], &[]), 0.0);

    // A fleet that gained an account between the two reads is read over the
    // pairs that exist, rather than panicking on the length mismatch.
    assert!(
        (serve::utilization_rise(&[Some(0.10)], &[Some(0.15), Some(0.99)]) - 0.05).abs() < 1e-9
    );
}

/// **An answer larger than one frame is CARRIED, and it is charged.**
///
/// This test used to assert the opposite, and the re-review named that as the
/// defect: the fix before this one made an over-cap answer a readable 502
/// instead of a dead stream, but it left `MAX_BODY_BYTES` (65 519 bytes) as the
/// ceiling on a whole answer. Claude Code always streams, so every borrowed
/// answer of any length was bought on the lender's account, debited, and handed
/// back to the borrower as an error with no content.
///
/// A body travels as a run of frames terminated by an empty one now, so the
/// size of an answer is no longer a thing the borrower can be refused over.
/// Both halves are measured: the client gets the WHOLE body, byte for byte, and
/// the lease is charged the rise the lender measured (0.07, from the scripted
/// reader).
///
/// Watch it fail by restoring the one-frame refusal in `serve_on_own_account`
/// (`if body.len() > MAX_BODY_BYTES { return Ok(undeliverable_answer(..)) }`):
/// the status is 502 and the body is the refusal's own JSON rather than the
/// answer's bytes.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_larger_than_one_frame_is_carried_and_charged() {
    // One byte over what a frame carries: the smallest reply that reaches the
    // branch, so the test cannot pass because the body was enormous.
    let (upstream, hits) = fleet::spawn_huge_upstream(serve::MAX_BODY_BYTES + 1).await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.50, 300, 5)],
        )],
    );

    let now = teamclaude_rs::now_ms();
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 5,
        until: None,
    };
    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        held.record_scoped(lease, borrower_id, tcr_peer_wire::LendScope::All);
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }
    let utilization = std::sync::Arc::new(mesh::ScriptedUtilization::new(vec![
        vec![Some(0.10)],
        vec![Some(0.17)],
    ]));
    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers,
        ledger.clone(),
        lender_proxy,
        utilization.clone(),
        fleet::dry_manager(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

    let ask = ask_for("/v1/messages");
    let answer = serve::open_serve(&lender_id, &lease, &ask, &client_headers(), &store)
        .await
        .expect("a reply over one frame must not kill the SERVE stream")
        .served()
        .expect("the borrower is answered rather than left to re-ask elsewhere");
    assert_eq!(
        answer.status().as_u16(),
        200,
        "an answer over one frame is carried to the client, not refused back to it"
    );
    let carried = axum::body::to_bytes(answer.into_body(), 64 * 1024 * 1024)
        .await
        .expect("the carried answer's body reads");
    assert_eq!(
        carried.len(),
        serve::MAX_BODY_BYTES + 1,
        "every byte of the answer arrives, across as many frames as it took"
    );
    assert!(
        carried.iter().all(|byte| *byte == b'x'),
        "the body is the origin's own bytes and not a refusal's JSON"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the lender's own account really did pay for this request"
    );

    let held = ledger.lock().expect("ledger lock");
    let spent = held
        .live(teamclaude_rs::now_ms())
        .first()
        .map(|lease| lease.spent)
        .expect("the lease is still live");
    assert!(
        (spent - 0.07).abs() < 1e-9,
        "the lease is charged the rise the lender measured, not nothing: {spent}"
    );
}

// ---------------------------------------------------------------------------
// The hostile borrower: a correctly pinned, correctly authenticated peer that
// hand-builds frames its own code would have refused to build. That is the case
// the lender's backstops exist for, and the only way to reach them: the
// borrower's own half refuses first, which is the point of it.
// ---------------------------------------------------------------------------

/// A pinned lender/borrower pair with the lender's peers file and ledger ready.
///
/// The temp dirs are carried in the struct because they must outlive the
/// listener: dropping one deletes the lender's node key under a live socket.
struct HostilePair {
    lender_key: teamclaude_rs::peer::id::NodeKey,
    borrower_key: teamclaude_rs::peer::id::NodeKey,
    key_dir: PathBuf,
    peers_path: PathBuf,
    ledger: std::sync::Arc<std::sync::Mutex<Ledger>>,
    _homes: (tempfile::TempDir, tempfile::TempDir),
}

fn hostile_pair() -> HostilePair {
    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let peers_path = lender_home.path().join("tcr-peers.json");
    let lender_key = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key");
    let borrower_key = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key");

    write_peers(
        &peers_path,
        vec![lender_row_for(
            borrower_key.id(),
            vec![LendGrant::new(Window::SevenDay, 0.50, 300, 5)],
        )],
    );

    let now = teamclaude_rs::now_ms();
    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        // `record_scoped` with the GRANTEE, not `record`: the review's M1 is
        // fixed, so a lease bound to nobody is a lease `enter_relay` refuses
        // (`RelayRefusal::NotTheGrantee`) and this test's borrow would be
        // refused before it reached the subject.
        held.record_scoped(
            Lease {
                lease_id: LEASE,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.50),
                granted_at_ms: now,
                expires_at_ms: now + 300_000,
                spent: 0.0,
                max_inflight: 5,
                until: None,
            },
            borrower_key.id(),
            tcr_peer_wire::LendScope::All,
        );
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }

    HostilePair {
        lender_key,
        borrower_key,
        key_dir: lender_home.path().to_path_buf(),
        peers_path,
        ledger,
        _homes: (lender_home, borrower_home),
    }
}

/// Drive one hand-built SERVE frame at a lender and read its answer.
///
/// `Err` is the lender closing the stream, which is what every refusal in
/// `handle_serve_on` does: there is no arm that explains itself to a borrower
/// whose frame should not have existed.
async fn hostile_serve(
    peer_addr: std::net::SocketAddr,
    lender: &teamclaude_rs::peer::id::NodeKey,
    borrower: &teamclaude_rs::peer::id::NodeKey,
    request: serve::ServeRequest,
) -> anyhow::Result<serve::ServeReply> {
    let mut stream = tokio::net::TcpStream::connect(peer_addr)
        .await
        .expect("the lender's listener answers");
    let mut session = teamclaude_rs::peer::noise::dial_handshake(
        &mut stream,
        borrower.secret_bytes(),
        teamclaude_rs::peer::noise::Handshake::Return,
        Some(&lender.id().0),
        None,
    )
    .await
    .expect("a pinned peer completes the handshake");

    let header = tcr_peer_wire::StreamHeader {
        kind: tcr_peer_wire::StreamKind::Serve,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: request.request_id,
    };
    serve::send_control(&mut stream, &mut session, &header)
        .await
        .expect("the header is written");
    serve::send_control(&mut stream, &mut session, &request)
        .await
        .expect("the frame is written");
    // THE ACK, read exactly where a borrower reads it. A refusal here is a
    // refusal before the body, so it is reported in the same word the caller
    // already reads; a lender that closed the stream instead answers `Err`,
    // which is what every `bail!` in `handle_serve_on` does.
    let ack: serve::ServeAck = serve::recv_control(&mut stream, &mut session).await?;
    if let serve::ServeAck::Refused { refusal } = ack {
        return Ok(serve::ServeReply::Refused { refusal });
    }

    // The body, and then the empty frame that ends it: a body is a run of
    // frames now, and a lender waits for the terminator before it serves.
    teamclaude_rs::peer::noise::send_encrypted(&mut stream, &mut session.transport, b"{}")
        .await
        .expect("the body is written");
    teamclaude_rs::peer::noise::send_encrypted(&mut stream, &mut session.transport, b"")
        .await
        .expect("the body terminator is written");

    serve::recv_control(&mut stream, &mut session).await
}

/// **A borrowed request pays the LENDER's per-organization GCRA bucket**: the
/// executable version of the claim `src/peer/serve.rs`'s module doc makes in
/// prose.
///
/// The lender's throttle is tightened to one instant slot then one per 350 ms,
/// and its fleet is one account in one organization, so three concurrent
/// BORROWED requests land in one bucket and the closed form is `(3-1)*350 =
/// 700 ms`. The control is the same three requests through a lender with the
/// throttle off.
///
/// Margins are deliberately wide (500 ms against a predicted 700, 300 ms
/// against a predicted ~0): this asserts that a borrowed request is paced at
/// all, not the tuning of the pacing.
///
/// Watch it fail by serving the relayed request anywhere other than through the
/// lender's own proxy: which is the only way to get a borrowed request past
/// `Manager::throttle_send` at all.
#[tokio::test(flavor = "multi_thread")]
async fn borrowed_requests_are_paced_by_the_lenders_own_bucket() {
    let paced = borrowed_burst(fleet::tight_throttle()).await;
    let loose = borrowed_burst(Default::default()).await;

    assert!(
        paced >= std::time::Duration::from_millis(500),
        "three borrowed requests on one org must be paced by that org's bucket, took {paced:?}"
    );
    assert!(
        loose < std::time::Duration::from_millis(300),
        "the control: with the throttle off the same three are not paced, took {loose:?}"
    );
}

/// Three concurrent borrowed requests against one lender, and how long the last
/// one took. The lender's fleet is one account in one organization, so every
/// one of the three lands in the same per-org bucket.
async fn borrowed_burst(throttle: teamclaude_rs::config::ThrottleConfig) -> std::time::Duration {
    let (upstream, _hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy = fleet::spawn_proxy(fleet::lending_manager(&upstream, throttle)).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.50, 300, 5)],
        )],
    );

    let now = teamclaude_rs::now_ms();
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 5,
        until: None,
    };
    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        // `record_scoped` with the GRANTEE, not `record`: the review's M1 is
        // fixed, so a lease bound to nobody is a lease `enter_relay` refuses
        // (`RelayRefusal::NotTheGrantee`) and this test's borrow would be
        // refused before it reached the subject.
        held.record_scoped(lease, borrower_id, tcr_peer_wire::LendScope::All);
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }

    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        ledger,
        lender_proxy,
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;
    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );

    // One warm request first, so the measured burst is not paying for whatever
    // the first request through a fresh proxy costs (a connect, a pin, a first
    // selection). The bucket is left to refill before the timed burst.
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    let warm = ask_for("/v1/messages");
    serve::open_serve(&lender_id, &lease, &warm, &HeaderMap::new(), &store)
        .await
        .expect("the warmup ran")
        .served()
        .expect("the warmup served");
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;

    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..3 {
        let peers = borrower_peers.clone();
        handles.push(tokio::spawn(async move {
            let store = PeerStore::open(&peers).expect("the borrower's peers file");
            let ask = ask_for("/v1/messages");
            serve::open_serve(&lender_id, &lease, &ask, &HeaderMap::new(), &store)
                .await
                .expect("the SERVE stream ran")
                .served()
                .expect("the lender served it");
        }));
    }
    for handle in handles {
        handle.await.expect("a borrowed request finished");
    }
    started.elapsed()
}

/// **The lender refuses a local-control path IN A FRAME**: hole C, on the one
/// route where its absence is reachable end to end.
///
/// A borrower's own refusal is the check that matters, and it runs before
/// anything is opened. This is the backstop against an older or a malicious
/// borrower, and here the borrower is malicious on purpose: it hand-builds a
/// frame naming `/_tcr/accounts`, which the lender would otherwise forward to
/// its OWN proxy on loopback: where that route ADDS A LIVE CREDENTIAL and its
/// entire authorization is that the caller reached loopback.
///
/// Measured on the fake upstream's hit count and on the stream: the lender
/// answers nothing and serves nothing.
///
/// Watch it fail by deleting the `LOCAL_PREFIX` arm of
/// `serve_is_allowed_for_path`: the frame is then forwarded and the lender's own
/// proxy answers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_refuses_a_local_control_path_in_a_frame() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = mesh::spawn_lender(
        pair.key_dir.clone(),
        pair.peers_path.clone(),
        pair.ledger.clone(),
        lender_proxy,
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    // The malicious borrower: a well-formed, correctly authenticated stream
    // carrying a path its own code would have refused to build.
    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        serve::ServeRequest {
            lease_id: LEASE,
            request_id: 1,
            method: "POST".to_string(),
            path: "/_tcr/accounts".to_string(),
            headers: Vec::new(),
            body_bytes: 2,
            proto: tcr_peer_wire::PROTO_VERSION,
            flow: serve::SERVE_FLOW,
        },
    )
    .await;

    assert!(
        answered.is_err(),
        "the lender closes on a forbidden path rather than answering it"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing was served: a relayed request for this Mac's own control route \
         never reaches its proxy"
    );
}

/// **A borrower speaking the older SERVE flow is refused BY NAME, and nothing
/// is served on its behalf.**
///
/// The ack is a change to the ORDER of frames on this stream: flow 1 wrote the
/// body straight after the request frame and waited for a reply, flow 2 waits
/// to be acknowledged first. Serving a flow-1 borrower on this build would mean
/// reading a body nobody acknowledged and leaving that borrower to report every
/// outcome as "may have been billed", which is the defect the ack exists to
/// fix, so the lender refuses rather than guesses.
///
/// The missing field reads 0 through `#[serde(default)]`, so a frame from a
/// build written before the field existed lands in the same refusal rather than
/// failing to parse.
///
/// Watch it fail by deleting the `request.flow != SERVE_FLOW` arm in
/// `handle_serve_on`: the lender acks, serves the request on its own account,
/// and the upstream's hit count goes to 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrower_on_the_older_serve_flow_is_refused_and_nothing_is_served() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = mesh::spawn_lender(
        pair.key_dir.clone(),
        pair.peers_path.clone(),
        pair.ledger.clone(),
        lender_proxy,
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    // Flow 1: the order this stream had before the ack. `0` is the same case,
    // it is what a frame with no `flow` field at all parses to.
    for flow in [0, 1] {
        let answered = hostile_serve(
            peer_addr,
            &pair.lender_key,
            &pair.borrower_key,
            serve::ServeRequest {
                lease_id: LEASE,
                request_id: u128::from(flow) + 1,
                method: "POST".to_string(),
                path: "/v1/messages".to_string(),
                headers: Vec::new(),
                body_bytes: 2,
                proto: tcr_peer_wire::PROTO_VERSION,
                flow,
            },
        )
        .await;
        assert!(
            answered.is_err(),
            "a borrower on SERVE flow {flow} is refused, not served on a guess about \
             which frame comes next"
        );
    }

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing reached the lender's own account: a flow this build does not speak is \
         refused before anything is sent upstream"
    );
}

/// **An answer that cannot be carried back says `x-should-retry: false`.**
///
/// The review's finding: `undeliverable_answer` is the one answer this build is
/// CERTAIN already ran on the lender's account and was debited, and it was the
/// one answer with no `x-should-retry` on it. Its siblings both set it
/// (`lease.rs`'s `delivered_unknown_response`, `egress.rs`'s of the same name)
/// and `src/proxy.rs` makes it the house rule for an answer that means "do not
/// send this again". Without it the SDK behind the borrower retries a request
/// that has already been paid for.
///
/// The branch is reached the way production reaches it: the lender's own
/// upstream answers a `content-length` it does not deliver and closes, so
/// reading the body fails AFTER the account has served the request. The
/// `upstream` here is that server directly rather than the lender's proxy,
/// because a proxy in between would answer its own error and the read would
/// succeed.
///
/// Watch it fail by removing the header pair from `undeliverable_answer`: the
/// status is still 502 and the assertion on the header goes red.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_cannot_be_carried_back_is_never_retryable() {
    // An upstream that promises 4 096 bytes, sends 8, and closes.
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the truncating upstream");
    let truncating = listening
        .local_addr()
        .expect("the truncating upstream's addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listening.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = vec![0_u8; 8192];
                let _ = stream.read(&mut scratch).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                          content-length: 4096\r\n\r\ntruncate",
                    )
                    .await;
                let _ = stream.flush().await;
            });
        }
    });

    let pair = hostile_pair();
    let peer_addr = mesh::spawn_lender(
        pair.key_dir.clone(),
        pair.peers_path.clone(),
        pair.ledger.clone(),
        format!("http://{truncating}"),
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        serve::ServeRequest {
            lease_id: LEASE,
            request_id: 1,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            headers: Vec::new(),
            body_bytes: 2,
            proto: tcr_peer_wire::PROTO_VERSION,
            flow: serve::SERVE_FLOW,
        },
    )
    .await
    .expect("an answer that cannot be read is still an answer, never a dead stream");

    let serve::ServeReply::Served {
        status, headers, ..
    } = answered
    else {
        panic!("the lender answers `Served` with its own 502, not a refusal");
    };
    assert_eq!(
        status, 502,
        "the request ran and its answer cannot be carried"
    );
    let retry = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-should-retry"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        retry,
        Some("false"),
        "this request was served and debited on the lender's account; a client that \
         retries it pays for it twice"
    );
}

// ---------------------------------------------------------------------------
// A lease has a SCOPE, and it never leaves the lender
// ---------------------------------------------------------------------------

/// **`--scope` round-trips through one parser**, and every label goes through
/// the shared sanitizer on the way in.
///
/// One parser for the CLI's input, its output and anything that reads a scope
/// back out of a file as text: two spellings of what `group:work` means is how
/// a lease comes to draw from somewhere the operator did not name.
///
/// The refusals are the interesting half. A bare `2` is not a scope, an
/// `account:` with nothing after it names no account, and a label that is an
/// email or a uuid is refused by `sanitize_label`: the same refusal the rest
/// of the peer surface gives, because a scope is written to a file and printed
/// by a CLI in a PUBLIC repository.
///
/// Watch it fail by making `LendScope::parse` accept a bare word as a group
/// name: `"2"` then parses and the first refusal below goes green.
#[test]
fn a_lend_scope_round_trips_and_refuses_what_is_not_a_scope() {
    use tcr_peer_wire::{LendScope, LendScopeRefusal};

    for (spec, expected) in [
        ("all", LendScope::All),
        ("ALL", LendScope::All),
        ("group:work", LendScope::Group("work".to_string())),
        (
            "account:studio-mac",
            LendScope::Accounts(vec!["studio-mac".to_string()]),
        ),
        (
            "accounts:studio-mac,attic-nuc",
            LendScope::Accounts(vec!["studio-mac".to_string(), "attic-nuc".to_string()]),
        ),
    ] {
        let parsed = LendScope::parse(spec).expect(spec);
        assert_eq!(parsed, expected, "{spec} parses to the scope it names");
        // And the spelling it prints parses back to the same scope, so a
        // printed scope can be pasted into the flag it came from.
        assert_eq!(
            LendScope::parse(&parsed.to_spec()).expect("the printed form parses back"),
            expected,
            "{spec} does not survive a round trip through {}",
            parsed.to_spec()
        );
    }

    assert!(matches!(
        LendScope::parse("2"),
        Err(LendScopeRefusal::Shape { .. })
    ));
    assert!(matches!(
        LendScope::parse("work"),
        Err(LendScopeRefusal::Shape { .. })
    ));
    assert_eq!(
        LendScope::parse("account:,,"),
        Err(LendScopeRefusal::NoAccounts)
    );
    // An email and a uuid are both refused, by the sanitizer rather than by a
    // second list here.
    assert!(matches!(
        LendScope::parse("account:alice@example.com"),
        Err(LendScopeRefusal::Label(_))
    ));
    assert!(matches!(
        LendScope::parse("group:11111111-1111-1111-1111-111111111111"),
        Err(LendScopeRefusal::Label(_))
    ));

    // The default is every account, which is what a lease meant before scopes
    // existed: so a grant written by an older build keeps behaving as it did.
    assert_eq!(LendScope::default(), LendScope::All);
}

/// **One answer to "is this account inside that scope"**, read by the headroom
/// arithmetic, by the picker restriction and by the `lentTo` line.
///
/// Three readers and one function on purpose: a scope that meant one set of
/// accounts when the headroom was computed and another when the request was
/// served would advertise one account's room and spend another's.
///
/// Watch it fail by making the `Group` arm compare against the label instead of
/// the group list: the second assertion then goes green for the wrong reason
/// and the third goes red.
#[test]
fn a_scope_covers_exactly_the_accounts_it_names() {
    use tcr_peer_wire::LendScope;

    let groups = vec!["work".to_string(), "spare".to_string()];
    assert!(lease::scope_covers(&LendScope::All, "studio-mac", &groups));
    assert!(lease::scope_covers(
        &LendScope::Group("work".to_string()),
        "studio-mac",
        &groups
    ));
    assert!(!lease::scope_covers(
        &LendScope::Group("studio-mac".to_string()),
        "studio-mac",
        &groups
    ));
    assert!(lease::scope_covers(
        &LendScope::Accounts(vec!["attic-nuc".to_string(), "studio-mac".to_string()]),
        "studio-mac",
        &groups
    ));
    assert!(!lease::scope_covers(
        &LendScope::Accounts(vec!["attic-nuc".to_string()]),
        "studio-mac",
        &groups
    ));
    // An account in NO group is covered by `All` and by its own name, and by
    // nothing else: which is what stops an ungrouped account leaking into a
    // group-scoped lease.
    assert!(lease::scope_covers(&LendScope::All, "loner", &[]));
    assert!(!lease::scope_covers(
        &LendScope::Group("work".to_string()),
        "loner",
        &[]
    ));
}

// ---------------------------------------------------------------------------
// A lease can be bounded in time
// ---------------------------------------------------------------------------

/// **A lease whose `until` has passed is refused**, with budget left on it and
/// its renewal deadline still ahead.
///
/// Both halves in one test, because the second is the control that stops the
/// first from passing for the wrong reason: the same lease with the end one
/// hour AHEAD is allowed, so the refusal is about `until` and not about an
/// expired ttl, a spent budget or an unmeasured window.
///
/// The word is `lease-expired` and not an `ended` of its own: adding a
/// `LeaseRefusal` variant is outside this test's scope on the wire crate, and
/// `LeaseExpired`'s own doc is "the lease's absolute deadline has passed",
/// which `until` is. The borrower's action is identical either way: stop
/// spending this lease.
///
/// Watch it fail by deleting the `lease.until` check from `Ledger::may_relay`.
#[test]
fn a_lease_whose_end_has_passed_is_refused_with_budget_left() {
    let now_ms = 1_700_000_000_000_i64;
    let now_s = 1_700_000_000_u64;

    let ended = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: now_ms - 60_000,
        // The RENEWAL deadline is still ahead: this lease is not expired.
        expires_at_ms: now_ms + 300_000,
        // And it is not spent.
        spent: 0.0,
        max_inflight: 2,
        until: Some(now_s - 1),
    };
    let mut ledger = Ledger::new();
    ledger.record(ended);
    ledger.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(
        ledger.may_relay(LEASE, now_ms),
        Err(LeaseRefusal::LeaseExpired),
        "at `until` the lender stops renewing, whatever is left on the budget"
    );

    // The control: the same lease, one hour of lending left.
    let mut running = Ledger::new();
    running.record(Lease {
        until: Some(now_s + 3_600),
        ..ended
    });
    running.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(
        running.may_relay(LEASE, now_ms),
        Ok(()),
        "a lease still inside its end serves, so the refusal above is about the end"
    );

    // And a lease with NO end serves, which is the default an operator gets by
    // not asking for one.
    let mut endless = Ledger::new();
    endless.record(Lease {
        until: None,
        ..ended
    });
    endless.note_owner_headroom(Window::SevenDay, 0.30);
    assert_eq!(endless.may_relay(LEASE, now_ms), Ok(()));
}

/// **A grant whose end has already passed mints nothing**, and one still ahead
/// puts its end on the lease.
///
/// Refusing to mint is the same answer `may_relay` would give on the first
/// relay, told one round trip earlier: the same reasoning `clamp_to_grant`
/// already applies to a window with no measurement.
///
/// Watch it fail by deleting the `end.is_some_and(..)` check from
/// `clamp_to_grant`: the first assertion then gets a lease.
#[test]
fn a_grant_past_its_end_mints_nothing_and_one_ahead_carries_it() {
    let now_ms = 1_700_000_000_000_i64;
    let now_s = 1_700_000_000_u64;
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let granted = LendGrant::new(Window::SevenDay, 0.20, 300, 2);

    let past = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.40,
        now_ms,
        LEASE,
        Some(now_s - 1),
    );
    assert_eq!(past.lease, None);
    assert_eq!(past.refusal, Some(LeaseRefusal::LeaseExpired));

    let ahead = lease::clamp_to_grant(
        &ask,
        Some(granted.clone()),
        true,
        0.40,
        now_ms,
        LEASE,
        Some(now_s + 7_200),
    );
    assert_eq!(
        ahead
            .lease
            .expect("a lease inside its end is granted")
            .until,
        Some(now_s + 7_200),
        "the lease carries the lender's end, which is the one time field a borrower may know"
    );

    // No end asked for, no end on the lease.
    let endless =
        lease::clamp_to_grant(&ask, Some(granted.clone()), true, 0.40, now_ms, LEASE, None);
    assert_eq!(
        endless.lease.expect("a lease with no end is granted").until,
        None
    );
}

/// **`--for 2h` writes an end two hours ahead, and `--until 18:00` writes
/// today's 18:00: or tomorrow's, when 18:00 has gone.**
///
/// The clock is injected, which is what makes this a measurement rather than an
/// approximation: a function reading the wall clock could only be asserted
/// about to within the time the assertion took.
///
/// The roll to tomorrow is the decision worth a test of its own. An operator
/// who types `--until 09:00` at 18:00 means tomorrow morning; answering "that
/// is in the past" would be technically true and useless.
///
/// A bare number is REFUSED rather than read as seconds or as an hour: `2` is
/// two hours to one reader and two seconds to another, and a lease is not
/// something to guess a unit on.
///
/// Watch it fail by making the duration arm default to seconds for a bare
/// number: `"2"` then parses and the last assertion goes green.
#[test]
fn a_lend_end_is_parsed_from_a_duration_or_a_clock_against_an_injected_now() {
    // A fixed instant with a known wall-clock time, in UTC so the assertion
    // does not depend on the machine's zone: 2023-11-14 22:13:20 UTC.
    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
        .expect("a literal unix timestamp is a valid instant");

    assert_eq!(
        lease::parse_lend_end("2h", now).expect("2h parses"),
        Some(1_700_000_000 + 7_200)
    );
    assert_eq!(
        lease::parse_lend_end("90m", now).expect("90m parses"),
        Some(1_700_000_000 + 5_400)
    );
    assert_eq!(
        lease::parse_lend_end("30s", now).expect("30s parses"),
        Some(1_700_000_000 + 30)
    );
    assert_eq!(
        lease::parse_lend_end("3d", now).expect("3d parses"),
        Some(1_700_000_000 + 3 * 86_400)
    );

    // No end, named rather than implied, so an operator can clear an end with
    // the same flag they set it with.
    assert_eq!(
        lease::parse_lend_end("none", now).expect("none parses"),
        None
    );
    assert_eq!(lease::parse_lend_end("", now).expect("empty parses"), None);

    // A clock time LATER today: 23:00 against a 22:13:20 now.
    let today = lease::parse_lend_end("23:00", now)
        .expect("23:00 parses")
        .expect("a clock time is an end");
    let today = time::OffsetDateTime::from_unix_timestamp(i64::try_from(today).expect("in range"))
        .expect("a valid instant");
    assert_eq!((today.hour(), today.minute()), (23, 0));
    assert_eq!(today.date(), now.date(), "later today is today");

    // And one that has GONE today rolls to tomorrow.
    let tomorrow = lease::parse_lend_end("09:00", now)
        .expect("09:00 parses")
        .expect("a clock time is an end");
    let tomorrow =
        time::OffsetDateTime::from_unix_timestamp(i64::try_from(tomorrow).expect("in range"))
            .expect("a valid instant");
    assert_eq!((tomorrow.hour(), tomorrow.minute()), (9, 0));
    assert_eq!(
        tomorrow.date(),
        now.date().next_day().expect("there is a tomorrow"),
        "09:00 has gone today, so the operator means tomorrow morning"
    );

    // Refusals: a bare number, a unit this build does not know, a duration
    // that is not into the future.
    for refused in ["2", "2w", "0h", "-1h", "25:00", "18:60", "18:00:00:00"] {
        assert!(
            lease::parse_lend_end(refused, now).is_err(),
            "{refused:?} is not an end and must be refused rather than guessed at"
        );
    }
}

/// **`lentTo`, per account label, from the lender's own grants and no wire** :
/// the read-only, time-bounded view of a lease from the account's side.
///
/// Two Macs with a grant on one account is the shape the panel draws as "Lent
/// to attic-nuc 20 % · studio-mac 20 %", and it is the one this asserts. An
/// account no grant reaches is ABSENT from the map rather than present with an
/// empty list, because the panel hides the line when it is empty and "no key"
/// is one fact for it to read rather than two.
///
/// Nothing is asked of a peer: this reads the lender's own file. A figure that
/// needed a reachable peer would render blank on a sleeping laptop, which is
/// every laptop most of the time.
///
/// Watch it fail by making `lent_to` skip the scope check (`continue` on every
/// account): the ungrouped account then appears too.
#[test]
fn lent_to_names_every_mac_a_grant_lends_one_account_to() {
    let dir = tempfile::tempdir().expect("a temp home");
    let peers = dir.path().join("tcr-peers.json");

    let grant = |fraction: f64| LendGrant::new(Window::SevenDay, fraction, 300, 2);
    // Two trusted Macs, each holding a grant on this node.
    let mut attic = lender_row_for(PeerId([11_u8; 32]), vec![grant(0.20)]);
    attic.label = "attic-nuc".to_string();
    let mut studio = lender_row_for(PeerId([12_u8; 32]), vec![grant(0.35)]);
    studio.label = "studio-mac".to_string();
    // And one pinned Mac that was never lent anything.
    let mut idle = lender_row_for(PeerId([13_u8; 32]), Vec::new());
    idle.label = "idle-mac".to_string();
    write_peers(&peers, vec![attic, studio, idle]);

    let store = PeerStore::open(&peers).expect("the peers file");
    let accounts = vec![
        ("lender-fake".to_string(), vec!["work".to_string()]),
        ("spare-fake".to_string(), Vec::new()),
    ];
    let lent = lease::lent_to(&store, &accounts);

    let on_first = lent
        .get("lender-fake")
        .expect("the account every grant covers has a line");
    let peers_named: Vec<&str> = on_first.iter().map(|row| row.peer.as_str()).collect();
    assert_eq!(
        peers_named,
        ["attic-nuc", "studio-mac"],
        "both Macs holding a grant appear on the account's line: {on_first:?}"
    );
    assert_eq!(on_first[0].window, Window::SevenDay);
    assert!((on_first[0].fraction - 0.20).abs() < f64::EPSILON);
    assert!((on_first[1].fraction - 0.35).abs() < f64::EPSILON);
    assert_eq!(
        on_first[0].scope,
        tcr_peer_wire::LendScope::All,
        "every grant is `All` until `LendGrant` carries a scope"
    );
    assert_eq!(on_first[0].until, None);

    // The JSON the panel actually reads: camelCase, and `until` absent rather
    // than null when the operator set no end.
    let json = serde_json::to_string(&lent).expect("the lentTo map serializes");
    assert!(json.contains("\"peer\":\"attic-nuc\""), "{json}");
    assert!(json.contains("\"fraction\":0.2"), "{json}");
    assert!(
        !json.contains("until"),
        "an absent end is an absent key, never a null: {json}"
    );

    // A pinned Mac with no grant is on nobody's line, and a `Group`-scoped
    // grant reaching an ungrouped account would be the other failure: both
    // are absences, so the positive control above is what makes them mean
    // something.
    assert!(
        !lent.values().flatten().any(|row| row.peer == "idle-mac"),
        "a pinned Mac that was lent nothing is on no account's line: {lent:?}"
    );
}

// ---------------------------------------------------------------------------
// Lease scope, time-bounded lending,
// and the storage both of them need.
// ---------------------------------------------------------------------------

/// Write a peers file BY HAND, at mode 0600, so a test can assert what a shape
/// this program did not produce reads as.
///
/// `config::save` cannot express any of the cases below: a pre-decision-12
/// single grant object, or a hand-planted `"ended": true`, because it
/// serializes the current struct. A fixture that went through the writer would
/// only ever prove the writer agrees with itself.
fn write_raw_peers(path: &Path, json: &str) {
    std::fs::write(path, json).expect("the fixture writes");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("0600, which is what `read_or_default` insists on");
}

/// **A peers file written before scope existed (one grant OBJECT, not a list)
/// reads as a one-element list**, with today's defaults for everything it
/// could not carry.
///
/// An operator's grant is not something to lose to a shape change, and the
/// alternative is worse than losing it: a parse error on the `lend` key takes
/// the whole peers file down, which means the pinned rows, the allows and the
/// network key go with it.
///
/// Watch it fail by putting `#[serde(default)]` back on `PeerRow::lend` in
/// place of `deserialize_with = "one_or_many_grants"`: the read then fails with
/// "invalid type: map, expected a sequence".
#[test]
fn an_old_single_grant_object_reads_as_a_one_element_list() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let node = PeerId([31_u8; 32]).to_wire();
    write_raw_peers(
        &peers,
        &format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "attic-nuc",
              "addedAt": 1,
              "allow": {{ "inspect": true }},
              "lend": {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }}
            }}
          ]
        }}"#
        ),
    );

    let file = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect("the pre-decision-12 shape still reads");
    let row = &file.peers[0];
    assert_eq!(row.lend.len(), 1, "one grant object is one lease: {row:?}");
    let grant = &row.lend[0];
    assert_eq!(grant.window, Window::SevenDay);
    assert!((grant.fraction - 0.20).abs() < f64::EPSILON);
    assert_eq!(
        grant.scope,
        tcr_peer_wire::LendScope::All,
        "a grant written before scopes existed meant every account, and still does"
    );
    assert_eq!(grant.until, None, "and it had no end");
    assert!(!grant.ended);
    assert_eq!(grant.id, 0, "no id until the file is next written");

    // The positive control: the modern LIST shape reads too, so the reader
    // above is not simply accepting everything as one grant.
    let modern = dir.path().join("modern.json");
    write_raw_peers(
        &modern,
        &format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "attic-nuc",
              "addedAt": 1,
              "lend": [
                {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }},
                {{ "window": "5h", "fraction": 0.1, "ttlS": 300, "maxInflight": 1 }}
              ]
            }}
          ]
        }}"#
        ),
    );
    let file = teamclaude_rs::peer::config::read_or_default(&modern).expect("the list shape reads");
    assert_eq!(file.peers[0].lend.len(), 2);
}

/// **`ended` is derived against the clock and NEVER read off the file.**
///
/// Two grants in one hand-written file: one whose end has passed and which the
/// file claims is still running, and one with no end at all which the file
/// claims has ended. Both claims are ignored.
///
/// The direction that matters is the second: a `"ended": true` a text editor
/// could plant on a live grant would switch lending off without any of the
/// verbs that are supposed to do that, and nothing would say why.
///
/// Two layers, and the test names both. The DERIVATION (`read_or_default`,
/// `PeerStore::peers`, `PeerStore::row`) is what every caller sees; the
/// `skip_deserializing` on the field is the layer under it, for a reader
/// written later that forgets to derive: so it is asserted through a raw
/// `serde_json` read, the one path in this program that does not derive.
///
/// Watch it fail two ways: delete the derivation loop in `read_or_default` (the
/// first assertion goes green-for-the-wrong-reason and the store assertions
/// fail), or remove `skip_deserializing` from `LendGrant::ended` (the raw-read
/// assertion at the bottom fails).
#[test]
fn an_ended_flag_in_the_file_is_ignored_and_the_clock_decides() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let node = PeerId([31_u8; 32]).to_wire();
    write_raw_peers(
        &peers,
        &format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "attic-nuc",
              "addedAt": 1,
              "lend": [
                {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2,
                  "until": 1, "ended": false }},
                {{ "window": "5h", "fraction": 0.1, "ttlS": 300, "maxInflight": 1,
                  "ended": true }}
              ]
            }}
          ]
        }}"#
        ),
    );

    let file = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    let lend = &file.peers[0].lend;
    assert!(
        lend[0].ended,
        "unix second 1 is long past, so this lease has ended whatever the file says"
    );
    assert!(
        !lend[1].ended,
        "a grant with no end has not ended, and a hand-planted `ended: true` does not \
         make it so"
    );

    // And through the STORE, which is what the serving path reads.
    let store = PeerStore::open(&peers).expect("the store opens");
    let row = store.peers().remove(0);
    assert!(row.lend[0].ended);
    assert!(!row.lend[1].ended);
    // The one reader the picker uses skips the ended grant and finds the live
    // one on its own window.
    let now_s = u64::try_from(teamclaude_rs::now_ms() / 1_000).expect("a positive clock");
    // Every scope in this fixture is `All`, which every fleet can hold, so the
    // enforceability predicate answers `true` and the ENDED grant is the only
    // thing being skipped here.
    let any_scope = |_: &tcr_peer_wire::LendScope| true;
    assert!(row.grant_for(Window::SevenDay, now_s, &any_scope).is_none());
    assert!(row.grant_for(Window::FiveHour, now_s, &any_scope).is_some());

    // The layer UNDER the derivation: a raw deserialize, which no production
    // caller does, still cannot pick up the file's `"ended": true`.
    let raw: PeerFile = serde_json::from_str(&std::fs::read_to_string(&peers).expect("the bytes"))
        .expect("the fixture deserializes");
    assert!(
        !raw.peers[0].lend[1].ended,
        "`ended` is derived, so a file claiming true cannot deserialize into true"
    );
}

/// **`Ledger::grant` carries the operator's `until` onto the lease, refuses a
/// grant whose end has passed, and `may_relay` refuses it afterwards** :
/// time-bounded lending, end to end, against a real peers file.
///
/// `clamp_to_grant`'s own gates already cover the arithmetic with an injected
/// end. What this adds is the wiring those gates cannot see: that the end
/// travels from the FILE to the lease, which is the half that was passing
/// `None` before this fix.
///
/// Watch it fail by passing `None` for `end` in `Ledger::grant` again: the
/// first assertion then gets a lease with no end and the third gets one at
/// all.
#[test]
fn a_grant_with_an_end_puts_it_on_the_lease_and_a_passed_end_mints_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([21_u8; 32]);
    let now_s = u64::try_from(teamclaude_rs::now_ms() / 1_000).expect("a positive clock");

    let mut ahead = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    ahead.until = Some(now_s + 7_200);
    write_peers(&peers, vec![lender_row_for(borrower, vec![ahead])]);
    let store = PeerStore::open(&peers).expect("the peers file opens");

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let lease = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("a grant inside its end mints a lease");
    assert_eq!(
        lease.until,
        Some(now_s + 7_200),
        "the end came off the operator's own grant, not from a parameter nobody sets"
    );

    // Past the end, that same lease is refused: and it is refused for the
    // END and not for the ttl, which is still 300 seconds away.
    assert_eq!(
        ledger.may_relay(
            lease.lease_id,
            (i64::try_from(now_s).expect("in range") + 7_201) * 1_000
        ),
        Err(LeaseRefusal::LeaseExpired),
        "at `until` the lender stops renewing"
    );

    // A grant whose end has ALREADY passed mints nothing at all. Same file,
    // same ledger, one field different.
    let mut passed = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    passed.until = Some(now_s - 1);
    write_peers(&peers, vec![lender_row_for(borrower, vec![passed])]);
    let store = PeerStore::open(&peers).expect("the peers file re-opens");
    let refused = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    assert_eq!(refused.lease, None);
    assert_eq!(
        refused.refusal,
        Some(LeaseRefusal::InspectNotGranted),
        "an ended grant is not a grant for this window at all: `grant_for` skips it, so the \
         answer is the same one an unpinned peer gets rather than a lease nobody can spend"
    );
}

/// Decision row 14, the half that has to be enforced rather than stored:
/// `Ledger::grant` refuses a lease asked for OUTSIDE the grant's window, and
/// mints the same lease inside it.
///
/// # Why the window is built around now instead of being written down
///
/// The refusal reads the wall clock, `Schedule::contains` asks what time it is
/// on this Mac, because that is the question an operator means by "between
/// 22:00 and 08:00". A test cannot hand `Ledger::grant` a pretend instant, so
/// it does the opposite: it builds one window that is open at whatever time the
/// suite happens to run and one that is not, three hours away on either side,
/// and the assertion is the same at any hour and in any timezone. The literal
/// 21:59-vs-22:01 boundary the decision names is pinned where an instant CAN be
/// handed over, against `schedule_refusal` itself, in `tests/peer_props.rs`.
///
/// Watch it fail by deleting the `schedule_refusal` call from `Ledger::grant`:
/// the closed window then mints a lease, which is the defect this exists to
/// catch: the field was stored, printed, and enforced nowhere.
#[test]
fn a_grant_outside_its_window_is_refused_and_inside_it_mints() {
    use teamclaude_rs::peer::schedule::Between;

    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([23_u8; 32]);

    // This Mac's own wall clock, read the way the refusal reads it.
    let now_utc = time::OffsetDateTime::now_utc();
    let offset = time::UtcOffset::local_offset_at(now_utc).unwrap_or(time::UtcOffset::UTC);
    let local = now_utc.to_offset(offset);
    let hour =
        |shift: i8| -> u8 { ((i16::from(local.hour()) + i16::from(shift)).rem_euclid(24)) as u8 };
    let open = format!(
        "{:02}:{:02}-{:02}:{:02}",
        hour(-3),
        local.minute(),
        hour(3),
        local.minute()
    );
    let shut = format!(
        "{:02}:{:02}-{:02}:{:02}",
        hour(3),
        local.minute(),
        hour(6),
        local.minute()
    );

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);

    let mut inside = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    inside.between = Some(open.parse::<Between>().expect("the open window parses"));
    write_peers(&peers, vec![lender_row_for(borrower, vec![inside])]);
    let store = PeerStore::open(&peers).expect("the peers file opens");
    assert!(
        ledger
            .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
            .answer
            .lease
            .is_some(),
        "a grant whose window is open right now ({open}) must still mint"
    );

    let mut outside = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    outside.between = Some(shut.parse::<Between>().expect("the shut window parses"));
    write_peers(&peers, vec![lender_row_for(borrower, vec![outside])]);
    let store = PeerStore::open(&peers).expect("the peers file re-opens");
    let refused = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    assert_eq!(refused.lease, None, "a window that is shut mints nothing");
    assert_eq!(
        refused.refusal,
        Some(LeaseRefusal::OutsideSchedule),
        "and it says WHICH refusal: a borrower told `InspectNotGranted` would go and ask \
         its operator for a grant it already has"
    );
}

/// **A lease's scope is recorded on the LENDER and never on the wire.**
///
/// The wire `Lease` carries window, unit, amount, ttl and a lease
/// id, nothing that names an account. So the ledger knows what the lease draws
/// from and the message does not.
///
/// The second half is asserted on the serialized message rather than by
/// reading the struct: a field added to `Lease` would be invisible to a
/// field-by-field check that only looks at the fields it knows.
///
/// Watch it fail by making `Ledger::grant` record with `LendScope::All`: the
/// first assertion then reads `all`.
#[test]
fn a_lease_scope_stays_on_the_lender_and_never_reaches_the_wire() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([22_u8; 32]);

    let mut scoped = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    scoped.scope = tcr_peer_wire::LendScope::Group("work".to_string());
    write_peers(&peers, vec![lender_row_for(borrower, vec![scoped])]);
    let store = PeerStore::open(&peers).expect("the peers file opens");

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    // A fleet that can hold every scope: this test is about what the ledger
    // RECORDS beside a lease, not about which scopes are enforceable, and
    // `NoFleetUtilization` would skip the grant before there was a lease to ask
    // about.
    let lease = ledger
        .grant(&borrower, &ask, &store, &AnyScope)
        .answer
        .lease
        .expect("a scoped grant mints a lease like any other");

    assert_eq!(
        ledger.scope_of(lease.lease_id),
        tcr_peer_wire::LendScope::Group("work".to_string()),
        "the lender knows which accounts this lease may draw from"
    );
    // A lease id the ledger never heard of is `All`, which is what every lease
    // minted before scope existed meant.
    assert_eq!(ledger.scope_of(0xdead_beef), tcr_peer_wire::LendScope::All);

    let wire = serde_json::to_string(&lease).expect("a lease serializes");
    for named in ["work", "scope", "group", "account"] {
        assert!(
            !wire.contains(named),
            "the wire lease must not carry {named:?}: {wire}"
        );
    }
}

/// **`lentTo` carries the real scope, the real end, and whether that end has
/// passed**: the account-side view, with the lease's scope in it.
///
/// The `All` case is the sibling test above
/// (`lent_to_names_every_mac_a_grant_lends_one_account_to`). This one is the
/// half that was hardcoded before this fix: a `group:work` lease reaches the
/// grouped account and NOT the ungrouped one, an ended lease is still a row
/// (the ended lease stays, greyed, so it can be re-lent), and the lease id is
/// on the line so the panel's click target can revoke exactly what it shows.
///
/// Watch it fail by reading `LendScope::All` for every grant again, the way
/// `lent_to` did before this fix: `spare-fake` then gets a line too.
#[test]
fn lent_to_carries_the_real_scope_the_real_end_and_whether_it_has_passed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let now_s = u64::try_from(teamclaude_rs::now_ms() / 1_000).expect("a positive clock");

    let mut live = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    live.scope = tcr_peer_wire::LendScope::Group("work".to_string());
    live.until = Some(now_s + 3_600);
    let mut over = LendGrant::new(Window::FiveHour, 0.10, 300, 1);
    over.scope = tcr_peer_wire::LendScope::Accounts(vec!["lender-fake".to_string()]);
    over.until = Some(now_s - 60);

    let mut attic = lender_row_for(PeerId([23_u8; 32]), vec![live, over]);
    attic.label = "attic-nuc".to_string();
    write_peers(&peers, vec![attic]);
    let store = PeerStore::open(&peers).expect("the peers file opens");

    let accounts = vec![
        ("lender-fake".to_string(), vec!["work".to_string()]),
        ("spare-fake".to_string(), Vec::new()),
    ];
    let lent = lease::lent_to(&store, &accounts);

    assert!(
        !lent.contains_key("spare-fake"),
        "neither scope names the ungrouped account, so it is absent from the map: {lent:?}"
    );
    let on_lender = lent
        .get("lender-fake")
        .expect("both scopes cover the grouped account");
    assert_eq!(on_lender.len(), 2, "two leases, two rows: {on_lender:?}");

    assert_eq!(
        on_lender[0].scope,
        tcr_peer_wire::LendScope::Group("work".to_string())
    );
    assert_eq!(on_lender[0].until, Some(now_s + 3_600));
    assert!(!on_lender[0].ended, "an hour ahead has not passed");
    assert_eq!(
        on_lender[1].scope,
        tcr_peer_wire::LendScope::Accounts(vec!["lender-fake".to_string()])
    );
    assert!(
        on_lender[1].ended,
        "a minute ago has passed, and the row is KEPT so it can be re-lent"
    );

    // The lease ids are on the line, distinct, and hex rather than a JSON
    // number no double can hold.
    assert_ne!(on_lender[0].id, on_lender[1].id);
    assert_eq!(
        on_lender[0].id.len(),
        32,
        "32 hex characters: {:?}",
        on_lender[0].id
    );
    let json = serde_json::to_string(&lent).expect("the lentTo map serializes");
    assert!(
        json.contains(&format!("\"id\":\"{}\"", on_lender[0].id)),
        "the id is a string in the JSON a panel parses: {json}"
    );
    assert!(json.contains("\"ended\":true"), "{json}");
}

// ---------------------------------------------------------------------------
// The CLI, through the binary this build produced. `--peers` points the whole
// peer surface at a temp file, so nothing here reads the operator's config directory or touches
// the proxy on 127.0.0.1:3456.
// ---------------------------------------------------------------------------

/// Run `tcr peer <args> --peers <path>` and return `(stdout, stderr, success)`.
fn run_tcr_peer(peers: &Path, args: &[&str]) -> (String, String, bool) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .arg("peer")
        .args(args)
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .output()
        .unwrap_or_else(|err| panic!("spawn tcr peer {args:?}: {err}"));
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// A peers file with one pinned Mac and nothing lent, plus that peer's id in
/// the wire form `tcr peer lend` takes.
fn cli_peers_file(dir: &Path) -> (PathBuf, String) {
    let peers = dir.join("tcr-peers.json");
    let node = PeerId([31_u8; 32]);
    let mut row = lender_row_for(node, Vec::new());
    row.label = "attic-nuc".to_string();
    write_peers(&peers, vec![row]);
    (peers, node.to_wire())
}

/// **`tcr peer lend --scope group:work` adds a SECOND lease rather than
/// replacing the first**, and prints the id that names it.
///
/// A trusted Mac may hold several leases at once, one per scope.
/// So the replace key is (window, scope) and not the window alone: the failure
/// this guards is an operator who lends `group:work` and silently loses the
/// `all` lease they were already lending on the same window.
///
/// Watch it fail by retaining on `existing.window != window` alone in the
/// `Lend` arm of `run_peer`: the file then holds one lease instead of two.
#[test]
fn lend_with_a_scope_adds_a_second_lease_on_the_same_window() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &["lend", &node, "--window", "7d", "--fraction", "0.2"],
    );
    assert!(ok, "plain lend: {out}{err}");
    assert!(out.contains("scope=all"), "{out}");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.3",
            "--scope",
            "group:work",
        ],
    );
    assert!(ok, "scoped lend: {out}{err}");
    assert!(out.contains("scope=group:work"), "{out}");
    assert!(out.contains("leases=2"), "{out}");

    let file = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    let lend = &file.peers[0].lend;
    assert_eq!(lend.len(), 2, "one lease per scope: {lend:?}");
    assert_eq!(lend[0].scope, tcr_peer_wire::LendScope::All);
    assert_eq!(
        lend[1].scope,
        tcr_peer_wire::LendScope::Group("work".to_string())
    );
    assert_ne!(lend[0].id, lend[1].id, "two leases, two handles");

    // And a scope that is not a scope changes nothing at all: the refusal
    // comes before the write.
    let (_, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--scope", "7"]);
    assert!(!ok, "a bare number is not a scope");
    assert!(err.contains("scope"), "{err}");
    let after = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(after.peers[0].lend.len(), 2, "a refused lend wrote nothing");
}

/// **`--list` prints one greppable line per lease with its id, and `--revoke
/// <lease-id>` removes exactly that one.**
///
/// The id is the whole point: an operator with two leases on one window has no
/// other way to name the one they want gone, and "the 7d one" is not a
/// selector.
///
/// Watch it fail by making `--revoke` clear `row.lend` instead of retaining on
/// the id: the surviving-lease assertion then finds nothing.
#[test]
fn lend_list_prints_ids_and_revoke_removes_exactly_one_lease() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    run_tcr_peer(
        &peers,
        &["lend", &node, "--window", "7d", "--fraction", "0.2"],
    );
    run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "5h",
            "--fraction",
            "0.1",
            "--scope",
            "group:work",
        ],
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--list"]);
    assert!(ok, "list: {out}{err}");
    let lines: Vec<&str> = out.lines().filter(|line| line.contains("lease=")).collect();
    assert_eq!(lines.len(), 2, "two leases, two lines: {out}");
    assert!(lines[0].contains("window=7d"), "{out}");
    assert!(lines[1].contains("scope=group:work"), "{out}");
    assert!(lines[1].contains("until=none"), "{out}");

    let file = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    let doomed = teamclaude_rs::peer::config::lease_id_string(file.peers[0].lend[0].id);
    let kept = file.peers[0].lend[1].id;
    assert!(
        out.contains(&doomed),
        "the printed line carries the id: {out}"
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--revoke", &doomed]);
    assert!(ok, "revoke: {out}{err}");
    assert!(out.contains("leases_left=1"), "{out}");
    let file = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(file.peers[0].lend.len(), 1);
    assert_eq!(
        file.peers[0].lend[0].id, kept,
        "the lease that survived is the one that was not named"
    );

    // An id nothing holds is a refusal that names it, never a silent no-op.
    let (_, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--revoke", &doomed]);
    assert!(!ok, "revoking a lease that is already gone is an error");
    assert!(err.contains(&doomed), "{err}");
}

/// **`--for 2h` writes an end, `--list` shows it, and `--relend` clears it.**
///
/// Through the surface an operator actually types, the end is
/// absolute unix seconds on disk, the lease is ended once it passes, and
/// re-lending is one command rather than deleting and re-creating the lease
/// (which would change the id every surface is holding).
///
/// Watch it fail by dropping `grant.until = end` from the `Lend` arm: the file
/// then has no end and the `ended` assertion below never fires.
#[test]
fn lend_for_a_duration_writes_an_end_and_relend_clears_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let before = u64::try_from(teamclaude_rs::now_ms() / 1_000).expect("a positive clock");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--for",
            "2h",
        ],
    );
    assert!(ok, "lend --for: {out}{err}");
    let file = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    let grant = &file.peers[0].lend[0];
    let until = grant.until.expect("`--for 2h` wrote an end");
    assert!(
        until >= before + 7_200 && until <= before + 7_260,
        "two hours ahead of now, in absolute unix seconds: {until} vs {before}"
    );
    assert!(!grant.ended, "two hours ahead has not passed");
    let id = teamclaude_rs::peer::config::lease_id_string(grant.id);

    // An end in the past reads as ended, through the same CLI.
    let (out, _, ok) = run_tcr_peer(
        &peers,
        &["lend", &node, "--relend", &id, "--until", "00:00:01"],
    );
    assert!(ok, "relend to a clock time: {out}");
    let (out, _, _) = run_tcr_peer(&peers, &["lend", &node, "--list"]);
    assert!(
        out.contains("ended=false"),
        "00:00:01 rolls to tomorrow: {out}"
    );

    // And `--relend` with no end at all clears it: "re-lend with
    // one click", on the same lease id every surface is already holding.
    let (out, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--relend", &id]);
    assert!(ok, "relend with no end: {out}{err}");
    assert!(out.contains("until=none"), "{out}");
    let file = teamclaude_rs::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(file.peers[0].lend[0].until, None);
    assert_eq!(
        teamclaude_rs::peer::config::lease_id_string(file.peers[0].lend[0].id),
        id,
        "re-lending keeps the id, because deleting and re-creating would strand every \
         surface holding the old one"
    );
}

/// **`tcr peer ls --json` carries `lentTo`, keyed by account label, with the
/// lease's real scope and end**, through the binary a
/// panel actually runs.
///
/// The library gates above prove `lent_to` computes the map. This proves the
/// CLI emits it: the block was absent from this output entirely before this
/// fix, and a panel cannot render a line the JSON does not carry. The account
/// labels come from the main config, which is why `--config` exists on this
/// verb.
///
/// `spare-fake` is the control: it is in no group, so a `group:work` lease must
/// not reach it, and its absence from a map that DOES have a `work-fake` key is
/// the assertion that means something.
///
/// Watch it fail by removing `lent_to` from `PeerLsJson`: the block is then
/// absent and the first assertion fails.
#[test]
fn peer_ls_json_carries_the_lent_to_line_keyed_by_account_label() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("teamclaude.json");
    // Obviously fake accounts: no real email, no org uuid. One in group `work`,
    // one in none.
    std::fs::write(
        &config,
        r#"{"accounts":[
             {"name":"work-fake","accessToken":"not-a-real-token","groups":["work"]},
             {"name":"spare-fake","accessToken":"not-a-real-token"}
           ]}"#,
    )
    .expect("the config fixture writes");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--scope",
            "group:work",
            "--for",
            "2h",
        ],
    );
    assert!(ok, "lend: {out}{err}");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "ls", "--json"])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(["--config", config.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer ls --json");
    assert!(
        output.status.success(),
        "ls --json: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ls --json emits one JSON object");

    let lent = &json["lentTo"];
    let row = &lent["work-fake"][0];
    assert_eq!(
        row["scope"]["group"], "work",
        "the grouped account's line names the scope it was lent under: {lent}"
    );
    assert_eq!(
        row["ended"], false,
        "two hours ahead has not passed: {lent}"
    );
    assert!(
        row["until"].is_u64(),
        "the end is absolute unix seconds: {lent}"
    );
    assert!(
        row["id"].is_string(),
        "the lease id is a string, never a number no double can hold: {lent}"
    );
    assert_eq!(row["peer"], "attic-nuc");

    assert!(
        lent.get("spare-fake").is_none(),
        "a `group:work` lease does not reach an account outside the group: {lent}"
    );
}

/// **`tcr peer ls --json` carries the four readouts the panel's Peers tab
/// draws: a grant's `mode`, its `handedKeyUntil`, the `internet` switch, and
/// the `exits` map.**
///
/// Every one of them was absent from this output, and a panel cannot draw a
/// key the JSON does not carry. They are asserted together and from one
/// invocation on purpose: they are one screen, and a reader that had to run
/// the binary four times would be reading four different instants.
///
/// The controls are the assertions that mean something. `mode` is read off the
/// SERVE grant as well as the hand one, because the serve value used to be
/// omitted as a serde default and an absent key reads as "unknown" to a
/// decoder that does not know the default. `handedKeyUntil` is absent on the
/// serve grant, because a serve grant hands nothing over whatever else is
/// true. And `spare-fake`, an account with no exit lock at all, is absent from
/// `exits` beside a `pinned-fake` that is present.
///
/// Watched red four ways, one per key: restore `skip_serializing_if =
/// "LendMode::is_serve"` on `LendGrant::mode`, return `None` from
/// `peer_ls_handed_key_until`, drop `internet` from `PeerLsJson`, and return
/// an empty map from `peer_ls_exits`.
#[test]
fn peer_ls_json_carries_the_mode_the_handed_key_the_switch_and_the_exits() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("teamclaude.json");
    // Obviously fake accounts, no real email and no org uuid. `work-fake` is
    // the one a hand grant can reach; `pinned-fake` carries the exit lock;
    // `spare-fake` has neither and is the control on both maps.
    //
    // The expiry is a fixed instant far ahead rather than a clock read, so the
    // assertion below is an equality and not a range.
    let expires_at_ms: i64 = 1_893_456_000_000;
    std::fs::write(
        &config,
        format!(
            r#"{{"accounts":[
                 {{"name":"work-fake","accessToken":"not-a-real-token","groups":["work"],
                   "expiresAt":{expires_at_ms}}},
                 {{"name":"pinned-fake","accessToken":"not-a-real-token",
                   "egress":"via {node}","egressStrict":true}},
                 {{"name":"spare-fake","accessToken":"not-a-real-token"}}
               ]}}"#
        ),
    )
    .expect("the config fixture writes");

    // A hand grant on the group the unpinned account is in, and a serve grant
    // beside it on the other window: two grants, two modes, one row.
    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--scope",
            "group:work",
            "--mode",
            "hand",
            "--config",
            config.to_str().expect("a utf-8 path"),
        ],
    );
    assert!(ok, "the hand lend: {out}{err}");
    let (out, err, ok) = run_tcr_peer(
        &peers,
        &["lend", &node, "--window", "5h", "--fraction", "0.2"],
    );
    assert!(ok, "the serve lend: {out}{err}");
    let (out, err, ok) = run_tcr_peer(&peers, &["internet", "on"]);
    assert!(ok, "internet on: {out}{err}");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "ls", "--json"])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(["--config", config.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer ls --json");
    assert!(
        output.status.success(),
        "ls --json: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ls --json emits one JSON object");

    let lend = json["peers"][0]["lend"]
        .as_array()
        .unwrap_or_else(|| panic!("the row carries its grants: {json}"))
        .clone();
    let hand = lend
        .iter()
        .find(|grant| grant["window"] == "7d")
        .unwrap_or_else(|| panic!("the hand grant is on the 7d window: {json}"));
    let serve = lend
        .iter()
        .find(|grant| grant["window"] == "5h")
        .unwrap_or_else(|| panic!("the serve grant is on the 5h window: {json}"));

    assert_eq!(hand["mode"], "hand", "the hand grant says so: {json}");
    assert_eq!(
        serve["mode"], "serve",
        "and so does the serve grant, rather than omitting the key and leaving a \
         decoder to guess the default: {json}"
    );
    assert_eq!(
        hand["handedKeyUntil"],
        serde_json::json!(expires_at_ms / 1_000),
        "the handed key's deadline is the covered account's own expiry, in unix \
         SECONDS: {json}"
    );
    assert!(
        serve.get("handedKeyUntil").is_none(),
        "a serve grant hands nothing over, so it carries no deadline: {json}"
    );

    assert_eq!(
        json["internet"], true,
        "the switch `tcr peer internet on` just set: {json}"
    );

    let exits = &json["exits"];
    let pinned = &exits["pinned-fake"];
    assert_eq!(
        pinned["egress"],
        format!("via {node}"),
        "the exit lock is reported in the file's own words: {json}"
    );
    assert_eq!(
        pinned["egressStrict"], true,
        "and with its strictness, which is the half that decides what an \
         unreachable route costs: {json}"
    );
    assert_eq!(
        pinned["peerDown"], true,
        "the pinned Mac has no endpoint on its row, so there is no way to reach \
         it right now: {json}"
    );
    assert!(
        pinned.get("waitingSeconds").is_none(),
        "this Mac has never heard from it, and `waiting since never` is not a \
         duration worth printing: {json}"
    );
    assert!(
        exits.get("spare-fake").is_none(),
        "an account with no pin and no strictness is absent rather than present \
         saying `local`: {json}"
    );
}

/// **An account whose own name is not a label still gets the "Lent
/// to …" line, through the binary a panel runs.**
///
/// `tcr peer ls --json` built its account list with
/// `filter_map(|a| sanitize_label(&a.name).ok())`, so an account named by
/// email (which is what an OAuth account is usually named here), was dropped
/// from `lentTo` outright. Its card then showed no line however much of it was
/// lent, and the two facts the filter conflated are different ones: that
/// account cannot be NAMED by `--scope account:<label>`, and it is very much
/// lent under an `all` grant.
///
/// The key is the name `tcr status` prints, because that is what a panel
/// matches its account card against; `lease::lent_to` sanitizes it itself for
/// the scope match, so `spare-fake` below stays out of a `group:work` lease
/// exactly as before.
///
/// Watch it fail by restoring the `filter_map(... .ok())` in `main.rs`'s
/// `peer ls --json` arm: the `alice@example.com` key disappears while every
/// other assertion here still passes.
#[test]
fn peer_ls_json_keeps_an_account_whose_name_is_not_a_label() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("teamclaude.json");
    // `alice@example.com` is the obviously-fake email this repository uses in
    // every fixture, and an `@` is exactly what `sanitize_label` refuses.
    std::fs::write(
        &config,
        r#"{"accounts":[
             {"name":"alice@example.com","accessToken":"not-a-real-token"},
             {"name":"spare-fake","accessToken":"not-a-real-token"}
           ]}"#,
    )
    .expect("the config fixture writes");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--scope",
            "all",
        ],
    );
    assert!(ok, "lend: {out}{err}");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "ls", "--json"])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(["--config", config.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer ls --json");
    assert!(
        output.status.success(),
        "ls --json: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ls --json emits one JSON object");

    let lent = &json["lentTo"];
    // The positive control first: the account whose name IS a label is there,
    // so an absence below is about the un-labelled one and not about an empty
    // `lentTo`.
    assert_eq!(
        lent["spare-fake"][0]["peer"], "attic-nuc",
        "the control account is lent under `all`: {lent}"
    );
    assert_eq!(
        lent["alice@example.com"][0]["peer"], "attic-nuc",
        "an `all` lease lends every account, including the one whose name is not a label: \
         {lent}"
    );
    assert_eq!(
        lent["alice@example.com"][0]["scope"], "all",
        "and the line names the scope it was lent under: {lent}"
    );
}

/// **A grant whose scope this Mac cannot hold a request inside is SKIPPED, and
/// the next grant on the same window mints instead.**
///
/// The grant list is positional (several leases, one per scope),
/// so `grant_for` returned the first row on the window whatever its scope. A
/// `--scope account:bob` row naming an account this lender does not carry
/// therefore shadowed the `all` row beneath it: the lease minted against the
/// dead scope and `handle_serve_on` refused every request on it with
/// `Unsupported`, while a grant that could have served sat one line below,
/// never reached.
///
/// The enforceability answer is the production one: `Manager`'s own
/// `scope_restriction`, the same function the serving leg asks: so a grant
/// skipped here and a request refused there cannot disagree.
///
/// Watch it fail by making the `enforceable` closure in `Ledger::grant` return
/// `true` unconditionally: the lease then mints at 0.05 from the `account:bob`
/// row, which is the fraction no request can spend.
#[test]
fn an_unenforceable_grant_is_skipped_and_the_next_one_on_the_window_mints() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([24_u8; 32]);

    // Two grants on ONE window. The first names an account this fleet does not
    // have; the second is the `all` grant the operator also wrote.
    let mut named = LendGrant::new(Window::SevenDay, 0.05, 300, 1);
    named.scope = tcr_peer_wire::LendScope::Accounts(vec!["bob".to_string()]);
    let everything = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    write_peers(
        &peers,
        vec![lender_row_for(borrower, vec![named, everything])],
    );
    let store = PeerStore::open(&peers).expect("the peers file opens");

    // The lender's real fleet: one account, and it is not `bob`.
    let manager = fleet::lending_manager("http://127.0.0.1:1", Default::default());

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let lease = ledger
        .grant(&borrower, &ask, &store, manager.as_ref())
        .answer
        .lease
        .expect("the `all` grant one row down mints a lease");

    assert_eq!(
        lease.unit,
        LeaseUnit::Fraction(0.20),
        "the lease came from the `all` grant (0.20), not from the unenforceable \
         `account:bob` row above it (0.05)"
    );
    assert_eq!(
        ledger.scope_of(lease.lease_id),
        tcr_peer_wire::LendScope::All,
        "and the scope recorded beside it is the one that can actually be served"
    );

    // The control, on the SAME fleet: a scope this Mac CAN hold is not skipped.
    // Without this, a `grant_for` that skipped every scoped grant would pass
    // the assertions above.
    let mut serveable = LendGrant::new(Window::FiveHour, 0.07, 300, 1);
    serveable.scope = tcr_peer_wire::LendScope::Accounts(vec!["lender-fake".to_string()]);
    let five_hour_all = LendGrant::new(Window::FiveHour, 0.30, 300, 2);
    write_peers(
        &peers,
        vec![lender_row_for(borrower, vec![serveable, five_hour_all])],
    );
    let store = PeerStore::open(&peers).expect("the peers file opens");
    let five_hour_ask = LeaseRequest {
        window: Window::FiveHour,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 300,
        max_inflight: 2,
    };
    ledger.note_owner_headroom(Window::FiveHour, 0.40);
    let kept = ledger
        .grant(&borrower, &five_hour_ask, &store, manager.as_ref())
        .answer
        .lease
        .expect("an enforceable scoped grant mints");
    assert_eq!(
        kept.unit,
        LeaseUnit::Fraction(0.07),
        "`account:lender-fake` names an account this fleet carries, so it is the grant that \
         mints: the skip is about enforceability and not about scopes in general"
    );
}

/// **A malformed grant inside a `lend` LIST is refused with its own line and
/// column**, which is what `read_peer_file` promises the operator.
///
/// `one_or_many_grants` was `#[serde(untagged)]`, and an untagged enum buffers
/// its input before trying each arm. The position it then reports is where the
/// BUFFER ran out, not where the bad field is: for this fixture it named line
/// 11 (where the list closes), for a defect on line 9, under the message
/// "data did not match any variant of untagged enum OneOrMany", which names
/// neither the field nor anything an operator can act on.
///
/// Watch it fail by putting the untagged enum back: measured, the refusal then
/// reads `... untagged enum OneOrMany at line 11 column 5`, and the first
/// assertion below names it.
#[test]
fn a_malformed_grant_inside_a_list_is_refused_with_its_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let node = PeerId([31_u8; 32]).to_wire();
    // The second grant's `fraction` is a string. Line 9 of this fixture, and
    // the reader has to say so.
    write_raw_peers(
        &peers,
        &format!(
            r#"{{
  "peers": [
    {{
      "node": "{node}",
      "label": "attic-nuc",
      "addedAt": 1,
      "lend": [
        {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }},
        {{ "window": "5h", "fraction": "a lot", "ttlS": 300, "maxInflight": 1 }}
      ]
    }}
  ]
}}"#
        ),
    );

    let refusal = teamclaude_rs::peer::config::read_or_default(&peers)
        .expect_err("a grant whose fraction is a string is not a readable peers file")
        .to_string();

    assert!(
        refusal.contains("line 9"),
        "the refusal names the line the bad grant is on: {refusal}"
    );
    assert!(
        !refusal.contains("OneOrMany"),
        "a buffered untagged enum's refusal names its own enum and not the operator's \
         field, which is the message this stopped producing: {refusal}"
    );
    assert!(
        refusal.contains("fraction") || refusal.contains("invalid type"),
        "and it says what was wrong with it, in serde's own words: {refusal}"
    );

    // The positive control: the same file with a valid second grant reads, so
    // the refusal above is about the bad field and not about the list shape.
    let good = dir.path().join("good.json");
    write_raw_peers(
        &good,
        &format!(
            r#"{{
  "peers": [
    {{
      "node": "{node}",
      "label": "attic-nuc",
      "addedAt": 1,
      "lend": [
        {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }},
        {{ "window": "5h", "fraction": 0.1, "ttlS": 300, "maxInflight": 1 }}
      ]
    }}
  ]
}}"#
        ),
    );
    let file = teamclaude_rs::peer::config::read_or_default(&good).expect("the list shape reads");
    assert_eq!(file.peers[0].lend.len(), 2);
}

// ---------------------------------------------------------------------------
// The READ surface: `until` and `ended` on a `tcr peer ls --json`
// row, and the peers file opened once per accepted connection
// ---------------------------------------------------------------------------

/// **A `tcr peer ls --json` row carries `until` and `ended`.**
///
/// Through the BINARY this build produced, because the writer is in
/// `src/main.rs` and the thing being gated is the JSON a panel decodes: not a
/// function a test could call. Three shapes, in the order an operator meets
/// them: a lease with no end, a lease given one by `--for 2h` on the real CLI,
/// and a lease whose end has passed.
///
/// The last one is the assertion row 13 exists for: an ended lease is still in
/// the list. A reader that dropped it would take the "re-lend with one click"
/// row off the screen at the exact moment it becomes the only useful control.
///
/// Watch it fail by returning `(None, false)` from `peer_ls_ends`: the `--for
/// 2h` assertion reads `null` and the ended one reads `false`.
#[test]
fn a_peer_ls_json_row_carries_until_and_ended() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    // A missing config, deliberately: `lentTo` is keyed by account label and
    // this test must never read the operator's real config directory.
    let config_path = home.path().join("teamclaude.json");
    let borrower = PeerId([7_u8; 32]);

    write_peers(
        &peers_path,
        vec![lender_row_for(
            borrower,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );

    let row = ls_json_row(&peers_path, &config_path);
    assert_eq!(
        row["until"],
        serde_json::Value::Null,
        "a lease with no end has no end to print"
    );
    assert_eq!(row["ended"], serde_json::Value::Bool(false));

    // `--for 2h` on the real CLI, so this covers argv → file → JSON and not
    // only the writer.
    let before = unix_seconds();
    let lent = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "lend", &borrower.to_wire()])
        .args(["--peers", &peers_path.display().to_string()])
        .args(["--window", "7d", "--fraction", "0.2", "--for", "2h"])
        .output()
        .expect("`tcr peer lend` runs");
    assert!(
        lent.status.success(),
        "`tcr peer lend --for 2h`: {}",
        String::from_utf8_lossy(&lent.stderr)
    );
    let after = unix_seconds();

    let row = ls_json_row(&peers_path, &config_path);
    let until = row["until"].as_u64().expect("`until` is a number");
    assert!(
        until >= before + 7_200 && until <= after + 7_200,
        "`--for 2h` prints the end it wrote: {until} is not two hours after \
         {before}..={after}"
    );
    assert_eq!(
        row["ended"],
        serde_json::Value::Bool(false),
        "a lease that ends in two hours has not ended"
    );

    // An end that has passed. Written directly because `--for` and `--until`
    // can only name a FUTURE instant, which is correct and leaves the ended
    // state unreachable through argv.
    let ended_at = unix_seconds() - 60;
    let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    grant.until = Some(ended_at);
    write_peers(&peers_path, vec![lender_row_for(borrower, vec![grant])]);

    let listing = ls_json(&peers_path, &config_path);
    let rows = listing["peers"].as_array().expect("`peers` is an array");
    assert_eq!(
        rows.len(),
        1,
        "an ended lease STAYS in the list, greyed, so it can be re-lent"
    );
    assert_eq!(rows[0]["ended"], serde_json::Value::Bool(true));
    assert_eq!(
        rows[0]["until"].as_u64(),
        Some(ended_at),
        "the ended row prints WHEN it ended, which is what its row reads"
    );
}

/// The whole `tcr peer ls --json` document, from the binary this build made.
fn ls_json(peers_path: &Path, config_path: &Path) -> serde_json::Value {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(["peer", "ls", "--json"])
        .args(["--peers", &peers_path.display().to_string()])
        .args(["--config", &config_path.display().to_string()])
        .output()
        .expect("`tcr peer ls --json` runs");
    assert!(
        out.status.success(),
        "`tcr peer ls --json`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("`--json` prints one JSON object")
}

/// The single row [`ls_json`] printed, with the count asserted rather than
/// indexed blindly: `peers[0]` on an empty array is a panic that reads like a
/// harness bug instead of the missing row it is.
fn ls_json_row(peers_path: &Path, config_path: &Path) -> serde_json::Value {
    let listing = ls_json(peers_path, config_path);
    let rows = listing["peers"].as_array().expect("`peers` is an array");
    assert_eq!(rows.len(), 1, "one pinned row");
    rows[0].clone()
}

/// The wall clock in unix SECONDS: the unit `until` is in.
fn unix_seconds() -> u64 {
    u64::try_from(teamclaude_rs::now_ms().max(0) / 1_000).unwrap_or(0)
}

/// **An accepted connection opens the peers file ONCE**: the review's L2.
///
/// It opened it twice: `serve_connection` read it before taking the socket
/// slot, and `serve_stream` read the same bytes again for the stream gate a
/// few microseconds later. Both reads are on the pre-authentication path of
/// every connection a stranger can open, so the duplicate is paid by the
/// machine being knocked at.
///
/// Counted, not reasoned about: `config::peers_file_opens` counts at
/// `read_peer_file`, which is the one place the bytes are read and the place
/// every route into the file passes through: so a regression through
/// `read_or_default`, `PeerStore::open` or `reload_if_changed` is counted the
/// same.
///
/// Per PATH, so the count is this test's temp file and not whatever the other
/// tests in this binary are doing on their own at the same moment.
///
/// Watch it fail by putting `read_or_default(&context.peers_path)?` back in
/// `serve_stream` in place of `store.file()`: the delta is 2.
#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_connection_opens_the_peers_file_once() {
    let (upstream, _hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");

    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );

    let now = teamclaude_rs::now_ms();
    let lease = Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };
    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        held.record_scoped(lease, borrower_id, tcr_peer_wire::LendScope::All);
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }

    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        ledger.clone(),
        lender_proxy.clone(),
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );
    let borrower_store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

    // AFTER the listener is up: standing one up reads the file once, and that
    // read is not per connection.
    let before = teamclaude_rs::peer::config::peers_file_opens(&lender_peers);
    // A positive control for the counter itself, on a path that is not the
    // lender's: an empty result here would otherwise read as "one open" no
    // matter what the listener did.
    assert_eq!(
        teamclaude_rs::peer::config::peers_file_opens(&borrower_peers),
        1,
        "the borrower's own store opened its own file once, so this counter \
         counts opens rather than always answering zero"
    );

    let ask = ask_for("/v1/messages");
    serve::open_serve(&lender_id, &lease, &ask, &HeaderMap::new(), &borrower_store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");

    let opened = teamclaude_rs::peer::config::peers_file_opens(&lender_peers) - before;
    assert_eq!(
        opened, 1,
        "one accepted connection opens the lender's peers file {opened} times; \
         the pre-authentication path must read it once and pass the value on"
    );
}

/// **The borrower's clock has a home on disk**: a lease this Mac is granted is
/// written to `peer-state.json`, and a FRESH `tcr peer ls --json` reads its
/// `until` back off the row for the Mac we borrow from.
///
/// `PeerLeaseProvider::leases` was a process-lifetime
/// cache and nothing else, so the one surface an operator reads :
/// `tcr peer ls --json`, which the panel renders: printed `until: null` on
/// every borrower while a lender was actively serving it. The two halves are
/// asserted separately on purpose: the state file (this process wrote it) and
/// the CLI's JSON (a DIFFERENT process read it), because a cache that persists
/// into a file no reader consults is the same defect with an extra step.
///
/// Watch it fail by deleting the `self.persist_borrowed();` line after the
/// cache insert in `PeerLeaseProvider::lease_for`: the state file reads zero
/// borrowed rows and the row's `until` is `null`.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrowed_lease_is_written_and_a_fresh_ls_reads_its_until() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");

    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    // The operator's own end, two hours out: the number the borrower's row has
    // to be able to print without asking anybody a second time.
    let ends_at = unix_seconds() + 7_200;
    let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    grant.until = Some(ends_at);
    write_peers(
        &lender_peers,
        vec![lender_row_for(borrower_id, vec![grant])],
    );

    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }
    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        ledger.clone(),
        lender_proxy.clone(),
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );

    // The production provider, asking for a lease it does not hold and then
    // spending it: no lease is handed to it, so what lands on disk is what the
    // borrow path itself learned.
    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let response = provider
        .try_serve(&ask)
        .await
        .expect("the lender granted a lease and served the borrowed request");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the request really was served on the lender's account"
    );

    let state_path = borrower_home.path().join("peer-state.json");
    let state = teamclaude_rs::peer::state::load(&state_path, teamclaude_rs::now_ms())
        .expect("the borrower's state file reads back");
    assert_eq!(
        state.borrowed.len(),
        1,
        "one lease was granted, so one borrowed row is on disk"
    );
    assert_eq!(
        state.borrowed[0].lender, lender_id,
        "the row names the Mac that granted it"
    );
    assert_eq!(
        state.borrowed[0].lease.until,
        Some(ends_at),
        "the lender's end crossed the wire and was written down"
    );
    assert!(
        state.leases.is_empty(),
        "a borrowed lease is not a lease this Mac lent: the two sections are \
         separate facts about opposite Macs"
    );

    // A DIFFERENT process, reading the same file: the whole point of item 1.
    // A missing config deliberately, so nothing reads the operator's real one.
    let config_path = borrower_home.path().join("teamclaude.json");
    let row = ls_json_row(&borrower_peers, &config_path);
    assert_eq!(
        row["until"].as_u64(),
        Some(ends_at),
        "the row for a Mac we borrow FROM prints when the lending ends"
    );
    assert_eq!(
        row["ended"],
        serde_json::Value::Bool(false),
        "a lease that ends in two hours has not ended"
    );
}

/// **A borrowed request reaches the API with the headers the API requires.**
///
/// The seam used to hand `PeerLeaseProvider` an `Ask` with no headers at all,
/// and the provider handed `open_serve` an empty map, so the lender put a
/// bearer on the request and nothing else. `api.anthropic.com` answers a
/// message request with no `anthropic-version` with a 400, and the borrower
/// returned that 400 to its client as a served answer. Every test passed,
/// because the fake upstream answered 200 to anything with a Bearer.
///
/// The instrument is the LENDER's own outbound request, recorded by the fake
/// upstream, and the fake upstream now refuses what the real one refuses
/// (`tests/tools/api_contract.rs`). Both halves are asserted: the headers the
/// API needs arrived, and the client's own credential did not.
///
/// Watch it fail by putting `&HeaderMap::new()` back in
/// `PeerLeaseProvider::try_serve`'s `open_serve` call: the lender's request is
/// answered 400 and `try_serve` returns `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrow_through_the_provider_carries_the_headers_the_api_requires() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");

    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );

    let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }
    let peer_addr = mesh::spawn_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        ledger.clone(),
        lender_proxy.clone(),
        std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );

    // The production provider, handed the `Ask` the proxy's own seam builds
    // from a client request: `Ask::scrubbed` has already removed the credentials.
    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let response = provider
        .try_serve(&ask)
        .await
        .expect("the lender granted a lease and served the borrowed request");
    assert_eq!(
        response.status().as_u16(),
        200,
        "a borrow the API would refuse is not a served answer"
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    let seen = served.lock().expect("served lock").clone();
    assert_eq!(seen.len(), 1);
    for required in ["anthropic-version", "content-type"] {
        assert!(
            seen[0].header(required).is_some(),
            "the lender's outbound request must carry {required}: {:?}",
            seen[0].headers
        );
    }
    assert_eq!(
        seen[0].token,
        fleet::LENDER_TOKEN,
        "and it is served on the LENDER's own credential"
    );
    // Over VALUES, so a client credential arriving under a name nobody thought
    // to check is caught too.
    assert!(
        !seen[0]
            .values()
            .iter()
            .any(|value| value.contains("not-a-real-client-token")),
        "the client's own credential must not cross to the lender: {:?}",
        seen[0].headers
    );
}

/// A borrowed lease whose END has passed reads `ended: true`, while its
/// RENEWAL ttl is still ahead, the two clocks `Lease::until` and
/// `Lease::expires_at_ms` keep apart.
///
/// Written straight into the state file because no operator act on a BORROWER
/// can set an end in the past: the end is the lender's, and `--for`/`--until`
/// can only name a future instant.
///
/// Watch it fail by dropping the `borrowed` arm from `peer_ls_ends`
/// (`src/main.rs`): `ended` reads `false` and `until` reads `null`.
#[test]
fn a_borrowed_lease_whose_end_has_passed_reads_ended() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let config_path = home.path().join("teamclaude.json");
    let lender = PeerId([9_u8; 32]);
    write_peers(
        &peers_path,
        vec![borrower_row_for(
            lender,
            true,
            vec!["127.0.0.1:1".to_string()],
        )],
    );

    let ended_at = unix_seconds() - 60;
    let now_ms = teamclaude_rs::now_ms();
    let state = teamclaude_rs::peer::state::PeerState {
        borrowed: vec![teamclaude_rs::peer::state::BorrowedRow {
            lease: Lease {
                lease_id: 0x5ea5e,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.20),
                granted_at_ms: now_ms - 120_000,
                // AHEAD: the renewal ttl has not run out, so `load` keeps the
                // row. What has passed is the LENDING, which is the fact the
                // greyed row renders.
                expires_at_ms: now_ms + 300_000,
                spent: 0.0,
                max_inflight: 2,
                until: Some(ended_at),
            },
            lender,
        }],
        ..teamclaude_rs::peer::state::PeerState::default()
    };
    teamclaude_rs::peer::state::save(&home.path().join("peer-state.json"), &state)
        .expect("the borrower's state file writes");

    let row = ls_json_row(&peers_path, &config_path);
    assert_eq!(
        row["ended"],
        serde_json::Value::Bool(true),
        "a borrowed lease whose end has passed greys its row"
    );
    assert_eq!(
        row["until"].as_u64(),
        Some(ended_at),
        "the ended row prints WHEN it ended"
    );
}

/// Borrowed leases are restored at boot, and one whose ttl ran out while the
/// process was down is not.
///
/// The restore is in `PeerLeaseProvider::new` rather than a second call,
/// because a borrower answering one request off an empty cache asks its lender
/// for a lease it already holds.
///
/// Watch it fail by returning `HashMap::new()` from the restore arm in
/// `PeerLeaseProvider::new`: `borrowed()` is empty and the live lease is lost.
#[test]
fn a_live_borrowed_lease_is_restored_at_boot_and_an_expired_one_is_not() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let live_lender = PeerId([3_u8; 32]);
    let dead_lender = PeerId([4_u8; 32]);
    let now_ms = teamclaude_rs::now_ms();

    let row_for = |lender: PeerId, expires_at_ms: i64| teamclaude_rs::peer::state::BorrowedRow {
        lease: Lease {
            lease_id: 0x11,
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.20),
            granted_at_ms: now_ms - 600_000,
            expires_at_ms,
            spent: 0.0,
            max_inflight: 2,
            until: None,
        },
        lender,
    };
    let state = teamclaude_rs::peer::state::PeerState {
        borrowed: vec![
            row_for(live_lender, now_ms + 300_000),
            row_for(dead_lender, now_ms - 1_000),
        ],
        ..teamclaude_rs::peer::state::PeerState::default()
    };
    teamclaude_rs::peer::state::save(&home.path().join("peer-state.json"), &state)
        .expect("the borrower's state file writes");

    let restored = PeerLeaseProvider::new(peers_path).borrowed();
    assert_eq!(
        restored.len(),
        1,
        "the expired lease is dropped on the way in, the live one is not"
    );
    assert_eq!(
        restored[0].lender, live_lender,
        "and the one that survived is the one whose ttl is still ahead"
    );
}

/// **A lender that accepts and never answers releases the client**, and the
/// local picker gets its turn.
///
/// The review's finding at `serve.rs`: the borrow path had no
/// deadline anywhere, a bare `TcpStream::connect`, an unbounded
/// `dial_handshake` and an unbounded wait for the reply: on the answer path of
/// a live client's request, while the listener's own half and the blind-egress
/// carry each carry one. A borrower pointed at a Mac that accepts TCP and then
/// says nothing waited forever with a real client on the other end.
///
/// The silent lender is a bare `TcpListener` that accepts and holds: it is the
/// shape the deadline exists for, and it stalls the exchange at the FIRST hop
/// inside it (the handshake), which no per-frame timeout further down would
/// ever see.
///
/// `Borrowed::NotThisLender` and not an error, because that is what makes the
/// picker proceed: an `Err` is a protocol failure and is logged as one. It is
/// the NOT-DELIVERED arm specifically, because this lender stalls in the
/// handshake and no body ever crossed: the other arm, a deadline reached after
/// the body crossed, is
/// `a_borrow_that_was_delivered_is_never_offered_to_another_lender`.
///
/// The whole call is wrapped in a five-second test-side timeout so that
/// deleting the production deadline goes RED rather than hanging the suite.
/// Watch it fail by replacing `open_serve_within`'s
/// `tokio::time::timeout(deadline, ...)` with a bare await of `borrow_once`:
/// the harness reports "the borrow never returned".
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_that_never_answers_releases_the_client_at_the_deadline() {
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = PeerId([5_u8; 32]);

    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a lender that will never answer");
    let addr = silent.local_addr().expect("the silent lender's address");
    tokio::spawn(async move {
        // Accepted and HELD: the connection is open, so the borrower is past
        // `connect` and stuck in the handshake, which is the hop the review
        // named first.
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(lender_id, true, vec![addr.to_string()])],
    );
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    let lease = lease_with(LEASE, 0.20, 0.0);
    let ask = ask_for("/v1/messages");

    let deadline = std::time::Duration::from_millis(200);
    let started = std::time::Instant::now();
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serve::open_serve_within(
            &lender_id,
            &lease,
            &ask,
            &HeaderMap::new(),
            &store,
            deadline,
        ),
    )
    .await
    .expect("the borrow never returned: a client is waiting on this path and nothing bounds it");
    let waited = started.elapsed();

    assert!(
        matches!(answer, Ok(serve::Borrowed::NotThisLender)),
        "a lender that never took the body is `NotThisLender`: the same fact to the \
         caller as a refusal, so the local picker proceeds, and not an error"
    );
    assert!(
        waited < std::time::Duration::from_secs(2),
        "the client was held for {waited:?} against a 200ms deadline"
    );
}

/// **A borrow whose body has crossed is never offered to another lender.**
///
/// The review's finding: a borrow that hit `BORROW_TIMEOUT` answered
/// `Ok(None)`, which is the same word the path uses for a refusal and for a
/// lender with no address, so `try_serve` handed the SAME body to the next
/// lender with a fresh request id. One POST was then executed and billed on
/// two accounts, and the borrower could not tell: both answers were 200s it
/// never waited for.
///
/// The fleet here is two lenders in front of ONE origin that holds every
/// answer for two seconds, against a 300 ms borrow deadline. So the first
/// lender takes the body, puts it on the origin, and cannot answer in time.
/// The instrument is the origin's own arrival count: it is the only place that
/// can say how many times this fleet sent one request.
///
/// Watch it fail by answering `Ok(Borrowed::NotThisLender)` in
/// `open_serve_within`'s delivered-timeout arm: the count goes to 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrow_that_was_delivered_is_never_offered_to_another_lender() {
    let (upstream, arrivals) = fleet::spawn_slow_upstream(std::time::Duration::from_secs(2)).await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    // Two lenders, each a real listener with its own key, ledger and peers
    // file, both serving through the same slow origin.
    let mut rows = Vec::new();
    let mut homes = Vec::new();
    for _ in 0..2 {
        let home = tempfile::tempdir().expect("a lender's temp home");
        let peers = home.path().join("tcr-peers.json");
        let id = teamclaude_rs::peer::id::NodeKey::load_or_mint(home.path())
            .expect("the lender's key")
            .id();
        write_peers(
            &peers,
            vec![lender_row_for(
                borrower_id,
                vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
            )],
        );
        let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
        {
            let mut held = ledger.lock().expect("ledger lock");
            held.note_owner_headroom(Window::SevenDay, 0.30);
        }
        let addr = mesh::spawn_lender(
            home.path().to_path_buf(),
            peers,
            ledger,
            lender_proxy.clone(),
            std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
            fleet::dry_manager(),
        )
        .await;
        rows.push(borrower_row_for(id, true, vec![addr.to_string()]));
        homes.push(home);
    }

    // The deadline is the borrower's own file, which is the production path:
    // shorter than the origin's delay, so the first lender runs out of time
    // with the body already gone.
    let file = teamclaude_rs::peer::config::PeerFile {
        peers: rows,
        borrow_timeout_ms: 300,
        ..teamclaude_rs::peer::config::PeerFile::default()
    };
    teamclaude_rs::peer::config::save(&borrower_peers, &file)
        .expect("the borrower's peers file writes");

    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let answer = provider
        .try_serve(&ask)
        .await
        .expect("a delivered borrow answers the client rather than falling through");
    assert_eq!(
        answer.status().as_u16(),
        502,
        "the client is told the outcome is unknown, not handed a retryable 429"
    );
    assert_eq!(
        answer
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false"),
        "a client that retries this would pay for the request twice"
    );

    // The origin's own count, after long enough for a second lender's copy to
    // have arrived if one had been sent.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(
        arrivals.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one POST, one arrival: a borrow that was delivered must not be executed on a \
         second account"
    );

    drop(homes);
}

/// **A hand-mode failure that never reached an upstream is not a delivery.**
///
/// The review's finding at `lease.rs`: the hand arm of `try_serve` mapped EVERY
/// `Err` from `serve_on_handed_bearer` to the no-retry 502, including a connect
/// or resolver failure where no byte left this Mac. `reqwest` reports "the
/// socket never opened" and "the reply never arrived" as one type, so a lease
/// whose upstream this Mac could not even reach ended the ladder and told the
/// client its request may already have been paid for.
///
/// What the arm now asks is this predicate, and it is the same pair of
/// questions `src/proxy.rs` asks on the direct path (`is_connect`, plus the
/// resolver check). It is measured against REAL `reqwest` errors carrying the
/// production context string, on both sides of the answer: the connect failure
/// that must fall through to the next lender, and the timeout that must not. A
/// test that measured only the first would pass against a predicate answering
/// "nothing left this Mac" for everything, which is the expensive direction to
/// be wrong in.
///
/// Watch it fail by dropping `cause.is_connect()` from the predicate: a refused
/// connect is then read as a request that may already have run, which is the
/// behaviour this replaces. Wrapping is NOT where the risk turned out to be:
/// `anyhow`'s own `downcast_ref` already walks the chain, measured by mutating
/// the walk back to a bare `downcast_ref` and watching this test stay green.
#[tokio::test(flavor = "multi_thread")]
async fn a_connect_failure_is_not_a_delivery_and_a_timeout_is() {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(150))
        .build()
        .expect("a client");

    // A port bound and immediately dropped: nothing listens, so the connect is
    // refused rather than hanging.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port to learn its number");
    let dead = closed.local_addr().expect("the dead address");
    drop(closed);

    let refused = client
        .post(format!("http://{dead}/v1/messages"))
        .send()
        .await
        .expect_err("nothing listens there");
    assert!(refused.is_connect(), "the fixture is a connect failure");
    let wrapped = anyhow::Error::new(refused)
        // The production context, verbatim from `serve_on_handed_bearer`.
        .context("peer hand: the request on the handed bearer did not reach upstream");
    assert!(
        lease::nothing_left_this_mac(&wrapped),
        "a connect failure under a context string is still a connect failure, and the \
         next lender may be asked"
    );

    // THE OTHER SIDE, and the one that costs money if it is wrong. A request
    // that went out and whose answer never came back may have run upstream.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the silent upstream");
    let silent_addr = silent.local_addr().expect("the silent address");
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _)) = silent.accept().await {
            open.push(conn);
        }
    });
    let timed_out = client
        .post(format!("http://{silent_addr}/v1/messages"))
        .send()
        .await
        .expect_err("a server that never answers");
    held.abort();
    assert!(
        !timed_out.is_connect(),
        "the fixture connected before it timed out"
    );
    let wrapped = anyhow::Error::new(timed_out)
        .context("peer hand: the request on the handed bearer did not reach upstream");
    assert!(
        !lease::nothing_left_this_mac(&wrapped),
        "a request that connected and then got no answer may have run on the owner's \
         account, so it is never offered to another lender"
    );

    // And something that is not a transport error at all is never read as one.
    assert!(
        !lease::nothing_left_this_mac(&anyhow::anyhow!("peer hand: a frame did not parse")),
        "only a transport failure can prove nothing left this Mac"
    );
}

/// **A lender that answers nothing has taken nothing, and the ladder goes
/// on.**
///
/// The review's finding: the borrower set `delivered` on the line that wrote
/// the body, so every lender-side refusal that happens BEFORE an upstream is
/// reached, the header gate with `inspect` turned off, and each `bail!` in
/// `handle_serve_on`, produced a no-retry 502 claiming the request may have
/// been billed, and skipped every remaining lender for the rest of the ask's
/// TTL. Nothing had been sent anywhere.
///
/// The fix is one frame: [`serve::ServeAck`]. The lender acknowledges taking
/// the request once its gates have passed and before it sends upstream, and the
/// borrower writes the body only after that ack. No ack is "not this lender".
///
/// The lender here reads the stream header and the request frame and then
/// answers nothing, which is what every one of those refusals looks like from
/// the borrower's end.
///
/// Watch it fail on the pre-ack build: the body is written before anything is
/// read back, so the answer is `DeliveredUnknown`.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_that_never_acknowledges_has_taken_nothing() {
    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.50, 300, 5)],
        )],
    );
    let addr =
        mesh::spawn_lender_that_takes_nothing(lender_home.path().to_path_buf(), lender_peers).await;
    write_peers(
        &borrower_peers,
        vec![borrower_row_for(lender_id, true, vec![addr.to_string()])],
    );
    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");

    let lease = lease_with(LEASE, 0.20, 0.0);
    let ask = ask_for("/v1/messages");
    let answer = serve::open_serve_within(
        &lender_id,
        &lease,
        &ask,
        &client_headers(),
        &store,
        std::time::Duration::from_millis(400),
    )
    .await;

    assert!(
        matches!(answer, Ok(serve::Borrowed::NotThisLender)),
        "a lender that never acknowledged has nothing of this request, so the next lender \
         may be asked and the client must not be told its request may have been billed"
    );
}

/// **A lender that refuses at its own gate is "not this lender", and the next
/// one serves.**
///
/// The review's finding: every refusal on the lender's half happens after the
/// borrower has already written the body, and the borrower set `delivered` on
/// that write. So a Mac that had turned `inspect` off, while the borrower still
/// held a live lease from it, answered nothing at all: the stream closed, the
/// borrower read `DeliveredUnknown` and told its client the request may have
/// been billed, with every remaining lender skipped.
///
/// The fix is one frame. The lender acknowledges TAKING the request once its
/// header and lease gates have passed and before it sends anything upstream,
/// and the borrower writes the body only after that ack. A close with no ack is
/// a lender that took nothing.
///
/// Two lenders in file order in front of one origin. The first has `inspect`
/// off, so its stream gate refuses; the second serves. The instrument is the
/// origin's own arrival count plus the status the client is handed.
///
/// Watch it fail on the pre-ack build: the answer is a 502 from the first
/// lender's silent close and the origin sees no arrival at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lender_that_refuses_at_its_gate_is_not_this_lender_and_the_next_serves() {
    let (upstream, arrivals, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    // Two lenders, asked in the order they are written: the first one grants a
    // lease and refuses the SERVE stream, the second one serves.
    let mut rows = Vec::new();
    let mut homes = Vec::new();
    for inspect in [false, true] {
        let home = tempfile::tempdir().expect("a lender's temp home");
        let peers = home.path().join("tcr-peers.json");
        let id = teamclaude_rs::peer::id::NodeKey::load_or_mint(home.path())
            .expect("the lender's key")
            .id();
        let mut row = lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        );
        // THE GATE THIS TEST IS ABOUT. `inspect` off is a lender that will hand
        // out a lease over CONTROL and refuse the SERVE stream that spends it,
        // which is exactly the shape an operator creates by turning disclosure
        // off while a borrower holds a cached lease.
        row.allow.inspect = inspect;
        write_peers(&peers, vec![row]);
        let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
        {
            let mut held = ledger.lock().expect("ledger lock");
            held.note_owner_headroom(Window::SevenDay, 0.30);
        }
        let addr = mesh::spawn_lender(
            home.path().to_path_buf(),
            peers,
            ledger,
            lender_proxy.clone(),
            std::sync::Arc::new(teamclaude_rs::peer::serve::NoFleetUtilization),
            fleet::dry_manager(),
        )
        .await;
        rows.push(borrower_row_for(id, true, vec![addr.to_string()]));
        homes.push(home);
    }

    let file = teamclaude_rs::peer::config::PeerFile {
        peers: rows,
        borrow_timeout_ms: 3_000,
        ..teamclaude_rs::peer::config::PeerFile::default()
    };
    teamclaude_rs::peer::config::save(&borrower_peers, &file)
        .expect("the borrower's peers file writes");

    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let answer = provider
        .try_serve(&ask)
        .await
        .expect("a refusal on the first lender must not end the ladder");
    assert_eq!(
        answer.status().as_u16(),
        200,
        "a lender that took nothing is not this lender, so the next one serves the client"
    );
    assert_eq!(
        arrivals.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one origin arrival: the refusing lender sent nothing and the serving \
         lender sent it once"
    );

    drop(homes);
}

/// **`borrowTimeoutMs` on the peers file is the deadline `open_serve` actually
/// uses**, not just a value [`serve::open_serve_within`] can be handed by
/// hand.
///
/// A peers file with `borrowTimeoutMs: 50` and the same silent, holding lender
/// as the test above: `open_serve` (the production call, never `_within`)
/// must release the client within a second, an order of magnitude under the
/// 10s built-in default, which proves the value on disk reached the deadline
/// rather than the constant winning anyway.
///
/// Watch it fail by reverting `open_serve` to hand `BORROW_TIMEOUT` to
/// `open_serve_within` instead of `store.file().borrow_timeout_ms`: the 50ms
/// setting is then ignored and the assertion on `waited` goes red at the 10s
/// default (bounded here by the 1s outer timeout so the suite fails fast
/// instead of hanging for ten seconds).
#[tokio::test(flavor = "multi_thread")]
async fn borrow_timeout_ms_on_the_peers_file_is_the_deadline_open_serve_uses() {
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = PeerId([6_u8; 32]);

    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a lender that will never answer");
    let addr = silent.local_addr().expect("the silent lender's address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });

    let file = teamclaude_rs::peer::config::PeerFile {
        peers: vec![borrower_row_for(lender_id, true, vec![addr.to_string()])],
        borrow_timeout_ms: 50,
        ..teamclaude_rs::peer::config::PeerFile::default()
    };
    teamclaude_rs::peer::config::save(&borrower_peers, &file)
        .expect("the borrower's peers file writes");

    let store = PeerStore::open(&borrower_peers).expect("the borrower's peers file");
    let lease = lease_with(LEASE, 0.20, 0.0);
    let ask = ask_for("/v1/messages");

    let started = std::time::Instant::now();
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        serve::open_serve(&lender_id, &lease, &ask, &HeaderMap::new(), &store),
    )
    .await
    .expect(
        "open_serve must return within 1s under a 50ms borrowTimeoutMs: the file's setting \
         was not read",
    );
    let waited = started.elapsed();

    assert!(
        matches!(answer, Ok(serve::Borrowed::NotThisLender)),
        "a lender that does not answer in time is `Ok(None)`: {:?}",
        answer.err()
    );
    assert!(
        waited < std::time::Duration::from_secs(1),
        "the client was held for {waited:?} against a 50ms `borrowTimeoutMs`"
    );
}

/// **A second ask is cut out of what is LEFT, not out of the whole.**
///
/// The review's finding: `Ledger::grant` took `lendable` from `headroom_for`
/// alone, which is what this Mac's fleet has left, and subtracted nothing for
/// the leases it had already handed out. A borrower that re-asked therefore
/// held up to `MAX_LEASES_PER_PEER` leases at a time, EACH clamped to the full
/// fraction: eight asks promised eight times the room that existed, and every
/// one of them passed `may_relay` until the account's own quota ran out.
///
/// Measured on the fractions the ledger minted, not on a log line: 0.30 of
/// room, a grant ceiling of 0.20, so the first lease is 0.20 and the second
/// can only be 0.10.
///
/// Watch it fail by dropping the `- self.committed_fraction(..)` term from
/// `Ledger::grant`: the second lease is 0.20 as well, and the two together
/// promise 0.40 of a 0.30 window.
#[test]
fn a_second_ask_is_cut_out_of_what_is_left_not_out_of_the_whole() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let borrower = PeerId([7_u8; 32]);
    write_peers(
        &peers_path,
        vec![lender_row_for(
            borrower,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );
    let store = PeerStore::open(&peers_path).expect("the peers file opens");
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        ttl_s: 600,
        max_inflight: 2,
    };

    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.30);

    let first = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("the first ask mints against 0.30 of room");
    assert_eq!(
        first.unit,
        LeaseUnit::Fraction(0.20),
        "the first lease is the grant's own ceiling"
    );
    assert!(
        (ledger.committed_fraction(
            &tcr_peer_wire::LendScope::All,
            Window::SevenDay,
            teamclaude_rs::now_ms()
        ) - 0.20)
            .abs()
            < 1e-9,
        "and 0.20 of the window is now promised"
    );

    let second = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer
        .lease
        .expect("0.10 is still lendable, so the second ask mints a smaller lease");
    let LeaseUnit::Fraction(fraction) = second.unit else {
        panic!("a fraction lease was asked for and a fraction lease must come back");
    };
    assert!(
        (fraction - 0.10).abs() < 1e-9,
        "the second lease is what is LEFT (0.30 - 0.20), not the grant ceiling again: \
         {fraction}"
    );
}

/// **Two scopes over the same accounts subtract from each other.**
///
/// The review's finding: `committed_fraction` counted only live leases whose
/// scope STRING matched the new ask's, while `headroom_for` combines the two
/// figures with `scoped.min(all)` and so treats them as one pool. An `all`
/// lease and a `group:work` lease over the same accounts therefore never
/// subtracted from each other, and the same headroom was promised twice: the
/// `all` lease could spend the whole window and the group lease still read as
/// fully funded.
///
/// Measured on the figure itself rather than through a mint, because the mint
/// needs a fleet reader that can enforce a group and the claim under test is
/// the arithmetic. Both directions are asserted: the group ask sees the `all`
/// lease, and the `all` ask sees the group lease. A scope this ledger CAN
/// prove disjoint is still disjoint, which is the third leg: two account sets
/// that name nobody in common do not subtract.
///
/// Watch it fail by restoring the `scope_key(..) == key` filter in
/// `Ledger::committed_fraction`: the first two assertions read 0.0, and the
/// ledger says nothing is promised while two leases are live.
#[test]
fn a_lease_on_an_overlapping_scope_is_already_promised_room() {
    let now = teamclaude_rs::now_ms();
    let borrower = PeerId([7_u8; 32]);
    let work = tcr_peer_wire::LendScope::Group("work".to_string());
    let all = tcr_peer_wire::LendScope::All;
    let live = |lease_id: u128, budget: f64| Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(budget),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    };

    let mut ledger = Ledger::new();
    ledger.record_scoped(live(0xA11, 0.20), borrower, all.clone());
    ledger.record_scoped(live(0xB22, 0.05), borrower, work.clone());

    assert!(
        (ledger.committed_fraction(&work, Window::SevenDay, now) - 0.25).abs() < 1e-9,
        "a `group:work` ask is cut out of what is left after BOTH leases, and the ledger \
         says {}",
        ledger.committed_fraction(&work, Window::SevenDay, now)
    );
    assert!(
        (ledger.committed_fraction(&all, Window::SevenDay, now) - 0.25).abs() < 1e-9,
        "and so is an `all` ask, in the other direction: {}",
        ledger.committed_fraction(&all, Window::SevenDay, now)
    );

    // The one pair this ledger can prove apart without the manager's group
    // membership: two account sets naming nobody in common.
    let mut named = Ledger::new();
    named.record_scoped(
        live(0xC33, 0.20),
        borrower,
        tcr_peer_wire::LendScope::Accounts(vec!["alice".to_string()]),
    );
    assert_eq!(
        named.committed_fraction(
            &tcr_peer_wire::LendScope::Accounts(vec!["bob".to_string()]),
            Window::SevenDay,
            now
        ),
        0.0,
        "`account:bob` draws from nothing `account:alice` holds, so nothing is promised"
    );
    assert!(
        (named.committed_fraction(
            &tcr_peer_wire::LendScope::Accounts(vec!["bob".to_string(), "alice".to_string()]),
            Window::SevenDay,
            now
        ) - 0.20)
            .abs()
            < 1e-9,
        "and a set that names alice as well does overlap"
    );
}

/// **A busy borrower cannot evict another lease's replay defence, nor its
/// own.**
///
/// The review's finding: the served cache was one FIFO for the whole process,
/// bounded by `SERVED_CAPACITY`. A borrower with requests of its own to make
/// pushed every other lease's `(lease, request)` pair out of it, and the
/// replay defence for those leases went quiet: a request id already served for
/// them was admitted again. It could also evict its OWN older pair and then
/// replay that.
///
/// Both halves are measured here, at the bound itself rather than at a
/// plausible-looking number: one lease fills its own cache and the OTHER
/// lease's remembered pair still refuses a replay.
///
/// Watch it fail by putting `admit_pair`'s eviction back on the whole cache
/// (`while cache-wide length >= SERVED_CAPACITY { drop the global oldest }`
/// with no per-lease bound): the quiet lease's pair is gone and the replay is
/// admitted.
#[test]
fn a_busy_lease_cannot_evict_another_leases_replay_defence() {
    let quiet = 0x9111_u128;
    let busy = 0x9222_u128;
    let peer = PeerId([7_u8; 32]);
    let now = teamclaude_rs::now_ms();

    let mut ledger = Ledger::new();
    for lease_id in [quiet, busy] {
        ledger.record_scoped(
            Lease {
                lease_id,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.20),
                granted_at_ms: now,
                expires_at_ms: now + 600_000,
                spent: 0.0,
                max_inflight: 64,
                until: None,
            },
            peer,
            tcr_peer_wire::LendScope::All,
        );
    }
    ledger.note_owner_headroom(Window::SevenDay, 0.90);

    // The quiet lease serves one request and releases its slot.
    ledger
        .enter_relay(quiet, &peer, 1, now)
        .expect("the quiet lease's first request is admitted");
    ledger.leave_relay(quiet);

    // The busy one serves more than the whole process used to remember.
    for request_id in 0..(lease::SERVED_CAPACITY as u128 + 16) {
        if ledger.enter_relay(busy, &peer, request_id, now).is_ok() {
            ledger.leave_relay(busy);
        }
    }

    assert!(
        ledger.served_len_for(busy) <= lease::SERVED_PER_LEASE_CAPACITY,
        "a lease may not remember more ids than its own bound: {}",
        ledger.served_len_for(busy)
    );
    assert_eq!(
        ledger.enter_relay(quiet, &peer, 1, now),
        Err(RelayRefusal::Replayed),
        "the quiet lease's one served pair must still be remembered: a busy borrower may \
         not turn another lease's replay defence off"
    );
}

/// **One pinned peer may hold eight live leases here, and the ninth ask is
/// refused**: the review's finding.
///
/// Nothing capped how many leases one peer could mint, and every mint rewrites
/// the whole state file under the same `FileLock` that `tcr peer accept` and
/// `tcr peer block` take with a hard five-second give-up: an authenticated peer
/// asking in a loop was both an unbounded ledger and an operator whose Accept
/// could not get the lock.
///
/// Nine asks, all identical and all from a peer this Mac lends to. Eight rows,
/// a refusal on the ninth, and the eight are untouched: a cap that refused by
/// dropping an older lease would break the borrower it was granted to.
///
/// A SECOND peer still gets its lease, which is what makes this a per-grantee
/// cap rather than a per-Mac one: one noisy borrower must not close the lender
/// to every other Mac in the mesh.
///
/// Watch it fail by raising `MAX_LEASES_PER_PEER` to 9: the ninth ask mints and
/// `ninth.lease.is_none()` goes red. (Measured: the first draft of this test
/// looped to the constant itself and stayed GREEN under exactly that mutation,
/// which is why the eight and the nine below are literals.)
#[test]
fn a_ninth_live_lease_for_one_peer_is_refused_and_the_eight_stand() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let borrower = PeerId([7_u8; 32]);
    let other = PeerId([8_u8; 32]);
    write_peers(
        &peers_path,
        vec![
            lender_row_for(
                borrower,
                vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
            ),
            lender_row_for(other, vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)]),
        ],
    );
    let store = PeerStore::open(&peers_path).expect("the peers file opens");
    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 600,
        max_inflight: 8,
    };

    let mut ledger = Ledger::new();
    // ENOUGH ROOM FOR EIGHT, because this test is about the COUNT cap and
    // nothing else. It used to note 0.40, which was room for two leases of
    // 0.20 and eight leases only because `grant` did not subtract what it had
    // already promised: the third ask minted a full fraction out of headroom
    // that was already spoken for. `Ledger::committed_fraction` subtracts it
    // now, so a fixture that wants eight leases has to have room for eight,
    // and the ask that runs out of room is
    // `a_second_ask_is_cut_out_of_what_is_left_not_out_of_the_whole`.
    ledger.note_owner_headroom(Window::SevenDay, 2.0);
    // EIGHT as a literal, and nine asks as a literal: a loop bounded by
    // `MAX_LEASES_PER_PEER` would pass against any value of it, which is a test
    // of nothing. The constant is asserted separately, as the contract it is.
    assert_eq!(
        lease::MAX_LEASES_PER_PEER,
        8,
        "this test is written against a cap of eight"
    );
    let mut minted = Vec::new();
    for ask_number in 1..=8 {
        let grant = ledger
            .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
            .answer;
        minted.push(
            grant
                .lease
                .unwrap_or_else(|| panic!("ask {ask_number} is inside the cap and must mint")),
        );
    }

    let ninth = ledger
        .grant(&borrower, &ask, &store, &serve::NoFleetUtilization)
        .answer;
    assert!(
        ninth.lease.is_none(),
        "the ninth live lease for one peer must not be minted"
    );
    assert_eq!(
        ninth.refusal,
        Some(LeaseRefusal::TooManyLeases),
        "the ninth ask is answered with a refusal frame naming the count cap, not the owner's \
         guard: a borrower that reads one for the other either hammers a dead lease or \
         backs off from a live one for the wrong reason"
    );

    let now_ms = teamclaude_rs::now_ms();
    assert_eq!(
        ledger.live_for(&borrower, now_ms),
        8,
        "the refusal leaves the leases this peer already holds exactly where it found them"
    );
    let live: Vec<u128> = ledger
        .live(now_ms)
        .iter()
        .map(|lease| lease.lease_id)
        .collect();
    for lease in &minted {
        assert!(
            live.contains(&lease.lease_id),
            "a cap that evicted an already-granted lease would break the borrower \
             holding it"
        );
    }

    // The cap is per GRANTEE: another Mac is not refused because this one asked
    // nine times.
    assert!(
        ledger
            .grant(&other, &ask, &store, &serve::NoFleetUtilization)
            .answer
            .lease
            .is_some(),
        "a second peer's first lease is refused by a cap that counts per Mac instead \
         of per grantee"
    );
}

/// **Five concurrent first asks against one lender mint ONE lease.**
///
/// The race measured live: `PeerLeaseProvider::lease_for` read
/// its cache under the mutex, missed, RELEASED the lock, then asked the lender
/// over CONTROL. Five borrows arriving together all missed, all asked, and all
/// got a lease of their own: so `max_inflight`, which bounds requests within
/// ONE lease, bound nothing across them, and the only ceiling left was the
/// lender's eight-lease-per-peer cap. Five concurrent borrows were five served
/// and zero refused.
///
/// The count is the lender's own `Control::LeaseRequest` frames and not the
/// leases its ledger holds: a lender that read five asks and minted one lease
/// would be a lender with a deduplicating ledger, which is a different fix in a
/// different process, and this gate is about what the BORROWER sends.
///
/// Watch it fail by deleting the ask-gate block in `lease_for` (the
/// `let _asking = gate.lock().await;` and the cache re-read under it): the
/// counter reads 5.
#[tokio::test(flavor = "multi_thread")]
async fn five_concurrent_first_asks_mint_one_lease() {
    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");

    let lender_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = teamclaude_rs::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();

    write_peers(
        &lender_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );

    let asks: mesh::LeaseAsks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Long enough that the four later asks are inside the first one's round
    // trip on any machine this suite runs on, and short enough that the test
    // costs a third of a second.
    let peer_addr = mesh::spawn_counting_lease_lender(
        lender_home.path().to_path_buf(),
        lender_peers.clone(),
        asks.clone(),
        std::time::Duration::from_millis(300),
    )
    .await;

    write_peers(
        &borrower_peers,
        vec![borrower_row_for(
            lender_id,
            true,
            vec![peer_addr.to_string()],
        )],
    );

    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let borrows = (0..5).map(|_| {
        let provider = &provider;
        async move {
            let ask = ask_for("/v1/messages");
            provider
                .try_serve(&ask)
                .await
                .map(|response| response.status().as_u16())
        }
    });
    let answers = futures::future::join_all(borrows).await;

    assert_eq!(
        asks.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "five concurrent first borrows against one lender ask it for ONE lease; \
         a borrower that asks per request holds one lease per request and \
         `max_inflight` binds nothing"
    );
    assert_eq!(
        answers,
        vec![Some(200); 5],
        "every borrow is served: single-flighting the ASK must not refuse a \
         request, only share the lease it waits for"
    );

    let state_path = borrower_home.path().join("peer-state.json");
    let state = teamclaude_rs::peer::state::load(&state_path, teamclaude_rs::now_ms())
        .expect("the borrower's state file reads back");
    assert_eq!(
        state.borrowed.len(),
        1,
        "one lease was granted, so one borrowed row is on disk"
    );
}

// ---------------------------------------------------------------------------
// The order a row's endpoints are tried in
// ---------------------------------------------------------------------------

/// **Newest observation first, and a forwarded hop last however new it is.**
///
/// The two rules are asserted against one row that violates both in its
/// stored order, because either rule alone passes a version that has the other
/// backwards: a list sorted only by time puts a fresh `Via` ahead of a direct
/// socket and spends a third machine's bytes to reach a peer this node could
/// have opened a socket to; a list grouped only by kind dials a year-old
/// address before yesterday's.
///
/// A hop costs another operator's machine and its consent, which is why it is
/// what is left when nothing direct answered and never a choice made on a
/// timestamp.
///
/// Watched red: with `order.sort_by_key(Endpoint::is_via)` deleted from
/// `serve::dial_order`, this fails with "a forwarded hop is tried last", the
/// `Via` endpoint leads, because it is the newest thing on the row.
#[test]
fn a_row_is_dialled_newest_first_and_through_a_peer_last() {
    let old: std::net::SocketAddr = "192.0.2.1:9600".parse().expect("a test address");
    let fresh: std::net::SocketAddr = "192.0.2.2:9600".parse().expect("a test address");
    let forwarder = PeerId([0x33; 32]);

    let mut row = PeerRow {
        node: PeerId([0x11; 32]),
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 1_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    };
    row.observe_endpoint(Endpoint::direct(old, 1_000, EndpointSource::Paired));
    row.observe_endpoint(Endpoint::direct(fresh, 2_000, EndpointSource::Hello));
    // The newest thing on the row, and still the last thing to try.
    row.observe_endpoint(Endpoint::via(forwarder, 3_000, EndpointSource::Hello));

    let order = serve::dial_order(&row);
    let addrs: Vec<Option<std::net::SocketAddr>> =
        order.iter().map(Endpoint::direct_addr).collect();
    assert_eq!(
        addrs,
        vec![Some(fresh), Some(old), None],
        "newest direct socket, then the older one, then the hop: {order:?}"
    );
    assert!(
        order.last().map(Endpoint::is_via).unwrap_or(false),
        "a forwarded hop is tried last: {order:?}"
    );
}

/// **A lease's tokens land on the path the borrow arrived over, and on no
/// other.**
///
/// PATH-ACCOUNTING metered the paths and could not reach the one place that
/// knows which of a peer's endpoints a SERVE came in on, so `Ledger::debit`
/// charged the meter with no path and every token was unattributed. The wiring
/// is one `note_lease_path` before the debit, inside the same lock, and this is
/// its gate.
///
/// A TOKENS lease on purpose. `Ledger::tokens_of` answers `None` for a
/// `Fraction` by design (the tokens a share of a window buys is a fact about
/// upstream's pricing that nothing on this Mac holds), so a fraction lease
/// charges nothing whatever the meter is told and a gate written on one would
/// pass with its own subject deleted.
///
/// # Why this is asked of the ledger and not of a served borrow
///
/// The parked two-process version of this test cannot be landed against this
/// build, and finding out why is the finding rather than the obstacle:
/// `Ledger::may_relay` refuses every `LeaseUnit::Tokens` lease with
/// `LeaseRefusal::Unsupported` before a relay is ever entered
/// (`fraction_budget` answers `None` for that unit), and `clamp_to_grant`
/// mints fractions only. So no served request can carry a tokens lease, the
/// per-path token branch in `Ledger::debit` is unreachable from the serve path
/// in this build, and a two-process test of it refuses at the lender with
/// `Unsupported`: measured, not inferred. What is reachable, and what this
/// gate holds, is the arithmetic itself: noted path plus tokens unit plus a
/// known grantee charges that path and nothing else.
///
/// Watched red by deleting the `note_lease_path` call below, which is the same
/// deletion the serve-side wiring would suffer: the meter then holds no row for
/// this borrower at all.
#[test]
fn a_leases_tokens_are_charged_to_the_path_it_was_noted_on() {
    let borrower = PeerId([0xb0_u8; 32]);
    let arrived_over: std::net::SocketAddr = "127.0.0.1:9901".parse().expect("a literal address");
    let path = teamclaude_rs::peer::config::Locator::Direct { addr: arrived_over };
    let now = teamclaude_rs::now_ms();

    let rows_for = |peer: PeerId| {
        let mut meter = teamclaude_rs::peer::tunnel::path_meter()
            .lock()
            .expect("the meter lock");
        meter
            .rows(teamclaude_rs::now_ms())
            .into_iter()
            .filter(|row| row.peer == peer)
            .collect::<Vec<_>>()
    };
    assert!(
        rows_for(borrower).is_empty(),
        "the control: this borrower's key is this test's own, so a row afterwards is this \
         charge and not another test's"
    );

    let mut ledger = Ledger::new();
    ledger.record_scoped(
        token_lease_at(0xb0, 1_000, now),
        borrower,
        tcr_peer_wire::LendScope::All,
    );
    // The line under test. Before the debit, so the tokens land on the path the
    // borrow arrived over rather than on the path of the borrow before it.
    ledger.note_lease_path(0xb0, path);
    let charged = ledger.debit(0xb0, REQUEST, 0.10);
    assert!(
        charged > 0.0,
        "the debit has to charge something, or the token arithmetic below is never reached"
    );

    let rows = rows_for(borrower);
    assert!(
        rows.iter()
            .any(|row| row.tokens_last_hour > 0 && row.locator == path),
        "the tokens have to land on the endpoint this borrow arrived over, or the per-path \
         figure is blank for every lease this Mac serves: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| row.locator == path),
        "and on no other path: {rows:?}"
    );
}
