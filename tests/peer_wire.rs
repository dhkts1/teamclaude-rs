//! Phase 1's gates: the wire contract, and the two properties a mixed-build LAN
//! depends on.
//!
//! # Every gate here is LIVE
//!
//! Four of these shipped live at once (the source of the wire crate, the
//! frame codec, and the serde conversions that carry forward compatibility)
//! and left `unknown_stream_kind_is_refused_not_ignored` `#[ignore]`d on the
//! production body it measures (`peer_stream_gate_rows`), which phase 2 had
//! not landed yet. A later merge brought that body into this tree, so the
//! last gate un-ignores and stays green: `cargo test -- --ignored --list`
//! prints nothing from this file.

use tcr_peer_wire::{
    Control, LabelRefusal, LeaseUnit, PeerId, StreamHeader, StreamKind, Window, MAX_LABEL_BYTES,
};
use teamclaude_rs::peer::listener::{peer_stream_gate_rows, StreamRefusal};

/// The invariant the whole design rests on: **no credential field on the peer
/// wire, ever.**
///
/// **This one is a denylist, and testing measured what gets past it.** It
/// is kept because it reads the SOURCE and so can refuse a spelling before any
/// value of the type exists, but it is no longer the gate that carries the
/// invariant: `every_wire_type_serializes_only_allowlisted_keys` (below) is,
/// and `tests/tools/mutate-wire-gate.sh` prints the three credential fields this
/// test stays green on. Read that script's output before trusting this test.
///
/// A source grep rather than a type assertion, because the thing being
/// forbidden is a field nobody has written yet: there is no type to ask. It
/// reads the wire crate's own source and refuses six spellings.
///
/// Carries its own POSITIVE CONTROL: it first asserts the grep finds a token
/// that IS there. Without that, a failed read, a moved file or a renamed crate
/// would print "clean" and this gate would pass by measuring nothing.
///
/// # The one exemption, and why it is a SPAN and not a word
///
/// As of 2026-09-18, exactly one credential is on this wire:
/// `Control::Handoff`'s `access_token`, for a `hand`-mode grant. The cheap way
/// to make this test green again would be to drop `access_token` from the
/// denylist, and that would also green `access_token` on `Enroll`, on
/// `Hello`, on anything. So the exemption is scoped to the SPAN of the
/// `Handoff` variant's declaration, found by brace-matching in the source
/// below: inside that span `access_token` is permitted and every other
/// spelling still fails; outside it nothing changed.
///
/// Watch it fail three ways, all three run by hand before this shipped:
/// (a) add `refresh_token: String` to `Handoff`; (b) add
/// `pub access_token: String` to `Enroll`; (c) add any listed spelling to any
/// struct in the crate.
#[test]
fn wire_has_no_credential_field() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/tcr-peer-wire/src/lib.rs"
    ))
    .expect("the wire crate's source is readable from the workspace root");

    // CODE only. The comments in that file have to be able to EXPLAIN what is
    // forbidden: "no Bearer, no api-key header" is the sentence a reader
    // needs: and a grep that cannot tell prose from a field declaration would
    // make writing that sentence the thing it blocks.
    let code_lines: Vec<String> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| line.to_lowercase())
        .collect();

    // The exempt span: the `Handoff` variant's declaration, from the line that
    // opens it to the line that closes its brace. Brace-matched rather than
    // "the next N lines", because a field added to the variant must land
    // INSIDE the span this test measures. Otherwise (a) below would pass for
    // the wrong reason, by falling out of a span that stopped too early.
    let start = code_lines
        .iter()
        .position(|line| line.trim_start().starts_with("handoff {"))
        .expect(
            "positive control failed: no `Handoff {` declaration in the wire crate, so the \
             scoped exemption below is measuring nothing. If the variant was renamed or \
             removed, this test's span must move with it.",
        );
    let mut depth = 0_i32;
    let mut end = None;
    for (offset, line) in code_lines[start..].iter().enumerate() {
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if depth <= 0 {
            end = Some(start + offset);
            break;
        }
    }
    let end = end.expect("the `Handoff` declaration's braces balance before end of file");

    let inside: String = code_lines[start..=end].join("\n");
    let outside: String = code_lines[..start]
        .iter()
        .chain(code_lines[end + 1..].iter())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");

    // Whole IDENTIFIERS, not substrings: a bare `.contains("token")` would
    // fail on `LeaseUnit::Tokens` and the `"tokens"` unit string today, before
    // any credential field is ever added: a gate that reds on its own clean
    // input is not a gate. Splitting on everything that is not
    // ASCII-alphanumeric-or-underscore turns `pub join_key: String` into the
    // identifiers `pub`, `join_key`, `string` and lets `token` mean the word
    // `token`, never a prefix of `tokens`.
    fn identifiers(code: &str) -> std::collections::HashSet<&str> {
        code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| !word.is_empty())
            .collect()
    }
    let outside_identifiers = identifiers(&outside);
    let inside_identifiers = identifiers(&inside);

    // Positive controls, one per region. Without them a failed read, a moved
    // file, an over-eager filter or a span that matched the wrong line would
    // print "clean" and this gate would pass by measuring nothing.
    assert!(
        outside.contains("pub struct peerid"),
        "positive control failed: the OUTSIDE region holds no code from the wire crate, \
         so its absence findings below prove nothing"
    );
    assert!(
        inside_identifiers.contains("access_token"),
        "positive control failed: the exempt span does not contain `access_token`, so the \
         exemption is either vacuous or aimed at the wrong lines. Span read:\n{inside}"
    );
    let declarations = code_lines
        .iter()
        .filter(|line| line.trim_start().starts_with("handoff {"))
        .count();
    assert_eq!(
        declarations, 1,
        "positive control failed: {declarations} `Handoff {{` declarations in the wire crate. \
         This test exempts exactly one span, so a second declaration is a credential field \
         nothing is measuring."
    );

    const FORBIDDEN: [&str; 9] = [
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "bearer",
        "authorization",
        "token",
        "secret",
        "join_key",
    ];

    // Outside the span: nothing changed, all nine spellings are refused.
    for forbidden in FORBIDDEN {
        // Lower-cased on both sides: a credential field named `Authorization`
        // is the same defect as one named `authorization`, and a case-sensitive
        // grep is how that ships.
        assert!(
            !outside_identifiers.contains(forbidden),
            "the peer wire must carry no credential field, and {forbidden:?} appears \
             as an identifier in the CODE of crates/tcr-peer-wire/src/lib.rs, OUTSIDE the \
             one exempt declaration (`Control::Handoff`). A token on the wire makes every \
             forwarding hop a disclosure boundary. Watch this fail by adding \
             `pub join_key: String` to `Enroll`."
        );
    }

    // Inside the span: `access_token` is the whole exception, by name. Every
    // other spelling is as forbidden here as anywhere else: a refresh token
    // on a hand-mode grant would hand over the account itself, which is the
    // one thing the owner keeps.
    for forbidden in FORBIDDEN.iter().filter(|word| **word != "access_token") {
        assert!(
            !inside_identifiers.contains(forbidden),
            "`Control::Handoff` is exempted for `access_token` and for nothing else, and \
             {forbidden:?} appears inside its declaration. This hands the borrower a \
             SHORT-LIVED bearer and keeps the refresh token on the owner's Mac; a second \
             credential spelling here is a different decision and needs its own row."
        );
    }
}

