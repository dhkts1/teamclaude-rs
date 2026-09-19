//! The sealed link one Mac sends one friend after it changed networks: the
//! key, the framing, the round trip through base32, and every refusal a reader
//! can answer with.
//!
//! # Nothing here touches a network or a file
//!
//! There is no listener, no temp file and no store in this file. It is pure
//! computation over fixed inputs, so it cannot reach the proxy on
//! `127.0.0.1:3456`, the operator's config directory, or anything else outside
//! the test process.
//!
//! # The values are obviously synthetic
//!
//! Every secret is a counting pattern, every peer id is one repeated byte and
//! every address is from a documentation range (RFC 5737 TEST-NET-1 and
//! TEST-NET-2, RFC 3849 for the v6 one). None of it is anybody's.

use std::net::SocketAddr;

use tcr_peer_wire::{decode_bytes, PeerId};
use teamclaude_rs::peer::config::{
    Allow, Endpoint, EndpointSource, PeerRow, MAX_ENDPOINTS_PER_PEER,
};
use teamclaude_rs::peer::discovery::{admissible_moved_endpoints, MAX_MOVED_ENDPOINTS_PER_PEER};
use teamclaude_rs::peer::moved::{
    open_link, open_link_field, seal_link, seal_link_field, MintRefusal, MovedKeys, MovedRecord,
    MovedRefusal, MAX_MOVED_AGE, MOVED_VERSION,
};

/// A peer id that is plainly not a real one: 32 copies of one byte.
fn peer_id(fill: u8) -> PeerId {
    PeerId([fill; 32])
}

/// The pair secret these tests run on: bytes 0 through 31.
fn counting_secret() -> [u8; 32] {
    let mut secret = [0_u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        // The cast is exact: the array is 32 long and 31 fits in a u8.
        *byte = index as u8;
    }
    secret
}

/// The keys for a row that HAS a shared secret. Every test that is not about
/// the absent case goes through here.
fn keys_for(secret: &[u8; 32]) -> MovedKeys {
    MovedKeys::for_row(Some(secret)).expect("a row holding a secret has keys")
}

fn address(text: &str) -> SocketAddr {
    text.parse()
        .expect("a literal socket address from a documentation range")
}

/// The moment every test in this file calls "now" unless it says otherwise.
const NOW: u64 = 1_758_240_000;

fn record_at(at: u64) -> MovedRecord {
    MovedRecord {
        v: MOVED_VERSION,
        at,
        eps: vec![address("192.0.2.7:41234")],
    }
}

/// A link is tried against every row this Mac pinned, and the rows it does not
/// belong to must all answer with one sentence.
///
/// Three wrong rows and not one, because the three ways to be wrong are
/// different mechanisms: the wrong pair secret is a wrong key, the wrong
/// publisher is the right key over associated data this reader built from its
/// own row, and both wrong is what a stranger's Mac actually holds. If any of
/// them produced a different refusal, the number of rows a Mac holds and how
/// close one of them came would be readable off the sentence, which is what
/// a link forwarded into the wrong group chat must never teach.
///
/// The first assert is the positive control: the right row DOES open it. Every
/// refusal below is only meaningful because sealing and opening work at all.
#[test]
fn a_link_from_a_pair_this_mac_is_not_in_opens_against_no_row() {
    let friend = peer_id(0x11);
    let stranger = peer_id(0x22);
    let ours = keys_for(&counting_secret());
    let theirs = keys_for(&[0xee; 32]);

    let record = record_at(NOW);
    let sealed = seal_link(&ours, &friend, &record).expect("the record seals for a pinned row");

    let opened = open_link(&ours, &friend, &sealed, NOW + 10)
        .expect("the row the link was sealed for must open it");
    assert_eq!(opened, record, "the record survives the round trip");

    let wrong_rows = [
        ("a different pair secret", &theirs, &friend),
        ("the same secret, another peer", &ours, &stranger),
        ("a stranger's Mac entirely", &theirs, &stranger),
    ];

    let mut sentences = Vec::new();
    for (what, keys, publisher) in wrong_rows {
        let refusal = open_link(keys, publisher, &sealed, NOW + 10)
            .expect_err("a row this link was not sealed for must not open it");
        assert_eq!(
            refusal,
            MovedRefusal::NotForThisMac,
            "{what} must answer with the one quiet refusal, not its own diagnosis"
        );
        sentences.push(refusal.to_string());
    }

    let first = sentences.first().expect("three rows were tried");
    for sentence in &sentences {
        assert_eq!(
            sentence, first,
            "every wrong row must answer with the SAME sentence, or the sentence \
             tells a stranger how close this Mac came to a match"
        );
    }

    // And the sentence names nothing: no peer, no address, no key.
    let sentence = first.clone();
    for secret in [friend.to_wire(), stranger.to_wire(), friend.display()] {
        assert!(
            !sentence.contains(&secret),
            "the refusal must not name a peer: {sentence}"
        );
    }
    assert!(
        !sentence.contains("192.0.2"),
        "the refusal must not name an address: {sentence}"
    );
}

