//! Verify that the process on the other end of a unix socket is a build we
//! trust, before handing it anything.
//!
//! # Why this exists
//!
//! The handoff gives the peer the live listening socket of the proxy that holds
//! every account's OAuth credentials. Whoever holds that socket serves Claude
//! Code traffic, and every request through it carries a bearer token. Without a
//! peer check, "can run code as this user" becomes "owns the Anthropic
//! credentials", which is a promotion the rest of this system takes some
//! trouble to prevent.
//!
//! Socket file permissions are the floor and not the answer. `0600` in a `0700`
//! directory stops other *users*; the concern here is another process belonging
//! to this one, which is the account every locally-running tool already has.
//!
//! # The audit token, not the pid
//!
//! `getsockopt` offers `LOCAL_PEERPID`, and it is the wrong one. A pid is
//! reusable, and a peer may `exec` between the moment it is checked and the
//! moment it is trusted, so a pid-keyed check can validate one program and then
//! hand the socket to a different one. That is the textbook shape of this bug.
//!
//! [`LOCAL_PEERTOKEN`] returns the peer's **audit token**, which names a
//! process instance rather than a recyclable number. It goes to
//! `SecCodeCopyGuestWithAttributes` as `kSecGuestAttributeAudit`, and the
//! resulting code object is checked with `SecCodeCheckValidity`.
//!
//! # Who owns which half
//!
//! Every Security-framework call below is
//! [`security_framework::os::macos::code_signing`]. That module has existed
//! since 2021 and exposes exactly this API: the audit-token guest attribute,
//! the requirement parser, and the validity check, with the CF ownership rules
//! already encoded in the types. This file briefly carried a second,
//! hand-written copy of those bindings because a search of the crate's
//! top-level modules did not reach `os::macos`; the copy is gone and the trade
//! is recorded below.
//!
//! What stays here is the part no crate does: reading the peer's audit token
//! off the socket with `getsockopt`. That is a socket call, not a
//! Security-framework call, and it is the only `unsafe` block left in this
//! module.
//!
//! One guard was lost in the move. The hand-written version refused a Security
//! call that returned `errSecSuccess` *and* a null out-parameter; upstream
//! checks the status only and then `assume_init()`s the out-parameter
//! (`security-framework-3.7.0/src/lib.rs:51`). Every failure these calls
//! actually produce carries a non-zero status, so upstream catches them all,
//! and a zero status with nothing written would be Apple violating its own
//! Create-rule contract. It is upstream's invariant to hold rather than ours to
//! re-derive, and holding a private copy of it was the more expensive mistake.
//!
//! # Failure is refusal
//!
//! Every error path here returns `Err`. There is deliberately no branch that
//! treats "could not determine the peer" as acceptable: an unsigned development
//! build and an attacker's binary are indistinguishable to this code, and they
//! should be, because they are equally unable to prove what they are.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::str::FromStr;

use core_foundation::base::TCFType;
use core_foundation::data::CFData;
use security_framework::os::macos::code_signing::{
    Flags, GuestAttributes, SecCode, SecRequirement,
};

/// `sys/un.h`: retrieve the peer's audit token. Deliberately NOT `LOCAL_PEERPID`
/// (`0x002`). See the module docs.
const LOCAL_PEERTOKEN: libc::c_int = 0x006;
/// `sys/un.h`: the option level for `LOCAL_*` socket options.
const SOL_LOCAL: libc::c_int = 0;

/// `audit_token_t` from `bsm/audit.h`: eight opaque words. Its contents are
/// never interpreted here; it is passed to the Security framework verbatim,
/// which is the whole point of using it rather than a pid.
#[repr(C)]
#[derive(Clone, Copy)]
struct AuditToken {
    val: [u32; 8],
}

/// Size of an `audit_token_t`: eight 32-bit words.
const AUDIT_TOKEN_BYTES: usize = std::mem::size_of::<AuditToken>();

/// Read the audit token of the process on the other end of `stream`.
fn peer_audit_token(stream: &UnixStream) -> io::Result<AuditToken> {
    let mut token = AuditToken { val: [0; 8] };
    let mut len = AUDIT_TOKEN_BYTES as libc::socklen_t;
    // SAFETY: `token` is a live, correctly sized `audit_token_t`, and `len`
    // describes it. `getsockopt` writes at most `len` bytes into it.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            SOL_LOCAL,
            LOCAL_PEERTOKEN,
            std::ptr::addr_of_mut!(token).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != AUDIT_TOKEN_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("LOCAL_PEERTOKEN returned {len} bytes, expected {AUDIT_TOKEN_BYTES}"),
        ));
    }
    Ok(token)
}

/// Resolve an audit token to a code object for that running process.
fn code_for_token(token: &AuditToken) -> io::Result<SecCode> {
    // The token's 32 bytes are what the Security framework wants, uninterpreted.
    let bytes = token_bytes(token);
    let data = CFData::from_buffer(&bytes);

    // `set_audit_token` takes a raw `CFDataRef` and does not extend its
    // lifetime for us, so `data` must outlive the call below. It does: both
    // locals live to the end of this function, and the call is the tail
    // expression. Do not hoist `attrs` out of this scope without carrying
    // `data` with it.
    let mut attrs = GuestAttributes::new();
    attrs.set_audit_token(data.as_concrete_TypeRef());

    // A failure here is "cannot establish what the peer is", which is a refusal
    // like any other. `100001` (kPOSIXErrorEPERM) and `100003` (no such
    // process) are both documented outcomes, not impossible ones.
    SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE).map_err(|e| {
        io::Error::other(format!(
            "resolving the peer audit token to a code object failed: {e}"
        ))
    })
}