/// A newer build on the same LAN will name a stream kind this one does not
/// know. The PARSE must survive that: and the handler must still refuse it.
///
/// Both halves, in one test, because they are one decision: a kind that parses
/// and is then treated as a tunnel is an open relay, and a kind that fails the
/// parse takes the whole connection down for a value that was only ever
/// informational.
///
/// Watch it fail by making `peer_stream_gate_rows` accept `StreamKind::Unknown`
/// (e.g. by moving the unknown-kind check after the row lookup and letting a
/// pinned peer's grants decide it), and separately by deleting the
/// `Unknown(u16)` arm so the parse cannot represent it.
///
/// Live now that all of this code is merged into this tree:
/// `peer_stream_gate_rows` (`src/peer/listener.rs`) is real, and this test
/// needs no pin store: `row: None` is the unpinned case, and the
/// unknown-kind check runs BEFORE the row is even read, so both an unpinned
/// AND a pinned stranger get the same refusal.
#[test]
fn unknown_stream_kind_is_refused_not_ignored() {
    // The parse survives, today. This half needs no production body.
    assert_eq!(StreamKind::from(9_999_u16), StreamKind::Unknown(9_999));

    // And the handler refuses it, before it ever consults a pin.
    let header = StreamHeader {
        kind: StreamKind::Unknown(9_999),
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 1,
    };
    assert_eq!(
        peer_stream_gate_rows(&header, None),
        Err(StreamRefusal::UnknownKind(9_999))
    );
}

/// Every enum on this wire has an unknown arm, and an unknown value must land
/// in it rather than failing the whole message.
///
/// Live: this measures the serde derives, which are real today.
#[test]
fn an_unknown_window_or_unit_parses_into_its_unknown_arm() {
    let window: Window = serde_json::from_str("\"90d\"").expect("an unknown window still parses");
    assert_eq!(window, Window::Unknown);

    let unit: LeaseUnit = serde_json::from_str("{\"unit\":\"goats\",\"amount\":3}")
        .expect("an unknown lease unit still parses");
    assert_eq!(unit, LeaseUnit::Unknown);

    let control: Control = serde_json::from_str("{\"type\":\"teleport\"}")
        .expect("an unknown control message still parses");
    assert_eq!(control, Control::Unknown);

    // And the known values still mean what they say, so the catch-all is not
    // swallowing everything: the failure mode a lone unknown-arm test cannot
    // see.
    let known: Window = serde_json::from_str("\"7d_oi\"").expect("a known window parses");
    assert_eq!(known, Window::SevenDayOi);

    let known_unit: LeaseUnit = serde_json::from_str("{\"unit\":\"fraction\",\"amount\":0.2}")
        .expect("a known lease unit parses");
    assert_eq!(known_unit, LeaseUnit::Fraction(0.2));
    assert_eq!(
        serde_json::to_string(&LeaseUnit::Tokens(1_000)).expect("a unit serializes"),
        "{\"unit\":\"tokens\",\"amount\":1000}",
        "the unit rides the wire as a VALUE a reader can see, not as a shape they infer"
    );

    // An amount that does not fit the unit it claims is also unknown, rather
    // than a silent zero: "tokens: 0.5" is a newer build's arithmetic, not
    // ours.
    let mismatched: LeaseUnit = serde_json::from_str("{\"unit\":\"tokens\",\"amount\":0.5}")
        .expect("a mismatched amount still parses");
    assert_eq!(mismatched, LeaseUnit::Unknown);
}

/// The stream kind's scalar mapping is a contract with every other build on the
/// LAN, so it round-trips and the numbers are pinned.
#[test]
fn stream_kinds_round_trip_through_their_wire_numbers() {
    for (kind, raw) in [
        (StreamKind::Tunnel, 1_u16),
        (StreamKind::Serve, 2),
        (StreamKind::Control, 3),
    ] {
        assert_eq!(
            u16::from(kind),
            raw,
            "{kind:?} must stay on the wire as {raw}"
        );
        assert_eq!(StreamKind::from(raw), kind);
    }
}

