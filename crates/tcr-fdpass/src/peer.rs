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
//! # Failure is refusal
//!
//! Every error path here returns `Err`. There is deliberately no branch that
//! treats "could not determine the peer" as acceptable: an unsigned development
//! build and an attacker's binary are indistinguishable to this code, and they
//! should be, because they are equally unable to prove what they are.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use core_foundation::base::TCFType;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef, OSStatus};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;

/// `sys/un.h`: retrieve the peer's audit token. Deliberately NOT `LOCAL_PEERPID`
/// (`0x002`) — see the module docs.
const LOCAL_PEERTOKEN: libc::c_int = 0x006;
/// `sys/un.h`: the option level for `LOCAL_*` socket options.
const SOL_LOCAL: libc::c_int = 0;
/// `kSecCSDefaultFlags`.
const SEC_CS_DEFAULT_FLAGS: u32 = 0;

/// `audit_token_t` from `bsm/audit.h`: eight opaque words. Its contents are
/// never interpreted here; it is passed to the Security framework verbatim,
/// which is the whole point of using it rather than a pid.
#[repr(C)]
#[derive(Clone, Copy)]
struct AuditToken {
    val: [u32; 8],
}

type SecCodeRef = *mut std::ffi::c_void;
type SecRequirementRef = *mut std::ffi::c_void;

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecGuestAttributeAudit: CFStringRef;

    fn SecCodeCopyGuestWithAttributes(
        host: SecCodeRef,
        attributes: CFDictionaryRef,
        flags: u32,
        guest: *mut SecCodeRef,
    ) -> OSStatus;

    fn SecRequirementCreateWithString(
        text: CFStringRef,
        flags: u32,
        requirement: *mut SecRequirementRef,
    ) -> OSStatus;

    fn SecCodeCheckValidity(
        code: SecCodeRef,
        flags: u32,
        requirement: SecRequirementRef,
    ) -> OSStatus;
}

/// A Core Foundation object this module owns and must release.
///
/// The Security calls below hand back `+1` references through out-parameters,
/// and there are several early returns between acquiring one and finishing with
/// it. Rather than place a `CFRelease` on each path and hope none is missed,
/// ownership is expressed in the type and `Drop` does it once.
struct OwnedCf(*mut std::ffi::c_void);

impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a +1 CF reference obtained from one of the
            // Security create/copy calls below, released exactly once here.
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }
}

/// Read the audit token of the process on the other end of `stream`.
fn peer_audit_token(stream: &UnixStream) -> io::Result<AuditToken> {
    let mut token = AuditToken { val: [0; 8] };
    let mut len = std::mem::size_of::<AuditToken>() as libc::socklen_t;
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
    if len as usize != std::mem::size_of::<AuditToken>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "LOCAL_PEERTOKEN returned {len} bytes, expected {}",
                std::mem::size_of::<AuditToken>()
            ),
        ));
    }
    Ok(token)
}

/// Resolve an audit token to a code object for that running process.
fn code_for_token(token: &AuditToken) -> io::Result<OwnedCf> {
    // SAFETY: reading the 32 bytes of a live `audit_token_t` as bytes. The
    // Security framework expects exactly these bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(token).cast::<u8>(),
            std::mem::size_of::<AuditToken>(),
        )
    };
    let data = CFData::from_buffer(bytes);
    // SAFETY: reading an immutable CFStringRef constant exported by the
    // Security framework; it is valid for the process lifetime.
    let key = unsafe { CFString::wrap_under_get_rule(kSecGuestAttributeAudit) };
    let attrs = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), data.as_CFType())]);

    let mut code: SecCodeRef = std::ptr::null_mut();
    // SAFETY: `attrs` outlives the call; `code` is a valid out-parameter.
    let status = unsafe {
        SecCodeCopyGuestWithAttributes(
            std::ptr::null_mut(),
            attrs.as_concrete_TypeRef(),
            SEC_CS_DEFAULT_FLAGS,
            &mut code,
        )
    };
    if status != 0 || code.is_null() {
        // 100001 is kPOSIXErrorEPERM and is a documented outcome here, not an
        // impossible one. It still means "cannot establish what the peer is",
        // which is a refusal like any other.
        return Err(io::Error::other(format!(
            "SecCodeCopyGuestWithAttributes failed for the peer audit token (OSStatus {status})"
        )));
    }
    Ok(OwnedCf(code))
}

/// Check that the process on the other end of `stream` satisfies `requirement`,
/// a code-signing requirement string.
///
/// The requirement this project uses is team-scoped rather than bundle-scoped,
/// because a CLI `tcr` handing over to a TcrBar (or the reverse) is ordinary and
/// those carry different identifiers:
///
/// ```text
/// anchor apple generic and certificate leaf[subject.OU] = "UJQ3GQF56Y"
/// ```
///
/// Returns `Ok(())` only when the peer is resolved AND satisfies it. Every
/// other outcome, including being unable to tell, is an error.
pub fn verify_peer(stream: &UnixStream, requirement: &str) -> io::Result<()> {
    let token = peer_audit_token(stream)?;
    let code = code_for_token(&token)?;

    let text = CFString::new(requirement);
    let mut req: SecRequirementRef = std::ptr::null_mut();
    // SAFETY: `text` outlives the call; `req` is a valid out-parameter.
    let status = unsafe {
        SecRequirementCreateWithString(text.as_concrete_TypeRef(), SEC_CS_DEFAULT_FLAGS, &mut req)
    };
    if status != 0 || req.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not parse the code requirement (OSStatus {status}): {requirement}"),
        ));
    }
    let req = OwnedCf(req);

    // SAFETY: both handles are live +1 references owned by this frame.
    let status = unsafe { SecCodeCheckValidity(code.0, SEC_CS_DEFAULT_FLAGS, req.0) };
    if status != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer does not satisfy the code requirement (OSStatus {status})"),
        ));
    }
    Ok(())
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
