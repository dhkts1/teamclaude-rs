//! The one seam for "no local account can serve this request".
//!
//! # Where it attaches, and why there
//!
//! The rotation loop asks `Manager::select_with_group` for an account; its
//! `None` arm (`src/proxy.rs:2311`) is the point at which this machine's own
//! fleet has nothing for this request. Today that arm rides out a transient
//! park, tries a revalidation serve, and then synthesizes the honest exhausted
//! 429. This trait slots in as the rung BETWEEN the revalidation serve and that
//! 429: everything local is cheaper than anything remote, so nothing here
//! pre-empts a recovery the local fleet could have made.
//!
//! The preference ladder, top to bottom:
//!
//! 1. local account, local egress: today's path, byte for byte;
//! 2. local account, a peer's internet (a blind tunnel; no lease, no ledger, no
//!    plaintext anywhere, and NOT this seam, it attaches on the egress side);
//! 3. **a peer's account under a lease: this seam, implementation one;**
//! 4. an api-key backend with its own budget: this seam, slot two;
//! 5. the existing exhausted 429 / offline 503, unchanged.
//!
//! One trait with two implementations rather than two seams, because the
//! decision "is there anything else to try?" must have exactly one answer for a
//! reader to trust either.
//!
//! # The headers cross, the credentials do not
//!
//! [`Ask`] used to carry no headers at all, because the only code in the proxy
//! that strips a client's own `authorization` and substitutes a pooled token
//! sits 215 lines DOWNSTREAM of this seam, and a fallback built here from the
//! raw request would have forwarded the client's credential to another host.
//! The conclusion drawn from that was "carry nothing", and it was wrong in a
//! way no test could see: a borrowed request then reached
//! `api.anthropic.com` with a bearer and nothing else, and the API answers a
//! request with no `anthropic-version` and no `content-type` with a 400. The
//! borrower handed that 400 back to its client as a served answer.
//!
//! So an `Ask` carries the client's headers, and the credential rule is kept by
//! [`Ask::scrubbed`], which is how that field is filled: it runs
//! [`crate::peer::serve::scrub_client_credentials`] over the client's map, so
//! the headers an `Ask` carries are the client's own minus every credential. What crosses a host boundary
//! is narrower still, `peer::serve::serve_request_from` keeps only
//! [`crate::peer::serve::LENDER_FORWARDED_HEADERS`], so the wire carries the
//! four names the API refuses a request without plus the user agent, and
//! nothing a borrower invented.
//!
//! # Inert until something installs a provider
//!
//! [`configured_provider`] answers whatever [`install_provider`] was given, and
//! `None` until something calls it, so an install is the whole of the feature
//! flag, and a build that never installs one behaves exactly as `main` does.
//! [`peer_lease_provider`] is the decision "does this peers file describe a node
//! that may borrow?", kept as a pure function of the file so it can be tested
//! without a socket.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use futures::future::BoxFuture;

use crate::peer::config::PeerStore;
use crate::peer::lease::PeerLeaseProvider;