/// The frame codec, and the one thing a peer controls about it: the length
/// prefix.
///
/// Live, because the codec is real. The truncation cases are the point: a
/// decoder that indexed before comparing would panic on a short buffer, and a
/// panic on a peer-supplied length is a remote denial of service on a socket
/// that is meant to survive mixed builds and half-open connections.
#[test]
fn a_frame_round_trips_and_a_short_buffer_asks_for_more() {
    let mut out = Vec::new();
    tcr_peer_wire::encode_frame(b"hello", &mut out).expect("a small payload encodes");
    assert_eq!(
        out.len(),
        2 + 5,
        "a u16 prefix and the payload, nothing else"
    );

    let (payload, consumed) = tcr_peer_wire::decode_frame(&out).expect("it decodes back");
    assert_eq!(payload, b"hello");
    assert_eq!(consumed, out.len());

    // A prefix promising more than arrived: ask for more, never panic and never
    // hand back a partial frame.
    assert_eq!(
        tcr_peer_wire::decode_frame(&out[..4]),
        Err(tcr_peer_wire::FrameError::Incomplete)
    );
    // Not even the prefix yet.
    assert_eq!(
        tcr_peer_wire::decode_frame(&out[..1]),
        Err(tcr_peer_wire::FrameError::Incomplete)
    );
    assert_eq!(
        tcr_peer_wire::decode_frame(&[]),
        Err(tcr_peer_wire::FrameError::Incomplete)
    );

    // A payload a Noise transport message cannot carry is refused rather than
    // truncated into a valid-looking shorter frame.
    let too_long = vec![0_u8; tcr_peer_wire::MAX_FRAME_BYTES + 1];
    assert_eq!(
        tcr_peer_wire::encode_frame(&too_long, &mut out),
        Err(tcr_peer_wire::FrameError::TooLong {
            len: tcr_peer_wire::MAX_FRAME_BYTES + 1
        })
    );

    // And the largest legal payload still encodes, so the bound is the protocol
    // limit and not one byte under it.
    let mut biggest = Vec::new();
    let max = vec![0_u8; tcr_peer_wire::MAX_FRAME_BYTES];
    tcr_peer_wire::encode_frame(&max, &mut biggest).expect("the ceiling itself is legal");
    assert_eq!(biggest.len(), 2 + tcr_peer_wire::MAX_FRAME_BYTES);
}

/// The label is the one free-text field that reaches another machine, a beacon
/// and a fixture. **This repository is public**, so the sanitizer refuses an
/// email shape, a uuid shape, an over-length string and an empty one: and it
/// is ONE function, so every entry point refuses them identically.
///
/// `LabelRefusal::OrgName` is gone (no organization-name
/// list exists in this tree, and a variant nobody constructs is a lie), so
/// this test names only the four shapes the sanitizer actually checks.
///
/// Watch it fail by returning `Ok` unconditionally.
#[test]
fn label_sanitizer_refuses_an_email_a_uuid_shape_a_too_long_and_an_empty_label() {
    let too_long = "x".repeat(MAX_LABEL_BYTES + 1);
    for refused in [
        "alice@example.com",
        "11111111-1111-1111-1111-111111111111",
        too_long.as_str(),
        "",
    ] {
        let verdict = tcr_peer_wire::sanitize_label(refused);
        assert!(
            verdict.is_err(),
            "the sanitizer must refuse {refused:?}, and name what it found"
        );
    }

    assert_eq!(
        tcr_peer_wire::sanitize_label("laptop-2").expect("a plain label is accepted"),
        "laptop-2"
    );

    // A canonical uuid is 36 bytes, over MAX_LABEL_BYTES (32): the uuid check
    // must run BEFORE the length cap, or `UuidShape` is never reachable for
    // the one input shape it names and every uuid comes back `TooLong`
    // instead. Watch it fail by swapping the two checks back.
    assert_eq!(
        tcr_peer_wire::sanitize_label("11111111-1111-1111-1111-111111111111"),
        Err(LabelRefusal::UuidShape)
    );
}

/// The display form is one-way on purpose: ten base32 characters is about 50
/// bits, so parsing one back would resolve a truncated id against the pin store
///: a prefix collision with a friendly face.
///
/// Watch it fail by implementing `parse` to accept the `tcr-` short form.
#[test]
fn the_short_display_form_is_never_parsed_back() {
    let id = PeerId([7_u8; 32]);
    let short = id.display();

    assert!(
        short.starts_with("tcr-"),
        "the display form carries its prefix"
    );
    assert!(
        PeerId::parse(&short).is_err(),
        "the truncated display form must not resolve to an identity"
    );
    assert_eq!(
        PeerId::parse(&id.to_wire()).expect("the full wire form round-trips"),
        id
    );
}

// ---------------------------------------------------------------------------
// The strong form of the credential gate: an allowlist of SERIALIZED keys
// ---------------------------------------------------------------------------

/// Every type in the wire crate whose keys `every_wire_type_serializes_only_\
/// allowlisted_keys` below checks.
///
/// **One list, read by two tests, and that is the point.** The allowlist test
/// is exact in both directions for the types it names: but it is silent about
/// a type nobody named, and that is not a theoretical gap: this change added
/// `Knock` and `InstanceId` to the wire crate, and until they were added here
/// the allowlist gate stayed green while a whole new message type went
/// unchecked. `the_allowlist_covers_every_serializable_wire_type` closes it by
/// reading the crate's SOURCE.
const ALLOWLISTED_WIRE_TYPES: &[&str] = &[
    "Caps",
    "CollapseHint",
    "Control",
    "Enroll",
    "Hello",
    // The one credential, in a newtype whose `Debug` redacts it
    // (`HandoffToken`). Transparent on the wire: it is the same bare string
    // `Control::Handoff`'s `accessToken` carried before the type existed, and
    // the scalar block at the end of
    // `every_wire_type_serializes_only_allowlisted_keys` is what says so.
    "HandoffToken",
    "InstanceId",
    "Knock",
    "Lease",
    "LeaseGrant",
    "LeaseReceipt",
    "LeaseRefusal",
    "LeaseRequest",
    "LeaseUnit",
    "Lendable",
    // The lease scope. It is in the wire crate because the lender's
    // file, its CLI and its ledger need one name for it: and it is on NO
    // message: a scope names the lender's own accounts or groups, and the
    // sample below is what asserts that its keys never appear inside one.
    "LendScope",
    "MoveOffer",
    "NeighborBrief",
    "PeerId",
    "StreamHeader",
    "StreamKind",
    "TunnelTarget",
    "Window",
];

