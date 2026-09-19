//! The wire crate's own encode/decode and parse/display pairs round-trip over
//! the whole input domain, not just the hand-picked examples
//! `tests/peer_wire.rs` already covers.
//!
//! # `PeerId`'s round trip uses `to_wire`, not `to_string`
//!
//! The obvious round trip is
//! `PeerId::parse(&id.to_string())`. `PeerId` has no `Display` impl and no
//! `to_string`; the only two string forms are [`tcr_peer_wire::PeerId::to_wire`]
//! (lossless, what `Serialize`/`Deserialize` use) and
//! [`tcr_peer_wire::PeerId::display`], whose own doc-comment says it is
//! **deliberately one-way**, `PeerId::parse` refuses it on purpose, so
//! `parse(&id.display())` would falsify a genuine roundtrip property on every
//! case, not find a bug. This file round-trips through `to_wire`, the form
//! `parse` actually accepts; `display()` not round-tripping is the documented,
//! intended behaviour and is not tested here as a "roundtrip" failure.
//! `InstanceId` and `LendScope` DO implement `Display` (`to_wire`/`to_spec`
//! respectively) and are round-tripped through `.to_string()`.

use proptest::prelude::*;
use tcr_peer_wire::{
    decode_frame, decode_key32, encode_frame, encode_key32, InstanceId, LendScope, PeerId,
    MAX_FRAME_BYTES,
};

/// A label `sanitize_label`/`LendScope::parse` accepts unchanged: short,
/// alphanumeric, never uuid-shaped. Kept simple on purpose, the point of this
/// property is the scope parser's own round trip, not `sanitize_label`'s
/// alphabet, which `tests/peer_wire.rs` already covers on its own.
fn valid_label() -> impl Strategy<Value = String> {
    "[A-Za-z][A-Za-z0-9]{0,9}"
}

fn any_lend_scope() -> impl Strategy<Value = LendScope> {
    prop_oneof![
        Just(LendScope::All),
        valid_label().prop_map(LendScope::Group),
        prop::collection::vec(valid_label(), 1..4).prop_map(LendScope::Accounts),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256)
    ))]

    /// `decode_frame(encode_frame(payload)) == payload`, over the whole size
    /// range `encode_frame` can be asked to carry plus 4,465 bytes past its
    /// own cap, the boundary this frame format exists to enforce.
    #[test]
    fn frame_roundtrips_or_refuses_too_long(payload in prop::collection::vec(any::<u8>(), 0..70_000usize)) {
        let mut out = Vec::new();
        let encoded = encode_frame(&payload, &mut out);
        if payload.len() > MAX_FRAME_BYTES {
            let refused = matches!(
                encoded,
                Err(tcr_peer_wire::FrameError::TooLong { len }) if len == payload.len()
            );
            prop_assert!(refused, "a payload over MAX_FRAME_BYTES must refuse with TooLong{{len}}");
            prop_assert!(out.is_empty(), "a refused frame must not have written a partial prefix");
        } else {
            prop_assert!(encoded.is_ok());
            let (decoded, consumed) = decode_frame(&out).expect("a frame this crate just wrote must decode");
            prop_assert_eq!(decoded, payload.as_slice());
            prop_assert_eq!(consumed, out.len());
        }
    }

    /// `decode_key32(encode_key32(bytes)) == bytes`, over every 32-byte value.
    #[test]
    fn key32_roundtrips(bytes in prop::array::uniform32(any::<u8>())) {
        let wire = encode_key32(&bytes);
        let back = decode_key32(&wire).expect("a value this crate just encoded must decode");
        prop_assert_eq!(back, bytes);
    }

    /// `PeerId::parse(&id.to_wire()) == id`, over every 32-byte key.
    #[test]
    fn peer_id_roundtrips_through_wire_form(bytes in prop::array::uniform32(any::<u8>())) {
        let id = PeerId(bytes);
        let back = PeerId::parse(&id.to_wire()).expect("a value this crate just encoded must decode");
        prop_assert_eq!(back, id);
    }

    /// `InstanceId::parse(&id.to_string()) == id`, over every 8-byte value.
    #[test]
    fn instance_id_roundtrips_through_display(bytes in prop::array::uniform8(any::<u8>())) {
        let id = InstanceId(bytes);
        let back = InstanceId::parse(&id.to_string()).expect("a value this crate just displayed must parse");
        prop_assert_eq!(back, id);
    }

    /// `LendScope::parse(&scope.to_string()) == scope`, over `All`, a random
    /// group name and a random non-empty account list.
    #[test]
    fn lend_scope_roundtrips_through_display(scope in any_lend_scope()) {
        let back = LendScope::parse(&scope.to_string()).expect("a value this crate just displayed must parse");
        prop_assert_eq!(back, scope);
    }
}