/// What a provider is told about a request the local fleet could not serve.
///
/// Deliberately small. Its headers are the client's own minus every credential,
/// and [`Ask::scrubbed`] is what makes that true (see the module docs).
pub struct Ask<'a> {
    /// The request path. A provider that relays MUST refuse the
    /// client-credential paths on this value before opening anything. See
    /// `src/peer/serve.rs`.
    pub path: &'a str,
    /// The HTTP method the client's own request carried.
    ///
    /// # Why an `Ask` carries one at all
    ///
    /// `ServeRequest::method` used to be written as the literal `"POST"` by the
    /// borrower, so a client that sent `GET /v1/messages` had it POSTed on
    /// somebody else's account: an answer to a question it never asked, paid
    /// for out of a lease. The lender refuses a non-POST frame now
    /// (`peer::serve::handle_serve_on`), but that refusal happens after the
    /// bytes have crossed a host boundary, so the method has to be knowable
    /// HERE, at the seam, where a request can still be answered locally for
    /// free.
    ///
    /// A `&str` rather than `axum::http::Method` deliberately: this struct is
    /// the whole vocabulary between the proxy and a provider, and a provider
    /// that matches on a string cannot be handed a method type it has to
    /// convert. Compared case-insensitively by every reader, because a method
    /// is a token a client chose.
    pub method: &'a str,
    /// The model the request asked for, when it named one.
    pub model: Option<&'a str>,
    /// The group the request asked for, when it named one. A reserved group
    /// means "these accounts or nothing", and a provider outside the fleet is
    /// not one of them.
    pub group: Option<&'a str>,
    /// The session-affinity key, so a provider can keep one session on one
    /// remote lender and stop paying a cold prompt prefix per request.
    pub affinity: Option<u64>,
    /// How many local accounts this request already tried. A provider that
    /// wants to log "we went remote after N local misses" has the number.
    pub tried_local: usize,
    /// The request body, already read once by the proxy.
    pub body: Bytes,
    /// The client's own request headers, with every credential already removed.
    ///
    /// # Why this is not "the raw headers"
    ///
    /// A relayed request that carries no `anthropic-version` and no
    /// `content-type` is answered 400 by the API, so a seam that dropped every
    /// header could only ever produce a 400 the borrower then served to its
    /// client. A seam that carried them all would hand a client's
    /// `authorization`, `cookie` or `x-api-key` to another Mac.
    /// [`Ask::scrubbed`] takes the raw map and answers the middle: the client's
    /// headers minus
    /// [`crate::peer::serve::CLIENT_CREDENTIAL_HEADERS`].
    ///
    /// A provider that puts these on a wire narrows them further to
    /// [`crate::peer::serve::LENDER_FORWARDED_HEADERS`]; this map is what the
    /// seam knows, not what a lender is told.
    pub headers: HeaderMap,
}

impl Ask<'_> {
    /// A client's request headers as an [`Ask`] may hold them: every credential
    /// removed.
    ///
    /// The one way to fill [`Ask::headers`] from a request, and the scrub is in
    /// here rather than at the call site for the reason
    /// `peer::serve::serve_request_from`'s is: there is no order of operations
    /// for a later caller to get wrong, because the call that produces the map
    /// removes the credentials on the way through.
    ///
    /// A caller could still write the field by hand, so this is not the last
    /// line of the defence: what actually crosses to another Mac is narrowed to
    /// [`crate::peer::serve::LENDER_FORWARDED_HEADERS`] by the two functions
    /// that put an `Ask`'s headers on a wire, and neither of those names a
    /// credential.
    pub fn scrubbed(headers: &HeaderMap) -> HeaderMap {
        let mut scrubbed = headers.clone();
        crate::peer::serve::scrub_client_credentials(&mut scrubbed);
        scrubbed
    }
}

/// Something that can serve a request no local account could.
///
/// Boxed futures rather than an async trait so this stays object-safe: the
/// whole point is that the proxy holds one `dyn FallbackProvider` and does not
/// know which implementation it is talking to.
pub trait FallbackProvider: Send + Sync {
    /// For log lines and `tcr status --json`. Stable, lower case, one word.
    fn name(&self) -> &'static str;

    /// Serve it, or answer `None` for "not me", a refusal, a spent lease, no
    /// reachable lender, or a path this provider must not touch.
    ///
    /// `None` costs the caller nothing but the next rung, so a provider that is
    /// unsure answers `None` rather than an error page: the ladder's last rung
    /// is the honest 429 the proxy already has, and it is a better answer than
    /// anything a provider could invent.
    fn try_serve<'a>(&'a self, ask: &'a Ask<'a>) -> BoxFuture<'a, Option<Response>>;
}