/// **Every `pub struct` or `pub enum` in the wire crate that derives
/// `Serialize` is covered by the allowlist, and the source is what says so.**
///
/// # What the allowlist gate cannot see, and this can
///
/// `every_wire_type_serializes_only_allowlisted_keys` is exact about the types
/// in its table: a key added to one of them fails, and a key removed fails too.
/// It says nothing at all about a type that is not in the table. So the way
/// past it is not a cleverly named field: it is a NEW TYPE. Add
///
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// pub struct Probe { pub access_token_blob: String }
/// ```
///
/// to `crates/tcr-peer-wire/src/lib.rs` and every gate in this file stays
/// green: the denylist misses it because `access_token_blob` is not one of its
/// nine spellings, and the allowlist misses it because nobody put `Probe` in
/// the table.
///
/// This test reads the crate's source, collects every `pub struct`/`pub enum`
/// whose preceding `#[derive(...)]` names `Serialize`, and fails for any one
/// that is not in [`ALLOWLISTED_WIRE_TYPES`]. A new serializable wire type is
/// then a red test until somebody adds it on purpose: and adding it on purpose
/// means adding a sample to the table below, which is what makes its keys
/// exact.
///
/// **Watched red, exactly as the brief specifies**: with
/// `pub struct Probe { pub access_token_blob: String }` and a
/// `#[derive(Serialize, Deserialize)]` added to the wire crate, this fails with
///
/// ```text
/// these types derive Serialize in crates/tcr-peer-wire/src/lib.rs and are on
/// no allowlist: ["Probe"]
/// ```
///
/// It carries a POSITIVE CONTROL for the same reason the denylist does: a
/// regex that stopped matching, a moved file or a renamed crate would find
/// nothing and this gate would pass by measuring nothing.
/// The scan behind [`the_allowlist_covers_every_serializable_wire_type`],
/// pulled out so its two evasions (a blank line between the derive and the
/// item; `#[cfg_attr(..., derive(Serialize))]` instead of a bare
/// `#[derive(Serialize)]`) can be exercised directly against a small source
/// string instead of only through the whole wire crate.
///
/// A `#[derive(...)]` or `#[cfg_attr(..., derive(...))]` may wrap across
/// lines, so the scan carries the most recent attribute text forward rather
/// than looking only at the line above. Doc comments, other attributes, and
/// BLANK LINES between the derive and the item are all normal: a doc
/// comment block routinely has a blank line inside it, and rustfmt is happy
/// to leave one before the item: so none of them clear the carried-forward
/// text. Only the item itself (which consumes and clears it) does.
///
/// **Evasion 3**: `#[derive(Serialize, Deserialize)] pub struct X { ... }`
/// all on ONE line closes its attribute and opens its item in the same
/// line of text. Treating "starts an attribute" and "ends with `)]`" as the
/// whole story for a line put the entire line (attribute AND item), into
/// the carried-forward derive text and the item was never seen, so this
/// evaded the scan with `found: []`. The fix treats a line as carrying
/// BOTH roles when it does: close the attribute at its own `)]`, then keep
/// scanning the remainder of the same line for the item.
///
/// **Evasion 4**: a NON-derive attribute leading the same line
/// as the derive: `#[non_exhaustive] #[derive(Serialize, Deserialize)]
/// pub struct X { ... }`: meant the line did not start with `#[derive(` or
/// `#[cfg_attr(`, so the whole line fell through the old anchored check as
/// "normal text" and its `#[derive(...)]` was never recorded at all. The fix
/// stops anchoring on what the line starts with and instead consumes EVERY
/// `#[...]` attribute segment at the front of what remains of the line, in a
/// loop, regardless of which attribute comes first: so a non-derive
/// attribute ahead of the derive on one line no longer hides it.
///
/// **Evasion 5**: `pub(crate) struct` and `pub(super) struct`
/// (and the `enum` forms) are exactly as serializable as a bare `pub
/// struct` (visibility does not change what `derive(Serialize)` does), but
/// the old item match only tried `"pub struct "` / `"pub enum "` and missed
/// both. The item match now tries `pub(crate)`/`pub(super)` before the bare
/// `pub` form.
fn scan_serializable_types(source: &str) -> Vec<String> {
    let mut derives = String::new();
    let mut in_attr = false;
    let mut serializable: Vec<String> = Vec::new();

    for line in source.lines() {
        let mut rest = line.trim();

        // Consume every `#[...]` attribute segment at the front of what is
        // left of this line, in a loop, so a non-derive attribute ahead of a
        // derive attribute on the SAME line (evasion 4) is not enough to
        // hide the derive: each attribute closes at its own `]` and the next
        // `#[` immediately after is picked up by the next iteration, exactly
        // as evasion 3 already required for one attribute plus an item on
        // one line.
        loop {
            if in_attr {
                match rest.find(']') {
                    Some(close) => {
                        derives.push_str(&rest[..close]);
                        derives.push(' ');
                        in_attr = false;
                        rest = rest[close + 1..].trim_start();
                        continue;
                    }
                    None => {
                        derives.push_str(rest);
                        derives.push(' ');
                        rest = "";
                        break;
                    }
                }
            }

            match rest.strip_prefix("#[") {
                Some(stripped) => {
                    in_attr = true;
                    rest = stripped;
                }
                None => break,
            }
        }

        if rest.is_empty() {
            continue;
        }

        let item = rest
            .strip_prefix("pub(crate) struct ")
            .or_else(|| rest.strip_prefix("pub(crate) enum "))
            .or_else(|| rest.strip_prefix("pub(super) struct "))
            .or_else(|| rest.strip_prefix("pub(super) enum "))
            .or_else(|| rest.strip_prefix("pub struct "))
            .or_else(|| rest.strip_prefix("pub enum "));
        if let Some(item) = item {
            if derives.contains("Serialize") {
                // `pub struct PeerId(pub [u8; 32]);` and
                // `pub struct Hello {` both stop at the first delimiter.
                let name: String = item
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    serializable.push(name);
                }
            }
            derives.clear();
        }

        // Anything else on this line: a doc comment, a non-attribute,
        // non-item line, a blank line: is normal text between the derive
        // and its item and must not clear the carried-forward derive text.
    }

    serializable
}