/// A link that sat in a chat window too long is refused BY AGE, with the
/// numbers, and never as a forgery.
///
/// The mechanism under test is where `at` lives. It is inside the seal and it
/// is not in the associated data, precisely so that a reader can authenticate
/// the bytes first and then say "this is too old". Bind `at` into the
/// associated data instead and a link one second past the ceiling becomes
/// indistinguishable from bytes somebody forged, and the sentence a person
/// reads stops telling them the one thing they can act on: ask for a new one.
///
/// The ceiling is read off the constant, never written here as a number, so a
/// ruling that changes how long a link stays good changes one line in the
/// module and this test keeps meaning what it says.
#[test]
fn a_link_past_the_age_ceiling_is_refused_by_age_and_not_by_the_seal() {
    let friend = peer_id(0x11);
    let keys = keys_for(&counting_secret());
    let ceiling = MAX_MOVED_AGE.as_secs();

    // An hour past the ceiling, whatever the ceiling is.
    let over_by = 3_600;
    let sealed = seal_link(&keys, &friend, &record_at(NOW - ceiling - over_by))
        .expect("an old record still seals; age is the reader's decision");

    let refusal = open_link(&keys, &friend, &sealed, NOW)
        .expect_err("a link past the age ceiling must be refused");
    match refusal {
        MovedRefusal::Stale { age_s, max_s } => {
            assert_eq!(max_s, ceiling, "the refusal names the ceiling it applied");
            assert_eq!(
                age_s,
                ceiling + over_by,
                "the refusal names the age it read"
            );
        }
        other => panic!(
            "expected a staleness refusal, got {other}; a link that fails the SEAL \
             because of its age is one nobody can act on"
        ),
    }

    // The boundary itself is inside: a link exactly at the ceiling opens, so
    // the refusal above is the ceiling firing and not an off-by-a-long-way.
    let at_ceiling = seal_link(&keys, &friend, &record_at(NOW - ceiling))
        .expect("a record at the ceiling seals");
    open_link(&keys, &friend, &at_ceiling, NOW)
        .expect("a link exactly at the age ceiling is still good");
}

/// A row with no shared secret gets a named refusal with the remedy in it, and
/// no link at all.
///
/// This is the one test here guarding against a total break rather than a bad
/// message. `PeerRow::rendezvous_secret` is an `Option` whose own doc says
/// absent is not an error, so the tempting line is
/// `unwrap_or_default()`, which seals under thirty-two zero bytes: a key
/// anybody can guess, on a link that looks exactly like a good one. The second
/// half of this test is what makes the first half worth having, by showing that
/// the zero key really is a different key and therefore really would have been
/// a break.
#[test]
fn minting_for_a_row_with_no_shared_secret_refuses_by_name() {
    let refusal = MovedKeys::for_row(None).expect_err("a row with no shared secret cannot mint");
    assert!(
        matches!(refusal, MintRefusal::NoSharedSecret),
        "the refusal must name the missing secret, not something generic: {refusal}"
    );

    let sentence = refusal.to_string();
    assert!(
        sentence.contains("session"),
        "the refusal must carry the remedy, or a person reads it as a broken app: {sentence}"
    );

    // There is no link to print: the refusal is the whole of the return value,
    // and `MovedKeys` cannot be built any other way, so no caller can reach a
    // seal from here.
    assert!(
        MovedKeys::for_row(None).is_err(),
        "nothing is produced for a row with no secret"
    );

    // And the default an implementation might reach for is a DIFFERENT key, so
    // a link sealed under it opens for nobody holding the real row.
    let friend = peer_id(0x11);
    let zeroes = keys_for(&[0_u8; 32]);
    let real = keys_for(&counting_secret());
    let sealed_under_zeroes =
        seal_link(&zeroes, &friend, &record_at(NOW)).expect("the zero key seals like any other");
    let refusal = open_link(&real, &friend, &sealed_under_zeroes, NOW + 10)
        .expect_err("the row's real key must not open a link sealed under the default");
    assert_eq!(
        refusal,
        MovedRefusal::NotForThisMac,
        "the guessable key is not this pair's key"
    );

    // Debug on the keys prints no key material.
    assert_eq!(format!("{real:?}"), "MovedKeys(set)");
}

