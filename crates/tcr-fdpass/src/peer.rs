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

use core_foundation::base::{CFTypeID, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation::{declare_TCFType, impl_TCFType};
use core_foundation_sys::base::OSStatus;
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

/// The two Security-framework types this module touches, as proper CF types.
///
/// `declare_TCFType!`/`impl_TCFType!` are what `core-foundation` provides for
/// exactly this, and they replace a hand-rolled ownership wrapper that had to
/// be trusted by eye. They generate `Drop` (release), `Clone` (retain) and the
/// `TCFType` conversions, so ownership stops being something this file asserts
/// and becomes something the type system carries. It is also the shape
/// `security-framework` uses, which is what these bindings should eventually
/// become part of.
#[repr(C)]
pub struct __SecCode(std::ffi::c_void);
/// Opaque handle to a running program, as the Security framework sees it.
pub type SecCodeRef = *const __SecCode;
declare_TCFType!(SecCode, SecCodeRef);
impl_TCFType!(SecCode, SecCodeRef, SecCodeGetTypeID);

#[repr(C)]
pub struct __SecRequirement(std::ffi::c_void);
/// Opaque handle to a parsed code-signing requirement.
pub type SecRequirementRef = *const __SecRequirement;
declare_TCFType!(SecRequirement, SecRequirementRef);
impl_TCFType!(SecRequirement, SecRequirementRef, SecRequirementGetTypeID);

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecGuestAttributeAudit: CFStringRef;

    fn SecCodeGetTypeID() -> CFTypeID;
    fn SecRequirementGetTypeID() -> CFTypeID;

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

/// Reject a Security call that reported failure OR handed back nothing.
///
/// Both halves matter. A non-zero `OSStatus` is the documented failure, and a
/// zero status with a null out-parameter is the undocumented one; treating
/// either as success means wrapping a null in a CF type and releasing it later.
fn created<T>(status: OSStatus, out: *const T, call: &str, kind: io::ErrorKind) -> io::Result<()> {
    if status != 0 || out.is_null() {
        return Err(io::Error::new(
            kind,
            format!("{call} failed (OSStatus {status})"),
        ));
    }
    Ok(())
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
fn code_for_token(token: &AuditToken) -> io::Result<SecCode> {
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

    let mut code: SecCodeRef = std::ptr::null();
    // SAFETY: `attrs` outlives the call; `code` is a valid out-parameter.
    let status = unsafe {
        SecCodeCopyGuestWithAttributes(
            std::ptr::null(),
            attrs.as_concrete_TypeRef(),
            SEC_CS_DEFAULT_FLAGS,
            &mut code,
        )
    };
    // 100001 is kPOSIXErrorEPERM and is a documented outcome here, not an
    // impossible one. It still means "cannot establish what the peer is",
    // which is a refusal like any other.
    created(
        status,
        code,
        "SecCodeCopyGuestWithAttributes for the peer audit token",
        io::ErrorKind::Other,
    )?;
    // SAFETY: a Copy-rule call returns +1, which is exactly what
    // `wrap_under_create_rule` takes ownership of.
    Ok(unsafe { SecCode::wrap_under_create_rule(code) })
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
    let mut req: SecRequirementRef = std::ptr::null();
    // SAFETY: `text` outlives the call; `req` is a valid out-parameter.
    let status = unsafe {
        SecRequirementCreateWithString(text.as_concrete_TypeRef(), SEC_CS_DEFAULT_FLAGS, &mut req)
    };
    created(
        status,
        req,
        &format!("parsing the code requirement {requirement:?}"),
        io::ErrorKind::InvalidInput,
    )?;
    // SAFETY: a Create-rule call returns +1.
    let req = unsafe { SecRequirement::wrap_under_create_rule(req) };

    // SAFETY: both handles are live CF references owned by this frame.
    let status = unsafe {
        SecCodeCheckValidity(
            code.as_concrete_TypeRef(),
            SEC_CS_DEFAULT_FLAGS,
            req.as_concrete_TypeRef(),
        )
    };
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

    /// `created` has two independent rejection reasons and the integration
    /// tests only exercise one of them.
    ///
    /// In practice the Security calls return null *and* a non-zero status when
    /// they fail, so the null check alone makes every higher-level test pass
    /// even with the status check deleted (verified: removing `status != 0`
    /// leaves all four peer tests green). A call that returned a partial result
    /// with a failing status would then be wrapped and trusted. Tested directly
    /// because nothing above it can see the difference.
    #[test]
    fn created_rejects_a_failing_status_even_with_a_non_null_result() {
        let non_null: *const u8 = &1u8;

        created(0, non_null, "ok", io::ErrorKind::Other).expect("success must pass");

        let err = created(-1, non_null, "failing", io::ErrorKind::Other)
            .expect_err("a non-zero OSStatus must be rejected even when a pointer came back");
        assert!(
            err.to_string().contains("-1"),
            "the status belongs in the message: {err}"
        );

        created(0, std::ptr::null::<u8>(), "null", io::ErrorKind::Other)
            .expect_err("a null result must be rejected even when the status says success");
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