/// Walks every `.rs` file under `dir`, recursively, and returns the union of
/// [`scan_serializable_types`] over each one's contents, each name paired
/// with the file it was found in.
///
/// **The evasion this closes**: the exhaustiveness scan used to read one
/// hardcoded path (`crates/tcr-peer-wire/src/lib.rs`). A serializable wire
/// type declared in ANY other file under that crate's `src/`: a second
/// module, a file added later, was invisible to the allowlist gate no
/// matter what it derived. Reading the whole directory tree removes the
/// "which one file" assumption entirely: every `.rs` file under the wire
/// crate's source is scanned, however many there are.
///
/// The file rides along with every name so a failure message
/// can name where an uncovered type lives instead of leaving the reader to
/// grep the crate for it themselves.
fn scan_dir(dir: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .unwrap_or_else(|err| panic!("failed to read directory {}: {err}", current.display()));
        for entry in entries {
            let entry = entry.expect("a directory entry under the wire crate's src is readable");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
                found.extend(
                    scan_serializable_types(&source)
                        .into_iter()
                        .map(|name| (name, path.clone())),
                );
            }
        }
    }

    found
}

/// **Evasion 1**: a blank line between the `#[derive(...)]` and the item it
/// applies to must not hide the item from the scan. Before the fix, the scan
/// cleared its carried-forward derive text on any blank line, so this failed
/// with `found: []`.
#[test]
fn scan_survives_a_blank_line_between_derive_and_item() {
    let source = "#[derive(Serialize, Deserialize)]\n\npub struct BlankLineEvasion {\n    pub field: String,\n}\n";
    let found = scan_serializable_types(source);
    assert!(
        found.iter().any(|name| name == "BlankLineEvasion"),
        "a blank line between the derive and its item must not hide the item from the scan: \
         found {found:?}"
    );
}

/// **Evasion 2**: `#[cfg_attr(..., derive(Serialize, Deserialize))]` derives
/// `Serialize` exactly like a bare `#[derive(Serialize, Deserialize)]` under
/// the feature it is gated on, and must be recognised the same way. Before
/// the fix, only a line starting with `#[derive(` was tracked, so this
/// failed with `found: []`.
#[test]
fn scan_recognizes_cfg_attr_derive() {
    let source = "#[cfg_attr(feature = \"wire\", derive(Serialize, Deserialize))]\npub struct CfgAttrEvasion {\n    pub field: String,\n}\n";
    let found = scan_serializable_types(source);
    assert!(
        found.iter().any(|name| name == "CfgAttrEvasion"),
        "#[cfg_attr(..., derive(Serialize))] must be recognized the same as a bare \
         #[derive(Serialize)]: found {found:?}"
    );
}

/// Before the fix, an attribute and its item sharing one line of source
/// were swallowed whole as "derive text" and the item was never seen.
#[test]
fn scan_recognizes_a_derive_and_item_on_one_line() {
    let source = "#[derive(Serialize, Deserialize)] pub struct OneLineEvasion {\n    pub field: String,\n}\n";
    let found = scan_serializable_types(source);
    assert!(
        found.iter().any(|name| name == "OneLineEvasion"),
        "a #[derive(...)] and its item sharing one line of source must not hide the item \
         from the scan: found {found:?}"
    );
}

/// **Evasion 4** (testing finding): a non-derive attribute
/// leading the SAME line as the derive hid the derive entirely, because the
/// old scan only recognized a `#[derive(`/`#[cfg_attr(` at the very start of
/// a line. Before the fix this failed with `found: []`.
#[test]
fn scan_recognizes_a_non_derive_attribute_leading_the_derive_line() {
    let source = "#[non_exhaustive] #[derive(Serialize, Deserialize)]\npub struct LeadingAttributeEvasion {\n    pub field: String,\n}\n";
    let found = scan_serializable_types(source);
    assert!(
        found.iter().any(|name| name == "LeadingAttributeEvasion"),
        "a non-derive attribute leading the derive on the same line must not hide the \
         derive from the scan: found {found:?}"
    );
}

/// **Evasion 5** (testing finding): `pub(crate)` and
/// `pub(super)` types evaded the old item match entirely, which only tried
/// `pub struct `/`pub enum `. A `pub(crate)` type derives `Serialize` exactly
/// as much as a `pub` one. Before the fix both cases below returned `[]`.
#[test]
fn scan_recognizes_pub_crate_and_pub_super_items() {
    let source = "#[derive(Serialize, Deserialize)]\npub(crate) struct CrateVisibleEvasion {\n    pub field: String,\n}\n\n#[derive(Serialize, Deserialize)]\npub(super) enum SuperVisibleEvasion {\n    Variant,\n}\n";
    let found = scan_serializable_types(source);
    assert!(
        found.iter().any(|name| name == "CrateVisibleEvasion"),
        "pub(crate) struct must be found exactly like pub struct: found {found:?}"
    );
    assert!(
        found.iter().any(|name| name == "SuperVisibleEvasion"),
        "pub(super) enum must be found exactly like pub enum: found {found:?}"
    );
}