/// The wire form carries a record of any length, including the lengths that are
/// not a whole number of base32 symbols.
///
/// A framed link is magic, a version, a nonce and a sealed body, so its length
/// is whatever the body came to. The codec this wire is built on never sets
/// `spec.padding`, so it is unpadded and the last symbol carries the spare
/// bits, and whether it accepts that on the way back is an arithmetic claim
/// this test settles rather than assumes. If it did not, every link whose
/// length is not a multiple of five would be unreadable and nothing above this
/// layer would say why.
#[test]
fn a_record_whose_length_is_not_a_multiple_of_five_round_trips() {
    let friend = peer_id(0x11);
    let keys = keys_for(&counting_secret());

    let address_sets = [
        vec![],
        vec![address("192.0.2.7:41234")],
        vec![address("192.0.2.7:41234"), address("198.51.100.9:7755")],
        vec![
            address("192.0.2.7:41234"),
            address("198.51.100.9:7755"),
            address("[2001:db8::1]:41234"),
        ],
        vec![
            address("192.0.2.7:41234"),
            address("198.51.100.9:7755"),
            address("[2001:db8::1]:41234"),
            address("[2001:db8::2]:7755"),
        ],
    ];

    let mut lengths = Vec::new();
    for eps in address_sets {
        let record = MovedRecord {
            v: MOVED_VERSION,
            at: NOW,
            eps,
        };

        let sealed = seal_link(&keys, &friend, &record).expect("the record seals");
        lengths.push(sealed.len());

        let field = seal_link_field(&keys, &friend, &record).expect("the record seals to a field");
        assert!(
            !field.contains('='),
            "the wire form is unpadded, so a link never carries a character a chat \
             client could treat as the end of a URL: {field}"
        );

        let decoded = decode_bytes(&field).expect("the field decodes back");
        // Not the same bytes as `sealed`: each seal draws its own nonce. What
        // must match is the length and, below, the record itself.
        assert_eq!(
            decoded.len(),
            sealed.len(),
            "the field decodes to a link of the same length"
        );

        let opened = open_link_field(&keys, &friend, &field, NOW + 10)
            .expect("a link that went through the wire form opens");
        assert_eq!(opened, record, "the record survives encode and decode");
    }

    // The premise: at least one of those lengths really is not a multiple of
    // five, or this test proved nothing about trailing bits.
    assert!(
        lengths.iter().any(|len| !len.is_multiple_of(5)),
        "none of the framed lengths {lengths:?} tests the case this is named for"
    );
}