/// The one provider this process consults, installed once at boot.
///
/// A `OnceLock` rather than a config read per call: this is on the answer path
/// of a request that has already failed the whole local fleet, and re-deciding
/// "is there a provider?" per request would put a file read (and a second
/// spelling of the decision) in front of an answer the client is waiting for.
/// Policy INSIDE the provider is still hot, [`PeerLeaseProvider`] re-reads the
/// peers file through `PeerStore::reload_if_changed`, so a grant or a revoke
/// lands without a restart; what needs a restart is turning the whole seam on,
/// which is the same rule the peer listener's socket already has.
static PROVIDER: OnceLock<Box<dyn FallbackProvider>> = OnceLock::new();

/// Install the provider for this process. Answers `false` when one was already
/// installed, in which case NOTHING changed, the caller is the boot path and a
/// second install is a bug there, not a condition to paper over.
pub fn install_provider(provider: Box<dyn FallbackProvider>) -> bool {
    PROVIDER.set(provider).is_ok()
}

/// The provider this build consults, if any. `None` until [`install_provider`]
/// runs, and then the dry-fleet arm is byte-for-byte what it is on `main`.
pub fn configured_provider() -> Option<&'static dyn FallbackProvider> {
    PROVIDER.get().map(std::convert::AsRef::as_ref)
}

/// The peer-lease provider this peers file describes, or `None` for a file that
/// describes a node with nobody to borrow from.
///
/// **What "sharing on" means on the BORROWING side.** A lease needs an explicit
/// act on both machines, and the act on this one is `allow.disclose`
/// ([`crate::peer::config::Allow::allow_disclose`]), "this peer may read my
/// requests in full". So a row with `disclose` on and an address to dial is what
/// makes this node able to borrow at all; a row with `inspect` on says the
/// opposite thing (that this node LENDS) and does not qualify.
///
/// The rule is "sharing on and at least one lease". The second half
/// is not readable from this file and deliberately so: a lease is minted by the
/// LENDER, the borrower's copy is a cached hint (see
/// [`crate::peer::lease::Ledger`]), and nothing persists one here. The provider
/// asks for a lease per request instead, which is also what keeps it from
/// serving against a lease the lender has since dropped.
pub fn peer_lease_provider(store: &PeerStore) -> Option<PeerLeaseProvider> {
    // `has_a_way_back` and not `has_endpoint`, the same predicate
    // `PeerLeaseProvider::try_serve` asks per request. They were two copies of
    // one filter and the copies disagreed: a node that restarted while its only
    // lender had no address installed NO provider at all, so the forwarder
    // `try_serve` would have found was never reached and the whole seam was
    // silently off until that lender announced an address of its own.
    let lenders = store
        .peers()
        .into_iter()
        .filter(|row| row.allow.allow_disclose && crate::peer::serve::has_a_way_back(row, store))
        .count();
    if lenders == 0 {
        return None;
    }
    Some(PeerLeaseProvider::new(store.path().to_path_buf()))
}

/// Read the peers file at `path` and install a [`PeerLeaseProvider`] if it
/// describes a node that may borrow. Answers which of the three happened.
///
/// A malformed peers file is an error, never a silent "no provider": that is the
/// same rule `PeerStore::open` already applies, and the two must not disagree.
pub fn install_peer_lease_provider(path: &Path) -> Result<Installed> {
    let store =
        PeerStore::open(path).with_context(|| format!("peer lease: reading {}", path.display()))?;
    let Some(provider) = peer_lease_provider(&store) else {
        return Ok(Installed::NothingToBorrowFrom);
    };
    if install_provider(Box::new(provider)) {
        Ok(Installed::Yes)
    } else {
        Ok(Installed::AlreadyInstalled)
    }
}

/// What [`install_peer_lease_provider`] did, as three named facts rather than a
/// bool a caller has to guess the meaning of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// This process now consults a peer-lease provider.
    Yes,
    /// The peers file names no peer this node may disclose to, so there is
    /// nothing to borrow from and the arm stays exactly as it is on `main`.
    NothingToBorrowFrom,
    /// A provider was already installed and this call changed nothing.
    AlreadyInstalled,
}