/// Before the fix, `the_allowlist_covers_every_serializable_wire_type` read
/// exactly one hardcoded file. A serializable type declared in ANY other
/// `.rs` file under the wire crate's `src/` was invisible to it. This drives
/// [`scan_dir`] directly against a throwaway directory tree with two files :
/// one at the top, one nested a level down: and asserts both are found, so
/// "scanned a directory" is proven, not just "scanned lib.rs" under a new
/// name.
#[test]
fn scan_dir_reads_every_rs_file_not_only_a_hardcoded_one() {
    let root = tempfile::tempdir().expect("a temp dir for the multi-file scan fixture");

    std::fs::write(
        root.path().join("lib.rs"),
        "#[derive(Serialize, Deserialize)]\npub struct TopLevelType {\n    pub a: String,\n}\n",
    )
    .expect("write the top-level fixture file");

    let nested = root.path().join("nested_module");
    std::fs::create_dir_all(&nested).expect("create the nested module directory");
    std::fs::write(
        nested.join("second.rs"),
        "#[derive(Serialize, Deserialize)]\npub struct NestedModuleType {\n    pub b: String,\n}\n",
    )
    .expect("write the nested fixture file");

    let found = scan_dir(root.path());
    assert!(
        found.iter().any(|(name, _file)| name == "TopLevelType"),
        "positive control: the scan missed the top-level fixture file: found {found:?}"
    );
    assert!(
        found.iter().any(|(name, _file)| name == "NestedModuleType"),
        "the scan must read every .rs file under the directory, not only one hardcoded \
         file: found {found:?}"
    );

    // The file rides along with the name, and it names the RIGHT file: the
    // nested type is found under `nested_module/second.rs`, not under
    // `lib.rs`. Without this, "names the file" would be unproven: a scan
    // that stamped every hit with whichever path it read last would still
    // pass the two assertions above.
    let (_name, nested_file) = found
        .iter()
        .find(|(name, _file)| name == "NestedModuleType")
        .expect("NestedModuleType was asserted found above");
    assert_eq!(
        nested_file.file_name().and_then(|n| n.to_str()),
        Some("second.rs"),
        "the file paired with a found type must be the file it was actually found in: {found:?}"
    );
}

#[test]
fn the_allowlist_covers_every_serializable_wire_type() {
    let serializable = scan_dir(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/tcr-peer-wire/src"
    )));

    // Positive control: the scan really found types, and it found ones this
    // file names elsewhere. Without it, a parser that silently matched nothing
    // would satisfy the difference below by measuring nothing.
    assert!(
        serializable.len() >= 15,
        "positive control: the source scan found only {} serializable types in the wire \
         crate, so its findings below prove nothing. Found: {serializable:?}",
        serializable.len()
    );
    for expected in ["Hello", "StreamHeader", "Knock", "PeerId"] {
        assert!(
            serializable.iter().any(|(name, _file)| name == expected),
            "positive control: the scan missed {expected:?}, which is a `pub struct` with a \
             `Serialize` derive in that file: so what it did NOT find means nothing. \
             Found: {serializable:?}"
        );
    }

    // Named per finding (file and name both), so an uncovered type reads as
    // "here is where to look" rather than a bare identifier the reader has
    // to go grep for.
    let uncovered: Vec<String> = serializable
        .iter()
        .filter(|(name, _file)| !ALLOWLISTED_WIRE_TYPES.contains(&name.as_str()))
        .map(|(name, file)| format!("{name} ({})", file.display()))
        .collect();
    assert!(
        uncovered.is_empty(),
        "these types derive Serialize and are on no allowlist: {uncovered:?}. Every type on \
         this wire has to have its exact key set written down in \
         `every_wire_type_serializes_only_allowlisted_keys`, or a credential reaches the wire \
         on a type no gate ever looks at. Add a sample there and the name to \
         ALLOWLISTED_WIRE_TYPES."
    );

    // And the other direction: a name in the list that no longer exists in the
    // crate is a stale entry, which is how a list stops being a checklist.
    let stale: Vec<&&str> = ALLOWLISTED_WIRE_TYPES
        .iter()
        .filter(|name| !serializable.iter().any(|(found, _file)| found == *name))
        .collect();
    assert!(
        stale.is_empty(),
        "these names are in ALLOWLISTED_WIRE_TYPES and no longer derive Serialize in the \
         wire crate: {stale:?}. A list with dead entries is a list nobody trusts."
    );
}