/// A link a chat app cut in half is refused as a cut paste, not as a forgery.
///
/// The mechanism under test is WHERE the length is decided. It runs before the
/// AEAD, so bytes that could not hold a link are answered with a sentence about
/// the paste. Move that check after the AEAD and every truncation becomes "that
/// link was not meant for this Mac", which sends a person who pasted badly to
/// go and ask their friend whether they sent the right one.
///
/// This sweeps every truncation of one real link rather than one cut, because a
/// single cut only tests the length it happens to land on.
///
/// What the sweep also measures, and what nobody has ruled on yet: a cut that
/// still leaves enough bytes to hold a nonce and a tag CANNOT be told from a
/// forgery, because the AEAD is the only thing that would know. Those answer
/// `NotForThisMac`. The counts are asserted below so the bound is written down
/// rather than discovered later.
#[test]
fn a_link_cut_short_by_a_chat_app_is_refused_as_a_cut_paste() {
    let friend = peer_id(0x11);
    let keys = keys_for(&counting_secret());

    let field = seal_link_field(&keys, &friend, &record_at(NOW)).expect("the record seals");
    assert!(
        open_link_field(&keys, &friend, &field, NOW + 10).is_ok(),
        "the whole link opens, which is what makes every cut below a cut"
    );

    // The smallest link that could hold anything, read off the module's own
    // refusal rather than written here: one byte short of it must be a cut
    // paste and not a forgery.
    let whole = decode_bytes(&field).expect("the field decodes");
    let mut counts = (0_usize, 0_usize);
    for cut in 1..field.len() {
        let Some(shorter) = field.get(..cut) else {
            panic!("a base32 field is ASCII, so every byte offset is a character offset");
        };
        let refusal = open_link_field(&keys, &friend, shorter, NOW + 10)
            .expect_err("a cut link must never open");

        let decoded_bytes = decode_bytes(shorter).map(|bytes| bytes.len());
        match refusal {
            MovedRefusal::CutShort => counts.0 += 1,
            MovedRefusal::NotForThisMac => {
                counts.1 += 1;
                let decoded = decoded_bytes.unwrap_or_else(|_| {
                    panic!("a cut that reached the seal must have decoded: {cut} characters")
                });
                assert!(
                    decoded >= crate_min_link_bytes(),
                    "a cut that reached the seal held {decoded} bytes, fewer than the \
                     {} a link needs: the length check ran too late",
                    crate_min_link_bytes()
                );
            }
            other => panic!(
                "a cut link must answer about the paste or about the seal and nothing \
                 else, got {other} at {cut} characters"
            ),
        }
    }

    let (cut_short, indistinguishable) = counts;
    assert!(
        cut_short > 0,
        "no cut of this link was caught as a cut paste, so the length check is not \
         running before the seal"
    );
    assert!(
        indistinguishable > 0,
        "every cut was caught as a cut paste, which this format cannot do: a cut that \
         leaves a nonce and a tag is a forgery as far as the AEAD can tell, and a run \
         that says otherwise is measuring something else"
    );

    // And the same thing one layer down, on bytes rather than on the field: a
    // link cut below the frame minimum is a cut paste whatever it decoded from.
    for len in 0..(crate_min_link_bytes()) {
        let Some(shorter) = whole.get(..len) else {
            break;
        };
        let refusal = open_link(&keys, &friend, shorter, NOW + 10)
            .expect_err("bytes shorter than a link must never open");
        assert_eq!(
            refusal,
            MovedRefusal::CutShort,
            "{len} bytes is shorter than any link, so it is a cut paste"
        );
    }
}

// ---------------------------------------------------------------------------
// Where an address off a link lands on the row, and how hard it is believed
// ---------------------------------------------------------------------------

/// A row with no endpoints, for the two tests about what a link may write.
fn pinned_row(node: PeerId) -> PeerRow {
    PeerRow {
        node,
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: ROW_MS,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    }
}

/// A clock that is not the machine's, so these two tests read the same on every
/// run. 2026-01-01T00:00:00Z.
const ROW_MS: i64 = 1_767_225_600_000;

/// An address in a documentation range, one per port.
fn row_addr(port: u16) -> SocketAddr {
    address(&format!("198.51.100.7:{port}"))
}