/// Reinterpret an audit token as the bytes the Security framework expects.
///
/// Split out so it can be asserted on directly: `code_for_token` passes these
/// bytes straight through, so a wrong reinterpretation is invisible to every
/// caller above it.
fn token_bytes(token: &AuditToken) -> [u8; AUDIT_TOKEN_BYTES] {
    // SAFETY: `AuditToken` is `#[repr(C)]` over eight `u32`, so it has no
    // padding and no niches; every bit pattern of the source is a valid byte
    // array of the same size, which `AUDIT_TOKEN_BYTES` pins.
    unsafe { std::mem::transmute::<AuditToken, [u8; AUDIT_TOKEN_BYTES]>(*token) }
}

/// Check that the process on the other end of `stream` satisfies `requirement`,
/// a code-signing requirement string.
///
/// The requirement this project uses is team-scoped rather than bundle-scoped,
/// because a CLI `tcr` handing over to a TcrBar (or the reverse) is ordinary and
/// those carry different identifiers. It also pins the two Developer ID marker
/// OIDs, so an Apple Development certificate for the same team does not pass:
///
/// ```text
/// anchor apple generic
///   and certificate 1[field.1.2.840.113635.100.6.2.6]
///   and certificate leaf[field.1.2.840.113635.100.6.1.13]
///   and certificate leaf[subject.OU] = "UJQ3GQF56Y"
/// ```
///
/// Returns `Ok(())` only when the peer is resolved AND satisfies it. Every
/// other outcome, including being unable to tell, is an error.
pub fn verify_peer(stream: &UnixStream, requirement: &str) -> io::Result<()> {
    let token = peer_audit_token(stream)?;
    let code = code_for_token(&token)?;

    // A requirement that does not parse is the caller's bug, and must stay
    // distinguishable from a peer that parsed fine and was refused.
    let req = SecRequirement::from_str(requirement).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("parsing the code requirement {requirement:?} failed: {e}"),
        )
    })?;

    code.check_validity(Flags::NONE, &req).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer does not satisfy the code requirement: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test binary's own code-signing identifier, read from the signature
    /// rather than guessed. Rust binaries on Apple Silicon are ad-hoc signed by
    /// the linker, so there is always one.
    fn own_identifier() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        let out = std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "--verbose=2"])
            .arg(&exe)
            .output()
            .ok()?;
        // codesign writes its description to stderr.
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.strip_prefix("Identifier=").map(str::to_string))
    }

    /// POSITIVE CONTROL for the audit-token plumbing.
    ///
    /// Both ends of a socketpair belong to this process, so the peer resolves to
    /// this test binary. If this fails, the two assertions below prove nothing:
    /// a rejection would only mean the token never arrived, not that the
    /// requirement did any work.
    #[test]
    fn a_peer_audit_token_resolves_to_a_code_object() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let token = peer_audit_token(&a).expect("LOCAL_PEERTOKEN must work on a unix socketpair");
        code_for_token(&token).expect("the audit token must resolve to a code object");
    }

    /// The bytes handed to the Security framework must be the token's own, in
    /// its own order.
    ///
    /// `code_for_token` is a pure pass-through, so nothing above this can tell a
    /// correct reinterpretation from a byte-swapped or truncated one: a wrong
    /// token resolves to no process, which reads exactly like a peer that
    /// failed the check. Asserted directly against the little-endian layout of
    /// a known value.
    #[test]
    fn the_token_bytes_are_the_token_in_order() {
        let token = AuditToken {
            val: [1, 2, 3, 4, 5, 6, 7, 8],
        };
        let bytes = token_bytes(&token);
        assert_eq!(bytes.len(), 32, "an audit token is eight 32-bit words");
        for (word, chunk) in token.val.iter().zip(bytes.chunks_exact(4)) {
            assert_eq!(
                chunk,
                word.to_ne_bytes(),
                "each word must survive in native order"
            );
        }
    }

    /// THE test. This binary is ad-hoc signed, so it must NOT satisfy a
    /// Developer ID requirement.
    ///
    /// The error KIND is asserted, not merely that it failed. `PermissionDenied`
    /// means `SecCodeCheckValidity` was reached and said no. Any other kind
    /// would mean the check never ran, which is a passing test for a broken
    /// reason.
    #[test]
    fn an_adhoc_signed_peer_is_refused_by_a_developer_id_requirement() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let err = verify_peer(
            &a,
            r#"anchor apple generic and certificate leaf[subject.OU] = "UJQ3GQF56Y""#,
        )
        .expect_err("an ad-hoc signed binary must not satisfy a Developer ID requirement");
        assert_eq!(
            err.kind(),
            io::ErrorKind::PermissionDenied,
            "expected a refusal from SecCodeCheckValidity, got: {err}"
        );
    }

    /// The Ok path exists and is reachable, so a refusal above is a decision
    /// rather than this function being incapable of returning success.
    #[test]
    fn a_peer_satisfying_the_requirement_is_accepted() {
        let Some(id) = own_identifier() else {
            // No signature at all is not this test's business to assert about.
            return;
        };
        let (a, _b) = UnixStream::pair().expect("socketpair");
        verify_peer(&a, &format!("identifier \"{id}\""))
            .expect("this binary must satisfy a requirement naming its own identifier");
    }

    /// A malformed requirement is rejected at parse time, and distinguishably:
    /// it must not read as "the peer failed the check".
    #[test]
    fn a_malformed_requirement_is_reported_as_bad_input() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let err = verify_peer(&a, "this is not a requirement (((")
            .expect_err("a malformed requirement must not succeed");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "a bad requirement must be distinguishable from a refused peer, got: {err}"
        );
    }
}