/// **Every key this wire may carry, per type, as an exact set.**
///
/// The gate above (`wire_has_no_credential_field`) is a DENYLIST over
/// the crate's source identifiers, and testing proved three ways past it:
///
/// - `pub join_key_b64: String`: the identifier is `join_key_b64`, which is
///   not the listed `join_key`, so a whole-identifier match never fires;
/// - `pub access_token_jwt: String`: the same trick on `access_token`;
/// - `#[serde(rename = "accessToken")] pub note: String`, the field is named
///   `note`, and the renamed key `accesstoken` is not a listed spelling, so a
///   credential reaches the wire under a name the denylist never heard of.
///
/// A denylist cannot be fixed by lengthening it: the next spelling is the one
/// nobody wrote down. So this gate asks the opposite question. It SERIALIZES a
/// sample of every type on this wire and asserts the key names are exactly the
/// permitted set: which means **any** new key on **any** of these types fails
/// this test until somebody adds it here on purpose, whatever it is called and
/// however it got its name.
///
/// Each sample fills every `Option` field, so the expected set is exact in
/// both directions: a key added fails, and a key removed fails too.
///
/// Watch it fail: add `pub join_key_b64: String` to `Enroll` (or any of the
/// three probes above) and this test names the type and the offending key.
#[test]
fn every_wire_type_serializes_only_allowlisted_keys() {
    use std::collections::BTreeSet;
    use tcr_peer_wire::{
        Caps, CollapseHint, Enroll, Hello, InstanceId, Knock, Lease, LeaseGrant, LeaseReceipt,
        LeaseRefusal, LeaseRequest, LendScope, Lendable, MoveOffer, NeighborBrief, TunnelTarget,
        INSTANCE_ID_BYTES,
    };

    /// The keys one serialized sample carries at its top level, or a refusal
    /// if the sample is not a JSON object at all: a wire type that
    /// serializes to a scalar (`PeerId`, `StreamKind`, `Window`) carries no
    /// key and therefore no credential, and is asserted separately below.
    fn keys(value: &serde_json::Value) -> BTreeSet<String> {
        value
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Every key anywhere inside a sample, at any depth. The top-level
    /// assertions below cover each type's own fields; this catches a key on a
    /// type somebody forgot to add to the table.
    fn all_keys(value: &serde_json::Value, into: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    into.insert(key.clone());
                    all_keys(child, into);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    all_keys(item, into);
                }
            }
            _ => {}
        }
    }

    let node = PeerId([9_u8; 32]);
    let caps = Caps {
        egress: true,
        forward: true,
        lends: true,
    };
    let lease = Lease {
        lease_id: 1,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.2),
        granted_at_ms: 10,
        expires_at_ms: 20,
        spent: 0.1,
        max_inflight: 2,
        until: Some(30),
    };
    let hello = Hello {
        proto: 1,
        node,
        label: "laptop-2".to_string(),
        seq: 4,
        caps,
        addrs: vec!["192.0.2.7:9600".to_string()],
        ttl_s: 60,
        // Every optional block filled, so the expected set is exact in both
        // directions: a field REMOVED from `Hello` fails this test too.
        lendable: Some(vec![Lendable {
            window: Window::FiveHour,
            unit: LeaseUnit::Tokens(1_000),
            accounts: 2,
        }]),
        hops_to_egress: Some(1),
        briefs: Some(vec![NeighborBrief {
            node,
            caps,
            addrs: vec!["192.0.2.8:9600".to_string()],
        }]),
        build_sha: Some("deadbeef".to_string()),
        boot_id: Some(7),
        observed_you_at: Some("192.0.2.9:41000".to_string()),
    };

    let samples: Vec<(&str, serde_json::Value, &[&str])> = vec![
        (
            "StreamHeader",
            serde_json::to_value(StreamHeader {
                kind: StreamKind::Tunnel,
                target: Some(TunnelTarget::Origin {
                    host: "api.anthropic.com".to_string(),
                    port: 443,
                }),
                via: vec![node],
                hops_remaining: 1,
                request_id: 3,
            })
            .expect("a StreamHeader serializes"),
            &["kind", "target", "via", "hopsRemaining", "requestId"],
        ),
        (
            "TunnelTarget::Origin",
            serde_json::to_value(TunnelTarget::Origin {
                host: "api.anthropic.com".to_string(),
                port: 443,
            })
            .expect("a TunnelTarget serializes"),
            &["target", "host", "port"],
        ),
        (
            "LeaseUnit",
            serde_json::to_value(LeaseUnit::Fraction(0.2)).expect("a LeaseUnit serializes"),
            &["unit", "amount"],
        ),
        (
            "Lendable",
            serde_json::to_value(Lendable {
                window: Window::FiveHour,
                unit: LeaseUnit::Fraction(0.2),
                accounts: 3,
            })
            .expect("a Lendable serializes"),
            &["window", "unit", "accounts"],
        ),
        (
            "Lease",
            serde_json::to_value(lease).expect("a Lease serializes"),
            &[
                "leaseId",
                "window",
                "unit",
                "grantedAtMs",
                "expiresAtMs",
                "spent",
                "maxInflight",
                // The absolute end of the LENDING, the one time
                // field a borrower may know. Filled in the sample above, like
                // every other `Option` here, so this set stays exact in both
                // directions.
                "until",
            ],
        ),
        (
            "Caps",
            serde_json::to_value(caps).expect("Caps serialize"),
            &["egress", "forward", "lends"],
        ),
        (
            "NeighborBrief",
            serde_json::to_value(NeighborBrief {
                node,
                caps,
                addrs: vec!["192.0.2.8:9600".to_string()],
            })
            .expect("a NeighborBrief serializes"),
            &["node", "caps", "addrs"],
        ),
        (
            "Hello",
            serde_json::to_value(hello.clone()).expect("a Hello serializes"),
            &[
                "proto",
                "node",
                "label",
                "seq",
                "caps",
                "addrs",
                "ttlS",
                "lendable",
                "hopsToEgress",
                "briefs",
                "buildSha",
                "bootId",
                "observedYouAt",
            ],
        ),
        (
            "Enroll",
            serde_json::to_value(Enroll {
                invite_id: 5,
                label: "laptop-2".to_string(),
            })
            .expect("an Enroll serializes"),
            &["inviteId", "label"],
        ),
        (
            "LeaseRequest",
            serde_json::to_value(LeaseRequest {
                window: Window::SevenDayOi,
                unit: LeaseUnit::Fraction(0.2),
                ttl_s: 300,
                max_inflight: 2,
            })
            .expect("a LeaseRequest serializes"),
            &["window", "unit", "ttlS", "maxInflight"],
        ),
        (
            "LeaseGrant",
            serde_json::to_value(LeaseGrant {
                lease: Some(lease),
                refusal: Some(LeaseRefusal::OwnerGuard),
            })
            .expect("a LeaseGrant serializes"),
            &["lease", "refusal"],
        ),
        (
            "LeaseReceipt",
            serde_json::to_value(LeaseReceipt {
                lease_id: 1,
                request_id: 3,
                spent: 0.05,
            })
            .expect("a LeaseReceipt serializes"),
            &["leaseId", "requestId", "spent"],
        ),
        (
            "CollapseHint",
            serde_json::to_value(CollapseHint {
                node,
                addrs: vec!["192.0.2.8:9600".to_string()],
                observed_rtt_ms: 12,
            })
            .expect("a CollapseHint serializes"),
            &["node", "addrs", "observedRttMs"],
        ),
        (
            "MoveOffer",
            serde_json::to_value(MoveOffer {}).expect("a MoveOffer serializes"),
            &[],
        ),
        (
            "Control::Hello",
            serde_json::to_value(Control::Hello(hello)).expect("a Control serializes"),
            &[
                "type",
                "proto",
                "node",
                "label",
                "seq",
                "caps",
                "addrs",
                "ttlS",
                "lendable",
                "hopsToEgress",
                "briefs",
                "buildSha",
                "bootId",
                "observedYouAt",
            ],
        ),
        (
            "Control::Enroll",
            serde_json::to_value(Control::Enroll(Enroll {
                invite_id: 5,
                label: "laptop-2".to_string(),
            }))
            .expect("a Control serializes"),
            &["type", "inviteId", "label"],
        ),
        (
            "Control::Ping",
            serde_json::to_value(Control::Ping).expect("a Control serializes"),
            &["type"],
        ),
        // The ONE credential on this wire. It is in the table for
        // the same reason everything else is: its key set is exact, so a
        // `refreshToken` beside the bearer reds this test as well as the
        // scoped denylist above. The two gates disagree about HOW (one reads
        // the source, one reads the bytes) and agree about what is permitted.
        (
            "Control::Handoff",
            serde_json::to_value(Control::Handoff {
                lease_id: 1,
                access_token: tcr_peer_wire::HandoffToken::new("sk-not-a-real-token".to_string()),
                expires_at_ms: 20,
            })
            .expect("a Control serializes"),
            &["type", "leaseId", "accessToken", "expiresAtMs"],
        ),
        (
            "Control::UsageHint",
            serde_json::to_value(Control::UsageHint {
                lease_id: 1,
                spent: 0.03,
            })
            .expect("a Control serializes"),
            &["type", "leaseId", "spent"],
        ),
        (
            "Knock",
            serde_json::to_value(Knock {
                instance_id: InstanceId([3_u8; INSTANCE_ID_BYTES]),
                // The optional field is filled, so the expected set is exact
                // in both directions.
                proposed_name: Some("laptop-2".to_string()),
                wire_version: 1,
            })
            .expect("a Knock serializes"),
            &["instanceId", "proposedName", "wireVersion"],
        ),
        // The lease scope, in this crate and on NO message. Both
        // keyed variants are sampled so the union check below knows `group`
        // and `accounts`: and that is what makes putting a scope on a
        // MESSAGE fail: the message's own key set is exact, so a `scope` key
        // on `Lease` or `LeaseGrant` reds this test even though the scope's
        // own keys are permitted here.
        (
            "LendScope::Group",
            serde_json::to_value(LendScope::Group("work".to_string()))
                .expect("a LendScope serializes"),
            &["group"],
        ),
        (
            "LendScope::Accounts",
            serde_json::to_value(LendScope::Accounts(vec!["studio-mac".to_string()]))
                .expect("a LendScope serializes"),
            &["accounts"],
        ),
    ];

    // Positive control: the table is not empty and the samples really did
    // serialize into objects with keys. Without this, a future edit that left
    // `samples` empty (or a serializer that produced `null`), would pass
    // every assertion below by measuring nothing.
    assert!(
        samples.len() >= 17,
        "positive control: the table must cover every type on this wire, found {}",
        samples.len()
    );
    assert!(
        keys(&samples[0].1).contains("requestId"),
        "positive control: the samples are not serializing into keyed objects"
    );

    let mut every_key = BTreeSet::new();
    let mut allowed_anywhere = BTreeSet::new();

    for (name, value, allowed) in &samples {
        let found = keys(value);
        let expected: BTreeSet<String> = allowed.iter().map(|k| (*k).to_string()).collect();
        assert_eq!(
            found, expected,
            "{name} does not carry exactly the keys this wire permits. A key this test \
             has never heard of is how a credential reaches the wire under a name no \
             denylist spells; a key that vanished is a contract broken for every other \
             build on the LAN. Add it here on purpose, or take it off the type."
        );
        all_keys(value, &mut every_key);
        allowed_anywhere.extend(expected);
    }

    // A key nested inside a type nobody added to the table above.
    let unexpected: Vec<&String> = every_key.difference(&allowed_anywhere).collect();
    assert!(
        unexpected.is_empty(),
        "these keys appear somewhere in a serialized wire message and are on no type's \
         allowlist: {unexpected:?}"
    );

    // The three types that serialize to a scalar carry no key at all, which is
    // the strongest form of "no credential field": there is nowhere to put one.
    for (name, value) in [
        (
            // `LendScope::All` is the variant with no data at all, so it is a
            // bare string on this side of serde: the same "nowhere to put a
            // credential" property the four below have.
            "LendScope::All",
            serde_json::to_value(LendScope::All).expect("a LendScope serializes"),
        ),
        (
            // The bearer itself: a bare string, so the frame's shape did not
            // move when the redacting newtype went round it.
            "HandoffToken",
            serde_json::to_value(tcr_peer_wire::HandoffToken::new(
                "sk-not-a-real-token".to_string(),
            ))
            .expect("a HandoffToken serializes"),
        ),
        (
            "PeerId",
            serde_json::to_value(node).expect("a PeerId serializes"),
        ),
        (
            "StreamKind",
            serde_json::to_value(StreamKind::Control).expect("a StreamKind serializes"),
        ),
        (
            "Window",
            serde_json::to_value(Window::SevenDay).expect("a Window serializes"),
        ),
        (
            "LeaseRefusal",
            serde_json::to_value(LeaseRefusal::InFlightFull).expect("a LeaseRefusal serializes"),
        ),
        (
            "InstanceId",
            serde_json::to_value(InstanceId([4_u8; INSTANCE_ID_BYTES]))
                .expect("an InstanceId serializes"),
        ),
    ] {
        assert!(
            !value.is_object(),
            "{name} is expected on the wire as a scalar, and an object here means it \
             gained fields nothing checks: {value}"
        );
    }
}