/// **An address off a pasted link never evicts one a handshake proved, never
/// re-dates it, and never rewrites it as a link's.**
///
/// A link is the weakest thing that can reach the endpoint list, beside a
/// dead-drop record: it is sealed under a key a Mac this one has since
/// forgotten still holds, and the operator's confirmation authorizes the write
/// rather than the address. So it gets the three rules the other weak bands
/// get, with its own cap, and this says they hold for the new band rather than
/// only for the two older ones.
///
/// Four claims, each a different way the rules could be wrong:
///
/// 1. a full row admits nothing at all, however many addresses the link
///    carried;
/// 2. a locator the row holds from a stronger source is refused, not re-dated
///    and not rewritten as a link's, which is invisible in a count of
///    endpoints;
/// 3. a locator the row already holds from a link is refreshed, evicting
///    nothing;
/// 4. the third new address off a link is refused while the row still has free
///    slots, which is the cap firing and not the row's own eight.
///
/// Watched red, one mutation at a time: pointing
/// `discovery::admissible_moved_endpoints` at `MAX_ENDPOINTS_PER_PEER` instead
/// of its own cap fails claim 4, and pointing it at `EndpointSource::Drop`
/// fails claim 3, because a locator held as a link stops counting as this
/// band's own and is refused instead of refreshed.
#[test]
fn a_moved_endpoint_never_evicts_one_a_handshake_proved() {
    let peer = peer_id(0xA1);

    // The four a record may carry, so the caps below are what bounds the write
    // rather than the size of the list handed in.
    let carried: Vec<Endpoint> = (9_800_u16..9_804)
        .map(|port| Endpoint::direct(row_addr(port), ROW_MS + 1_000, EndpointSource::Moved))
        .collect();

    // 1. A full row: two endpoints the pairing proved and six a session did.
    let mut full = pinned_row(peer);
    full.observe_endpoint(Endpoint::direct(
        row_addr(9_700),
        ROW_MS,
        EndpointSource::Paired,
    ));
    full.observe_endpoint(Endpoint::direct(
        row_addr(9_701),
        ROW_MS,
        EndpointSource::Paired,
    ));
    for port in (9_702_u16..).take(MAX_ENDPOINTS_PER_PEER - 2) {
        full.observe_endpoint(Endpoint::direct(
            row_addr(port),
            ROW_MS,
            EndpointSource::Hello,
        ));
    }
    assert_eq!(
        full.endpoints.len(),
        MAX_ENDPOINTS_PER_PEER,
        "the row this claim needs is a full one: {:?}",
        full.endpoints
    );
    assert!(
        admissible_moved_endpoints(&full, &carried).is_empty(),
        "a full row admits nothing off a link: every slot holds an address this Mac \
         proved, and a link's endpoint carries this Mac's clock, so unbounded it would \
         lead them all"
    );
    let proven_count = full
        .endpoints
        .iter()
        .filter(|endpoint| endpoint.source == EndpointSource::Paired)
        .count();
    assert_eq!(
        proven_count, 2,
        "the two the pairing proved are still on the row, unchanged: {:?}",
        full.endpoints
    );

    // 2. A locator the row already holds from a stronger source.
    let mut held_stronger = pinned_row(peer);
    held_stronger.observe_endpoint(Endpoint::direct(
        row_addr(9_700),
        ROW_MS,
        EndpointSource::Paired,
    ));
    let same_locator = vec![Endpoint::direct(
        row_addr(9_700),
        ROW_MS + 1_000,
        EndpointSource::Moved,
    )];
    assert!(
        admissible_moved_endpoints(&held_stronger, &same_locator).is_empty(),
        "a locator the pairing proved is left alone: refreshing it would re-date it and \
         rewrite its source as the weakest band there is"
    );

    // 3. The same locator, already held from a link: refreshed, evicting
    //    nothing.
    let mut held_as_moved = pinned_row(peer);
    held_as_moved.observe_endpoint(Endpoint::direct(
        row_addr(9_900),
        ROW_MS,
        EndpointSource::Moved,
    ));
    let refresh = vec![Endpoint::direct(
        row_addr(9_900),
        ROW_MS + 1_000,
        EndpointSource::Moved,
    )];
    let admissible = admissible_moved_endpoints(&held_as_moved, &refresh);
    assert_eq!(
        admissible, refresh,
        "an address this row already holds from a link is refreshed rather than refused, \
         which is what makes a second paste of the same link cost nothing: {admissible:?}"
    );

    // 4. The cap, with room to spare on the row itself.
    let mut at_cap = pinned_row(peer);
    for port in (9_910_u16..).take(MAX_MOVED_ENDPOINTS_PER_PEER) {
        at_cap.observe_endpoint(Endpoint::direct(
            row_addr(port),
            ROW_MS,
            EndpointSource::Moved,
        ));
    }
    assert!(
        at_cap.endpoints.len() < MAX_ENDPOINTS_PER_PEER,
        "the row still has free slots, so this claim is the cap and not the row's own \
         limit: {:?}",
        at_cap.endpoints
    );
    let next = vec![Endpoint::direct(
        row_addr(9_920),
        ROW_MS + 1_000,
        EndpointSource::Moved,
    )];
    assert!(
        admissible_moved_endpoints(&at_cap, &next).is_empty(),
        "at most {MAX_MOVED_ENDPOINTS_PER_PEER} of the eight slots are an address off a \
         link, so the rest stay with what this Mac proved"
    );

    // And the cap really is what a whole record runs into: four addresses at
    // once onto an empty row writes two.
    let empty = pinned_row(peer);
    assert_eq!(
        admissible_moved_endpoints(&empty, &carried).len(),
        MAX_MOVED_ENDPOINTS_PER_PEER,
        "one link carrying four addresses writes at most the cap"
    );
}

/// **An address off a link is dialled behind a friend's word and beside a
/// record off a dead drop.**
///
/// The two sealed bands rank together because they are the same evidence: a key
/// derived from the pair's rendezvous secret, which a Mac this one has since
/// forgotten still holds. Below a brief, which came over a session that proved
/// a static key, from a Mac this node pinned, about a Mac it also pinned.
///
/// Asserted as the whole order over all seven sources rather than as one
/// comparison: "a link sorts after a brief" passes just as happily if the new
/// band swallowed one of the four above it.
///
/// The link's endpoint is the NEWEST on the row, which is what gives this test
/// teeth in both directions. Recency is the last key, so a link that wrongly
/// tied the brief's band would be dialled BEFORE the brief, and one given a
/// band of its own below the drop would be dialled after it. Either way the
/// order asserted below is the one that breaks.
///
/// Watched red: giving [`EndpointSource::Moved`] the brief's rank (`2`) in
/// `probe::source_rank` moves the link's address from index 5 to index 4, and
/// giving it a band of its own (`4`) swaps it with the drop's.
#[test]
fn a_moved_endpoint_sorts_behind_a_brief_and_beside_a_drop() {
    use teamclaude_rs::peer::probe::{self, PathTable};

    let port_of = |source: EndpointSource| -> u16 {
        match source {
            EndpointSource::Paired => 9_721,
            EndpointSource::Hello => 9_722,
            EndpointSource::Mapping => 9_723,
            EndpointSource::Beacon => 9_724,
            EndpointSource::Brief => 9_725,
            EndpointSource::Drop => 9_726,
            EndpointSource::Moved => 9_727,
        }
    };
    let addr_of = |source: EndpointSource| -> SocketAddr { row_addr(port_of(source)) };

    // Newest first, which is the order the source key has to overturn, and the
    // link's address leads it.
    let newest_first = [
        EndpointSource::Moved,
        EndpointSource::Drop,
        EndpointSource::Brief,
        EndpointSource::Beacon,
        EndpointSource::Mapping,
        EndpointSource::Hello,
        EndpointSource::Paired,
    ];
    let mut row = pinned_row(peer_id(0x16));
    row.endpoints = newest_first
        .iter()
        .enumerate()
        .map(|(nth, source)| {
            let age_ms = i64::try_from(nth).expect("seven endpoints fit") * 60_000;
            Endpoint::direct(addr_of(*source), ROW_MS - age_ms, *source)
        })
        .collect();

    let ordered: Vec<SocketAddr> = probe::order_endpoints(&row, &PathTable::default())
        .into_iter()
        .filter_map(|endpoint| endpoint.direct_addr())
        .collect();

    // Paired and Hello share a band, as do Mapping and Beacon, and so now do
    // Drop and Moved: within each the newer one leads, which is the recency
    // tiebreak and not a band of its own.
    let expected = vec![
        addr_of(EndpointSource::Hello),
        addr_of(EndpointSource::Paired),
        addr_of(EndpointSource::Beacon),
        addr_of(EndpointSource::Mapping),
        addr_of(EndpointSource::Brief),
        addr_of(EndpointSource::Moved),
        addr_of(EndpointSource::Drop),
    ];
    assert_eq!(
        ordered, expected,
        "the whole dial order, weakest evidence last: what a handshake proved, then this \
         Mac's own hints, then a friend's word, then the two sealed under a key a \
         forgotten peer still holds"
    );
}

/// The frame minimum, derived the way the module derives it: four bytes of
/// magic, a version byte, a twelve-byte nonce, a sixteen-byte tag and one byte
/// under the tag.
///
/// Spelled here rather than imported because it is the module's own private
/// arithmetic, and a test that imported it could not tell a changed minimum
/// from a broken one.
fn crate_min_link_bytes() -> usize {
    4 + 1 + 12 + 16 + 1
}
