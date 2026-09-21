//! The SERVE stream: someone else's account serves my request.
//!
//! Both halves live in this one file on purpose. The borrower's refusal and the
//! lender's backstop are THE SAME refusal, and a copy of a seven-element
//! security list in two files is a fact in two places that will drift; keeping
//! them in one module means a coder editing either reads both.
//!
//! # The disclosure this stream IS
//!
//! The lender sees the borrower's full request and response plaintext, prompts
//! included, because substituting a Bearer on ciphertext is not possible. That
//! is not a leak to be mitigated, it is what borrowing an account means. It
//! needs an explicit act on BOTH machines (`inspect` on the lender, `disclose`
//! on the borrower, both default false), so an unconfigured mesh is blind-only.
//!
//! Through a forwarder, a SERVE is a FRESH end-to-end Noise session inside a
//! TUNNEL: the forwarder holds ciphertext for a session it has no key to, and a
//! forwarder that lies about which node it reached fails the inner handshake
//! against the pinned static.
//!
//! # What the BORROWER does unconditionally, before any grant is consulted
//!
//! 1. **Strips its own credentials**, `authorization`, `x-api-key`,
//!    `proxy-authorization`, `cookie`, plus every hop-by-hop request header,
//!    from every frame before the frame is written. The hop-by-hop half is
//!    `src/proxy.rs`'s own predicate, never a second list here.
//! 2. **Refuses to open a SERVE at all** for the client-credential paths, and
//!    falls through to its own local path instead.
//!
//! Both are on the borrower, not the lender, and the difference is the whole
//! point: a lender-side refusal happens after the bytes are already in the
//! lender's process, its memory and its request log. The lender keeps the
//! identical check as a backstop against an older or malicious borrower, and
//! that is all it is.
//!
//! The scrub cannot be inherited from the existing path, and this is measured
//! rather than assumed: `build_upstream_headers` is the only code in this tree
//! that removes a client's own `authorization` and substitutes a pooled token,
//! its definition is `src/proxy.rs:3455`, its SINGLE call site is `:2526`, and
//! the seam a SERVE is built at is `:2311`, 215 lines upstream. Nothing
//! between them would stop a client credential going out to another host.
//!
//! # The seventh path is the one a coder who reads the array will miss
//!
//! Six prefixes live in `CLIENT_CREDENTIAL_PREFIXES` (`src/proxy.rs:820-827`)
//! and the seventh is a SEPARATE constant, `CLIENT_TOKEN_REFRESH_PATH`
//! (`:839`), `/v1/oauth/token`, whose own doc records the live defect that
//! omitting it caused: an exact compare let the trailing-slash spelling fall
//! through to the pooled path, putting our Bearer on a client's token
//! exchange. That path targets `platform.claude.com`, which a blind tunnel
//! carries for free and a SERVE must never touch, and it is matched with
//! `path_is_under` (the whole path or that path plus `/`, never a longer
//! identifier) rather than `starts_with`.
//!
//! Both constants went `pub(crate)` in `src/proxy.rs`, a visibility change in
//! `proxy.rs` and nowhere else, taken rather than the fallback (a peer-side
//! constant plus an element-for-element contract test), because one list with
//! two readers cannot drift and two lists with a test can. [`path_is_under`]
//! went `pub(crate)` with them, since a peer-side re-spelling of the matcher
//! would reintroduce exactly the `starts_with` defect the matcher exists to
//! fix. Never a silent copy.
//!
//! # What the LENDER forwards, which is an ALLOWLIST and not a filter
//!
//! The lender re-sends the relayed request through its own proxy, and it
//! once replayed **every** header the borrower supplied onto that
//! request. That is a hole with a measured shape rather than a theoretical one:
//! `build_upstream_headers` (`src/proxy.rs:3502`) removes `authorization`,
//! `x-api-key`, `accept-encoding` and the hop-by-hop names, and `cookie` is
//! none of those, so a borrower-supplied session cookie went out to Anthropic
//! on the lender's own TLS session, as would any header a borrower invented.
//!
//! So the lender forwards [`LENDER_FORWARDED_HEADERS`] and nothing else. An
//! allowlist rather than a longer denylist, because the two fail in opposite
//! directions: a name nobody thought of is dropped by the first and forwarded
//! by the second, and the set of names a borrower can invent is not
//! enumerable.
//!
//! This is the house pattern rather than a new idea, and the precedent is the
//! closest relative there is: `build_raw_relay_headers`
//! (`src/proxy.rs:3554`) carries `content-type`, `accept`, `user-agent` "and
//! nothing else" for the one other request in this tree built out of a client's
//! headers for a host whose credential is not the client's. This list is that
//! one plus `anthropic-version` and `anthropic-beta`, which a `/v1/messages`
//! relay needs and a token exchange does not.
//!
//! # The lender's per-org bucket, which is not a new mechanism
//!
//! A borrowed request counts against the LENDER's GCRA bucket for free, because
//! [`handle_serve_on`] sends it through the lender's own proxy and therefore
//! through the lender's own picker and its own
//! `Manager::throttle_send` call (`src/proxy.rs:2593`), which keys the
//! per-organization bucket through `Manager::throttle_bucket_key`
//! (`src/manager/throttle.rs:98`) and `bucket_key_for`
//! (`src/manager/throttle.rs:38`) off the SERVING account's `org_uuid`, the
//! lender's. There is no second bucket, no peer-keyed bucket and nothing to add:
//! one sender per org is preserved by construction, and the way to break it
//! would be to serve a relayed request anywhere other than through the lender's
//! own path. `tests/peer_lease.rs` measures it against a tightened per-org
//! bucket rather than asserting it in prose.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tcr_peer_wire::{
    Control, Hello, Lease, LeaseRefusal, PeerId, StreamHeader, StreamKind, Window, MAX_FRAME_BYTES,
    PROTO_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::fallback::Ask;
use crate::peer::config::{Endpoint, Locator, PeerRow, PeerStore};
use crate::peer::id::NodeKey;
use crate::peer::lease::Ledger;
use crate::peer::noise::{self, Handshake};
// ONE list, two readers. See the module docs: these are `pub(crate)` in
// `src/proxy.rs` precisely so this file holds no list and no matcher of its own.
use crate::proxy::{
    is_request_hop_by_hop, path_is_under, CLIENT_CREDENTIAL_PREFIXES, CLIENT_TOKEN_REFRESH_PATH,
};

/// **The borrower's refusal**: may a SERVE be opened for this path at all?
///
/// `false` means fall through to the local path, never an error page, because
/// the request is perfectly servable here, just not there.
///
/// Reads `CLIENT_CREDENTIAL_PREFIXES` and `CLIENT_TOKEN_REFRESH_PATH` from
/// `src/proxy.rs` through `path_is_under`, and holds no list of its own.
pub fn serve_is_allowed_for_path(path: &str) -> bool {
    // FIRST, and before any prefix compare below can mean anything: a path the
    // upstream URL parser would read differently than these compares do. See
    // [`relay_path_is_routable`], the review's H1, and note that it is asked
    // HERE rather than at each of this function's four call sites, so a fifth
    // caller cannot be the one that forgets.
    if !relay_path_is_routable(path) {
        return false;
    }
    if CLIENT_CREDENTIAL_PREFIXES
        .iter()
        .any(|base| path_is_under(path, base))
    {
        return false;
    }
    // This proxy's OWN local control surface, which is not an upstream path at
    // all. Behind it sit the local `/_tcr/` endpoints, including the one
    // privileged mutation that ADDS A LIVE CREDENTIAL, whose entire
    // authorization is `local_endpoint_gate` plus a content type
    // (`src/proxy.rs:1137`), that is, the fact that the caller reached
    // loopback. Relaying it would mean another host's request arriving at this
    // gate having satisfied it by construction. Matched with `path_is_under`,
    // the SAME matcher the proxy's own guard uses on this prefix
    // (`src/proxy.rs:2001`), so the two cannot disagree about what is local: a
    // spelling the proxy forwards upstream is a spelling this refuses to relay
    // for no reason, and a spelling the proxy answers locally is one a relay
    // must never reach.
    if path_is_under(path, crate::proxy::LOCAL_PREFIX) {
        return false;
    }
    // The seventh path, and the one a reader of the array above misses. Matched
    // the same way, so `/v1/oauth/token/` cannot fall through the way it once
    // fell through an exact compare.
    !path_is_under(path, CLIENT_TOKEN_REFRESH_PATH)
}

/// **May a relayed path be built into a URL at all?** The review's H1, and the
/// check that has to answer before [`serve_is_allowed_for_path`] means anything.
///
/// Every refusal above is a PREFIX COMPARE on the raw frame string, and the
/// string is a `String` the borrower chose. The lender then built its own
/// request with `format!("{upstream}{path}")`, so what went on the wire was
/// whatever `Url::parse` made of the pair, and that parser normalizes per
/// WHATWG. Measured on this repository's pinned `url`, with `upstream =
/// "http://127.0.0.1:3456"`:
///
/// ```text
/// /x/../_tcr/accounts/control       -> 127.0.0.1:3456 /_tcr/accounts/control
/// @169.254.169.254/latest/meta-data -> 169.254.169.254 /latest/meta-data
/// /v1/code\foo                      -> 127.0.0.1:3456 /v1/code/foo
/// ```
///
/// So three things passed the refusals above and arrived somewhere else. The
/// first reached the lender's own privileged local route having satisfied
/// `local_endpoint_gate` BY CONSTRUCTION, the caller is loopback, the target is
/// loopback, no `Sec-Fetch-*` header survives [`LENDER_FORWARDED_HEADERS`], and
/// `content-type: application/json` does, which is the exact outcome
/// [`serve_is_allowed_for_path`]'s own comment says it prevents. The second is
/// an SSRF with the lender's network position, link-local metadata included, on
/// a path that does not begin with `/` and therefore turns the authority into
/// userinfo. The third normalizes onto a `CLIENT_CREDENTIAL_PREFIXES` path.
///
/// **This is not a new class of defect in this tree, it is the same one twice.**
/// `src/proxy.rs` records it as measured in the same words and fixed it for the
/// local client with [`crate::proxy::path_is_ambiguous`], applied on the
/// forwarding path. The SERVE path never called it. So this asks the SAME
/// function rather than re-deriving the parser's rules here, one matcher, two
/// readers, for the reason the module docs give for the path lists.
///
/// # The escaped separators, which are this path's own case
///
/// `crate::proxy::path_is_ambiguous` deliberately leaves `%5c` alone and says
/// why: the URL parser does not decode it, so for the LOCAL forwarding path the
/// escaped spelling means exactly what it says. A relay is the other case. The
/// string this function guards is handed to a SECOND server, this Mac's own
/// proxy, over loopback, and then Anthropic's, and a server that decodes
/// `%2F` to `/` before its own routing reads `/v1/messages/..%2F_tcr` as
/// `/v1/messages/../_tcr`, which is the dot-segment escape this whole function
/// exists to refuse, arriving one decode later. Nothing in this tree can
/// promise what every hop's decoder does, and no legitimate Anthropic client
/// emits an escaped separator, so the escaped spellings are refused here.
///
/// Refused HERE and not by widening the proxy's matcher, for the reason that
/// matcher gives: the proxy routes on `uri.path()`, which it has already
/// parsed, so widening it would refuse local requests for no gain.
///
/// Three checks, and the rooted one is first: a path that does not start with
/// `/` is not a path at all for this purpose, whatever else is true of it.
pub fn relay_path_is_routable(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    // Case-insensitively, because `%2f` and `%2F` are the same escape and a
    // check that reads one spelling is a check a borrower spells the other way.
    let lowered = path.to_ascii_lowercase();
    if lowered.contains("%2f") || lowered.contains("%5c") {
        return false;
    }
    // A protocol-relative `//host/...` is rooted AND names an authority: `/`
    // then `/` is an empty first segment, which `path_is_ambiguous` has no
    // reason to object to and `Url::parse` reads as a host. Refused here rather
    // than by widening the proxy's own matcher, because the proxy never sees
    // one: its own path comes out of `uri.path()`, which has already resolved
    // the authority.
    if path.starts_with("//") {
        return false;
    }
    !crate::proxy::path_is_ambiguous(path)
}

/// **The borrower's scrub**, applied to every frame before it is written.
///
/// Removes every name in `CLIENT_CREDENTIAL_HEADERS` and every hop-by-hop
/// request header the proxy already refuses to forward
/// (`is_request_hop_by_hop`, `src/proxy.rs:3698`). Returns what it removed, so
/// the caller can assert on it: the gate for this is a frame that contains none
/// of them, watched red by deleting the scrub.
///
/// The hop-by-hop half is asked of `src/proxy.rs` rather than re-listed here.
/// Two reasons, and the second is the load-bearing one: a SERVE frame is a
/// request leaving this host, so every name that is wrong on the upstream hop is
/// wrong here for the same reason; and `proxy-authorization` is **a credential**
/// as well as a hop-by-hop header, so a second list is a second place for a
/// credential name to go missing.
pub fn scrub_client_credentials(headers: &mut HeaderMap) -> usize {
    // `HeaderMap::remove` drops EVERY value under the name, not the first, so a
    // client that sent `authorization` twice leaves nothing behind. The count is
    // header NAMES removed, which is what the gate asserts on.
    let credentials = CLIENT_CREDENTIAL_HEADERS
        .iter()
        .filter(|name| headers.remove(**name).is_some())
        .count();

    let hop_by_hop: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_request_hop_by_hop(name.as_str()))
        .cloned()
        .collect();
    let hops = hop_by_hop
        .iter()
        .filter(|name| headers.remove(*name).is_some())
        .count();

    credentials + hops
}

/// The header names the borrower removes as CREDENTIALS, in one place because
/// both the scrub and anything that asserts on the scrub must read the same
/// list. The hop-by-hop names are not here: those come from `src/proxy.rs`, see
/// [`scrub_client_credentials`].
///
/// - `x-api-key` because an api-key client authenticates with it alone, so
///   scrubbing only the Bearer would send the other credential shape untouched.
/// - `proxy-authorization` because this proxy accepts one
///   (`ProxyConfig::api_key`), so it is a credential for THIS hop that a relay
///   would otherwise hand to another host. It is also hop-by-hop, which is
///   belt and braces rather than a duplicate: it must not survive either check.
/// - `cookie` because a session cookie is a bearer credential in every way that
///   matters and nothing upstream of a lease needs one.
pub const CLIENT_CREDENTIAL_HEADERS: [&str; 4] = [
    "authorization",
    "x-api-key",
    "proxy-authorization",
    "cookie",
];

/// The header names the LENDER copies from a relayed request onto the request
/// it makes through its own proxy. Everything else is dropped.
///
/// An allowlist, for the reason in the module docs: the borrower's scrub is the
/// enforcement point, this is the lender's backstop, and a backstop written as
/// a denylist forwards every name nobody thought of.
///
/// - `content-type` and `accept`, because the request is JSON or SSE and the
///   answer's shape follows from them.
/// - `anthropic-version` and `anthropic-beta`, because they select the API
///   contract the borrower's client is written against; substituting the
///   lender's would answer a shape the borrower cannot parse.
/// - `user-agent`, because Anthropic's own rate-limit and abuse signals read
///   it, and a relayed request that claimed to be something else would be a
///   worse citizen than one that says what it is.
///
/// Nothing else. `host` and `content-length` are the lender's HTTP client's to
/// set and are wrong if inherited; `authorization`, `cookie`, `x-api-key` and
/// `proxy-authorization` are credentials; anything unnamed is a header a
/// borrower invented.
pub const LENDER_FORWARDED_HEADERS: [&str; 5] = [
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "user-agent",
];

/// Whether the lender copies this header name onto its own request.
///
/// Case-insensitive, because a header name is case-insensitive on the wire and
/// a borrower writes the frame: `Cookie` and `cookie` are one name, and an
/// exact compare here would be an allowlist with a trivial bypass.
pub fn lender_forwards_header(name: &str) -> bool {
    LENDER_FORWARDED_HEADERS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// The one HTTP method a relayed request may carry.
///
/// [`ServeRequest::method`] used to be built as the literal `"POST"` and read
/// by nobody, so a borrower that sent `GET` had it silently POSTed on the
/// lender's account. The field is honoured instead of removed because the
/// borrower's client really does choose the method and a lender must be able to
/// say which one it will serve. See [`handle_serve_on`], which refuses
/// anything else rather than rewriting it.
pub const SERVE_METHOD: &str = "POST";

/// The message of the lender's one log line per served request.
///
/// A constant rather than a literal in the `info!` because a gate greps for it
/// ([`handle_serve_on`] emits it, `tests/peer_boot.rs` asserts its field set),
/// and a gate keyed on a copy of a message is a gate that goes quiet the day
/// somebody rewords the message. The FIELDS are the contract as much as the
/// text: `peer`, `window`, `bytes`, `ms`, and nothing else.
pub const SERVED_LINE: &str = "peer serve: served a relayed request on this Mac's own account";

/// One account's utilization on one window, read from the lender's own fleet.
///
/// A trait rather than a `Manager` handle in the signature so the lender's half
/// can be driven without a fleet: the production implementation is
/// [`crate::manager::Manager`] and reads its accounts' quota, and a test hands
/// in a reader whose before/after answers are chosen.
///
/// The answer is **per account, positionally stable**: `select_with_group`
/// returns an index into a vector accounts are appended to and never removed
/// from, so index `i` is the same account across the two reads that bracket one
/// relayed request. A single scalar cannot carry the same fact, the fleet's
/// maximum can move because a DIFFERENT account's window moved, which is why
/// [`utilization_rise`] compares elementwise.
///
/// `None` at an index is "never measured", never "empty": see
/// [`utilization_rise`] for what that costs the lease.
pub trait WindowUtilization: Send + Sync {
    /// Every account's utilization on `window`, in account order.
    fn read(&self, window: Window) -> Vec<Option<f64>>;

    /// How a relayed request can be held inside `scope` on THIS node's fleet,
    /// which is the picker restriction.
    ///
    /// On the same trait as the utilization read because both are questions
    /// about the lender's own fleet that the serving leg has to ask without
    /// holding a `Manager` (see this trait's doc). The production answer is
    /// [`crate::manager::Manager`]'s, which reads its groups and its spill
    /// settings.
    ///
    /// **The default answers [`ScopeRestriction::Unenforceable`] for anything
    /// but [`tcr_peer_wire::LendScope::All`]**, and that direction is the
    /// decision: a reader that cannot resolve the lender's groups or its
    /// account labels cannot prove a request stayed inside the scope, and an
    /// unprovable restriction on somebody else's account is the failure this
    /// whole file exists to prevent. `handle_serve_on` refuses rather than
    /// serving out of scope.
    fn scope_restriction(&self, scope: &tcr_peer_wire::LendScope) -> ScopeRestriction {
        match scope {
            tcr_peer_wire::LendScope::All => ScopeRestriction::Unrestricted,
            tcr_peer_wire::LendScope::Group(_) | tcr_peer_wire::LendScope::Accounts(_) => {
                ScopeRestriction::Unenforceable
            }
        }
    }

    /// How much of `window` this node's guard band leaves **on `scope`'s
    /// accounts only**, or `None` for a reader that cannot answer per scope.
    ///
    /// The rule is: "the fraction is of the SCOPE's headroom (one account's
    /// window, the set's pooled window, the group's pooled window), computed by
    /// `lendable_fraction` over the scope's accounts only." A measurement
    /// found `Ledger::note_owner_headroom` with no production caller and the
    /// ledger's headroom keyed by window alone, so a group-scoped lease was
    /// clamped by (and its owner guard decided against) the whole fleet.
    ///
    /// On this trait, and not on a `Manager` handle, for the reason the trait
    /// exists: the serving leg asks it without holding a fleet, and a test hands
    /// in a reader whose answers are chosen. The production implementation is
    /// [`crate::manager::Manager::lendable_fraction`], which is the SAME
    /// function `lent_to` and the picker restriction already agree with about
    /// which accounts a scope draws from.
    ///
    /// **`None` is not zero.** A reader that cannot answer leaves the ledger's
    /// last `LendScope::All` note in place as the ceiling
    /// (`Ledger::headroom_for`), which is what every pre-decision-12 lease
    /// already meant; answering `0.0` here would refuse every lease a test
    /// reader is behind.
    fn lendable(&self, _scope: &tcr_peer_wire::LendScope, _window: Window) -> Option<f64> {
        None
    }

    /// The same figure for a `hand` grant: what may be lent out of the accounts
    /// whose BEARER can leave this Mac, which is a different set from the one
    /// above and often a smaller one.
    ///
    /// A hand lease is spent on the borrower's Mac with a token this one hands
    /// over, so a figure measured on an account that cannot be handed over
    /// funds a lease no bearer backs. `Ledger::grant` clamps a hand grant by
    /// this as well, which is what makes "funded only if a bearer exists" hold.
    ///
    /// **`None` is "this reader cannot answer" and leaves the clamp off**, the
    /// same direction [`Self::lendable`] takes and for the same reason: a
    /// reader with no fleet behind it (every test one, and
    /// [`NoFleetUtilization`]) would otherwise refuse every hand lease. The
    /// production answer is `Manager`'s and is never `None`.
    fn lendable_by_hand(&self, _scope: &tcr_peer_wire::LendScope, _window: Window) -> Option<f64> {
        None
    }
}

/// How the lender's own proxy can be made to pick inside a lease's scope.
///
/// # Why this is three answers and not a list of accounts
///
/// The relayed request re-enters the lender's OWN proxy over loopback
/// ([`handle_serve_on`]), so the only thing that crosses into the picker is a
/// header. The proxy reads two account-selection headers and both are STRICT:
/// [`crate::proxy::GROUP_HEADER_NAME`], which `Manager::select_with_group`
/// holds inside the named group (`strict_group`, `src/manager/select.rs:1139`,
/// for every group that is reserved or not a spill group), and
/// [`crate::proxy::ACCOUNTS_HEADER_NAME`], which holds the request inside a
/// named SET of accounts by benching every account outside it before the
/// rotation loop starts.
///
/// [`Self::Unenforceable`] is what is left: a scope this build cannot prove a
/// request stayed inside, a spill-and-not-reserved group, a group with no
/// member, an account set naming nothing this fleet carries. It is a refusal
/// rather than a best-effort, because the alternative is serving a scoped
/// lease on whichever account the picker liked, which is exactly the thing the
/// operator scoped it to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeRestriction {
    /// Every account may serve it, [`tcr_peer_wire::LendScope::All`].
    Unrestricted,
    /// Send it with this group on [`crate::proxy::GROUP_HEADER_NAME`], which
    /// the picker enforces strictly.
    Group(String),
    /// Send it with these account labels on
    /// [`crate::proxy::ACCOUNTS_HEADER_NAME`], which the picker enforces
    /// strictly. Already narrowed to the labels the lender's own fleet
    /// carries, so an empty list never reaches here, a set that named no
    /// local account is [`Self::Unenforceable`] instead, exactly as an empty
    /// group is.
    Accounts(Vec<String>),
    /// This build cannot hold the request inside the scope, so it must not
    /// serve it at all.
    Unenforceable,
}

/// The utilization rise one relayed request caused, as the largest rise any
/// single account showed between the two reads.
///
/// The largest single rise rather than the sum: one request is served by ONE
/// account (the lender's own picker chose it), so a sum over a fleet whose
/// other accounts were probed in the same interval would charge the lease for
/// the lender's own traffic.
///
/// **An index that was unmeasured before contributes nothing**, so the first
/// borrowed request against a never-probed fleet charges
/// [`crate::peer::lease::MIN_DEBIT`] rather than the whole of the first
/// measurement. That is the honest answer: a rise is a difference between two
/// measurements and there is only one here. The other direction would charge a
/// borrower for everything the owner had already spent.
///
/// A negative difference, a window that reset under the request, which these
/// headers really do report, is not a credit: it contributes `0.0` and
/// `Ledger::debit` then charges `MIN_DEBIT`.
pub fn utilization_rise(before: &[Option<f64>], after: &[Option<f64>]) -> f64 {
    before
        .iter()
        .zip(after.iter())
        .filter_map(|(before, after)| match (before, after) {
            (Some(before), Some(after)) => Some((after - before).max(0.0)),
            _ => None,
        })
        .fold(0.0_f64, f64::max)
}

/// A reader for a node that has no fleet to measure, every window unmeasured.
///
/// Named rather than an empty `Vec` at each call site: "this node cannot
/// measure its own utilization" is a fact a reader of the boot path should see
/// spelled out, and it is the honest state of a node that lends nothing.
pub struct NoFleetUtilization;

impl WindowUtilization for NoFleetUtilization {
    fn read(&self, _window: Window) -> Vec<Option<f64>> {
        Vec::new()
    }
}

/// How much body fits in ONE frame, which is now a chunk size and no longer a
/// ceiling on a body.
///
/// [`MAX_FRAME_BYTES`] is the whole frame a `u16` prefix can describe, and a
/// Noise transport message pays a 16-byte authentication tag inside it.
///
/// It used to be the ceiling on a whole body, and that was the defect: every
/// borrowed answer over 64 KiB, which any long completion is and which Claude
/// Code's streaming answers always are, was bought on the lender's account,
/// debited, and handed back as a 502 with no content. A body now travels as a
/// run of frames this size terminated by an empty one ([`send_body`],
/// [`recv_body`]), so the size a body may be is [`MAX_RELAYED_BODY_BYTES`] and
/// this is only how much of it one frame carries.
pub const MAX_BODY_BYTES: usize = MAX_FRAME_BYTES - 16;

/// The ceiling on a whole relayed body, request or answer, across every frame
/// it takes.
///
/// A bound is still needed, because the frames arrive before anything has said
/// how many there will be and a peer that keeps sending them would otherwise
/// grow this process's memory without limit. It is the size of a very large
/// completion rather than the size of one frame, and a body over it is refused
/// by name with both numbers.
pub const MAX_RELAYED_BODY_BYTES: usize = 32 * 1024 * 1024;

/// The SERVE stream's own flow version, which is NOT [`PROTO_VERSION`].
///
/// The two answer different questions and move on different clocks:
/// [`PROTO_VERSION`] is what a node advertises in discovery and records at
/// pairing, and bumping it to describe a change to one stream would re-date
/// every pinned row on the mesh. This is the order of frames on a SERVE stream
/// and nothing else.
///
/// **1 is the flow with no ack**: header, request, body, reply. **2 is this
/// one**: header, request, [`ServeAck`], body, reply. The difference is not
/// cosmetic and cannot be negotiated silently, a borrower that writes the body
/// before the ack has already made its request unrepeatable, which is the whole
/// defect the ack exists to fix. So a lender REFUSES any other flow by name
/// (see [`handle_serve_on`]) rather than guessing at the order, and a borrower
/// that reads a closed stream where the ack should be says the same thing in
/// its log.
pub const SERVE_FLOW: u16 = 2;

/// The metadata frame of a relayed request. **The body is not in it**, it
/// travels as the next frame, raw, because a `Vec<u8>` through JSON is three to
/// four times its own size and this stream's frames are `u16`-bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeRequest {
    /// Which lease pays for this request.
    pub lease_id: u128,
    /// One request, one id, for the whole path, what makes a retried or
    /// diamond-delivered relay debit once. The same id as the
    /// [`StreamHeader::request_id`] that opened the stream.
    pub request_id: u128,
    /// The HTTP method, as the borrower's client sent it.
    pub method: String,
    /// The already query-stripped request path.
    pub path: String,
    /// The query string the borrower's client sent, without its `?`, when it
    /// sent one.
    ///
    /// Apart from [`Self::path`] because the lender's own path gates match on
    /// the path, and a query string that could decide one of those is a query
    /// string that decides routing. The two are put back together only on the
    /// URL the lender builds for its own proxy.
    ///
    /// `#[serde(default)]` so a frame from a build written before this field
    /// existed still parses, and reads as the client having sent no query,
    /// which is exactly what that build meant.
    #[serde(default)]
    pub query: Option<String>,
    /// Header names and values, **after the borrower's scrub**. Pairs rather
    /// than a map because a request may legitimately repeat a name.
    pub headers: Vec<(String, String)>,
    /// How many bytes the body frames that follow this one carry in total.
    pub body_bytes: usize,
    /// The wire version the borrower speaks, so a lender can refuse a frame it
    /// would have to guess at.
    pub proto: u16,
    /// The order of frames the borrower is going to use. See [`SERVE_FLOW`].
    ///
    /// `#[serde(default)]` so a frame from a build written before this field
    /// existed parses and reads 0, which is what lets the lender refuse it BY
    /// NAME with both numbers rather than fail to deserialize and close with a
    /// message about JSON. 0 is never a flow this build speaks.
    #[serde(default)]
    pub flow: u16,
}

/// The lender's answer to "may I send you this request", and the frame that
/// makes a borrowed request repeatable until it is sent.
///
/// It sits between the request frame and the body, and it is the ONLY thing
/// that moves a borrow from "nothing has left this Mac" to "this may already
/// have run". Before it existed the borrower wrote the body first, so a lender
/// that refused at its header gate, or at any of `handle_serve_on`'s own gates,
/// produced a no-retry 502 telling the client its request may have been billed
/// when nothing had been sent anywhere.
///
/// A lender writes it once its gates have passed and BEFORE it sends anything
/// upstream. A stream that closes, or goes quiet, where this frame belongs is a
/// lender that took nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServeAck {
    /// Send the body. This lender has taken the request.
    Accepted,
    /// Refused before the body, in the lease vocabulary the borrower already
    /// reads. Nothing crossed, so the next lender may be asked.
    Refused { refusal: LeaseRefusal },
}

/// The lender's answer to one relayed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServeReply {
    /// Served on the lender's own account. The body follows as a run of frames
    /// terminated by an empty one, and `body_bytes` is what they add up to.
    Served {
        status: u16,
        headers: Vec<(String, String)>,
        body_bytes: usize,
    },
    /// Refused, in the lease vocabulary the borrower already reads.
    /// [`LeaseRefusal::InFlightFull`] is the one that means "retry in a moment"
    /// rather than "stop asking".
    ///
    /// Every refusal a lender decides BEFORE the body is a [`ServeAck`] now.
    /// What is left here is the one that can only be known after it: a body
    /// whose length does not match what the request frame promised, which the
    /// lender refuses without sending anything upstream, so it is still "not
    /// this lender" and still provably unrun.
    Refused { refusal: LeaseRefusal },
}

/// Build the metadata frame for one relayed request, scrubbing as it goes.
///
/// `headers` is whatever the caller holds. It is scrubbed IN this function
/// rather than by its caller, so there is no order of operations for a future
/// caller to get wrong: a credential cannot reach a [`ServeRequest`] because the
/// only constructor removes it. [`Ask`] carries no headers at all today, which
/// is the outer half of the same defence, this is the half that survives the
/// day something at the seam does hold them.
pub fn serve_request_from(
    ask: &Ask<'_>,
    headers: &HeaderMap,
    lease_id: u128,
    request_id: u128,
) -> Result<ServeRequest> {
    if !serve_is_allowed_for_path(ask.path) {
        bail!(
            "peer serve: {} is a path a relay must never carry, so no frame is built for it",
            ask.path
        );
    }
    // The METHOD, refused here and not rewritten. This function used to write
    // the literal `SERVE_METHOD` into the frame whatever the client had sent,
    // which is how a `GET` came to be POSTed on somebody else's account. The
    // seam refuses a non-POST before it builds an `Ask` at all
    // (`src/proxy.rs`), so this is the second half of that defence and the half
    // that holds for a caller written later: the only constructor of a
    // `ServeRequest` cannot produce one whose method the lender will refuse.
    if !ask.method.eq_ignore_ascii_case(SERVE_METHOD) {
        bail!(
            "peer serve: {} is not a method a relayed request may carry ({SERVE_METHOD} or \
             nothing); refused rather than rewritten",
            ask.method
        );
    }
    // The whole body, not one frame of it: a body is chunked over as many
    // frames as it takes ([`send_body`]), so what is refused here is a request
    // larger than this stream carries at all rather than one larger than a
    // `u16` prefix. A real request over the old one-frame cap was a routine
    // shape, and refusing it meant the mesh declined exactly the long
    // conversations it was built for.
    if ask.body.len() > MAX_RELAYED_BODY_BYTES {
        bail!(
            "peer serve: this request's body is {} bytes and a relayed request carries {}; \
             refused rather than truncated",
            ask.body.len(),
            MAX_RELAYED_BODY_BYTES
        );
    }

    let mut scrubbed = headers.clone();
    scrub_client_credentials(&mut scrubbed);
    // AND THEN NARROWED TO THE ALLOW LIST, which is what actually crosses the
    // host boundary. The scrub above removes the names that are credentials;
    // this keeps only the names a lender would forward anyway
    // ([`LENDER_FORWARDED_HEADERS`]), so a header a borrower invented is not
    // sent to another Mac merely because nobody has classified it yet. The
    // lender applies the same list to what it receives: two readers of one
    // rule, on either side of the wire, and the borrower's is the one that
    // decides what leaves this machine.
    let headers = scrubbed
        .iter()
        .filter(|(name, _)| lender_forwards_header(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();

    Ok(ServeRequest {
        lease_id,
        request_id,
        // The client's own method, which the check above has proved is the one
        // method this build relays. Written from the `Ask` rather than from the
        // constant so the frame says what the client asked for and a future
        // build that relays a second method has one place to widen.
        method: ask.method.to_string(),
        path: ask.path.to_string(),
        query: ask.query.map(str::to_string),
        headers,
        body_bytes: ask.body.len(),
        proto: PROTO_VERSION,
        flow: SERVE_FLOW,
    })
}

/// Write a body as a run of frames terminated by an empty one.
///
/// **The terminator is what makes this readable at all.** The reader cannot
/// count frames, the count is not on the wire ahead of them, and it must not
/// trust the byte total in the request or reply frame either: that figure is a
/// promise the sender makes and the reader CHECKS, so using it to decide when
/// to stop reading would make it unfalsifiable. An empty frame is a frame, it
/// costs a 16-byte tag, and a body of zero bytes is exactly the terminator on
/// its own.
async fn send_body<S>(stream: &mut S, session: &mut noise::PeerSession, body: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    for chunk in body.chunks(MAX_BODY_BYTES) {
        noise::send_encrypted(stream, &mut session.transport, chunk).await?;
    }
    noise::send_encrypted(stream, &mut session.transport, &[]).await
}

/// Read a body written by [`send_body`].
///
/// Bounded by [`MAX_RELAYED_BODY_BYTES`] as it goes rather than at the end: a
/// peer that keeps sending frames is refused when it crosses the bound, not
/// after this process has already held the bytes.
async fn recv_body<S>(stream: &mut S, session: &mut noise::PeerSession) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    loop {
        let frame = noise::recv_encrypted(stream, &mut session.transport).await?;
        if frame.is_empty() {
            return Ok(body);
        }
        if body.len() + frame.len() > MAX_RELAYED_BODY_BYTES {
            bail!(
                "peer serve: this body is past {MAX_RELAYED_BODY_BYTES} bytes and is still \
                 arriving; refused rather than held"
            );
        }
        body.extend_from_slice(&frame);
    }
}

/// Open one SERVE to a lender and return its answer.
///
/// Refuses before opening anything when [`serve_is_allowed_for_path`] says no,
/// and scrubs before writing the first frame. The lease is the borrower's
/// cached copy; the lender's ledger is what actually decides.
///
/// `None` is "not me" in every case a lease could not be spent, no address that
/// answers, `disclose` not granted here, a refusal frame, because the caller's
/// next rung is the honest 429 this proxy already has and that is a better
/// answer than anything invented here. A protocol failure mid-stream IS an
/// error: it is not the same fact as "this lender said no".
///
/// The node key comes from the peers file's own directory, the way
/// `tcr peer id` resolves it (`src/main.rs:868`), so a test that points
/// `--peers` at a temp dir points this at the same one.
///
/// # `headers` is the borrower's own, and it is scrubbed here
///
/// [`Ask`] carries none today, and `crate::fallback`'s module doc says why: the
/// only code that strips a client credential sits 215 lines downstream of the
/// seam an [`Ask`] is built at. So the production caller hands in an empty map,
/// and this parameter exists for the two cases an empty map cannot serve, the
/// day something at the seam does hold headers, and a test that puts a
/// credential in scope so [`scrub_client_credentials`]'s removal is a
/// MEASUREMENT rather than an absence over an empty map. It was
/// `&HeaderMap::new()` inline, which made the scrub on this path untestable and
/// therefore unwatched.
/// How long the whole borrow may take before the client is released: the
/// connect, the handshake, both frames out, the lender's reply and its body.
///
/// **One deadline over the whole exchange, not one per hop.** The review's
/// MEDIUM was that this path had none anywhere, a bare `TcpStream::connect`,
/// an unbounded `dial_handshake` and an unbounded wait for the reply, on the
/// answer path of a live client's request, while the listener's own half
/// ([`crate::peer::listener::HANDSHAKE_TIMEOUT`]) and the blind-egress carry
/// ([`crate::peer::egress::CARRY_SETUP_TIMEOUT`]) each carry one. Per-hop
/// deadlines would sum to a bound no caller can state; the client is waiting on
/// the total, so the total is what is bounded.
///
/// Ten seconds, and deliberately LONGER than the carry's five: a carry runs
/// after the direct path has already failed, while a borrow is this request's
/// first and only attempt at an answer, and a borrow that gave up before a
/// sleeping laptop's radio woke would spend a lease nobody could use.
///
/// # The operator's setting, and this constant's new job
///
/// [`crate::peer::config::PeerFile::borrow_timeout_ms`] is now the value
/// [`open_serve`] reads per call, off the [`PeerStore`] it is already handed,
/// so a lowered timeout takes effect without a restart, exactly like `via`.
/// This constant is what a fresh install starts at:
/// `PeerFile::borrow_timeout_ms`'s own serde default mirrors it, so a peers
/// file with no opinion behaves exactly as it did before that field existed.
pub const BORROW_TIMEOUT: Duration = Duration::from_millis(10_000);

pub async fn open_serve(
    lender: &PeerId,
    lease: &Lease,
    ask: &Ask<'_>,
    headers: &HeaderMap,
    store: &PeerStore,
) -> Result<Borrowed> {
    let deadline = Duration::from_millis(store.file().borrow_timeout_ms);
    open_serve_within(lender, lease, ask, headers, store, deadline).await
}

/// What one borrow from one lender came to.
///
/// **Three outcomes and not two, because the third one costs money.** This was
/// `Option<Response>`: `None` meant "not me", and a borrow that hit the
/// deadline answered it, so the caller offered the SAME body to the next
/// lender with a fresh request id. A lender that took the request and was
/// merely slow therefore had it executed and billed on its account, and again
/// on another peer's.
///
/// So the fact the caller needs is not "did an answer come back" but "did the
/// body cross". Once it has, this Mac cannot know whether the request ran, and
/// the one safe answer is to stop: no other lender, and no local re-send.
pub enum Borrowed {
    /// The lender answered, and this is its answer.
    Served(Response),
    /// Nothing crossed to this lender: no address that answers, `disclose` not
    /// granted here, a refusal frame, or a deadline reached before the body
    /// was written. The next lender may be asked.
    NotThisLender,
    /// The body crossed and no usable answer came back. The request may have
    /// been executed upstream, so it is never sent again, by anybody.
    DeliveredUnknown,
}

impl Borrowed {
    /// The answer, when there is one. For the callers that only ever ask
    /// "did this borrow produce a response".
    pub fn served(self) -> Option<Response> {
        match self {
            Self::Served(response) => Some(response),
            Self::NotThisLender | Self::DeliveredUnknown => None,
        }
    }
}

/// [`open_serve`] under an explicit deadline.
///
/// Exists so the deadline is a VALUE rather than a constant read inside the
/// function: a gate can then prove that a lender which accepts and never
/// answers releases the client, in a test that finishes in a fraction of a
/// second instead of ten, and the operator's own setting has somewhere to
/// arrive.
///
/// A deadline reached is never an error: "this lender did not answer in time"
/// is a fact about this lender, while an `Err` would be read as a protocol
/// failure and logged as one. WHICH outcome it is depends on one thing, whether
/// the body had already crossed when the clock ran out: see [`Borrowed`].
pub async fn open_serve_within(
    lender: &PeerId,
    lease: &Lease,
    ask: &Ask<'_>,
    headers: &HeaderMap,
    store: &PeerStore,
    deadline: Duration,
) -> Result<Borrowed> {
    // OUTSIDE the deadline, and first: the path refusal is unconditional and
    // decides without touching the network, so putting it inside a timeout
    // would only make it possible for a clock to change its answer.
    if !serve_is_allowed_for_path(ask.path) {
        tracing::debug!(
            path = ask.path,
            "peer serve: refused before opening anything, this path is never relayed"
        );
        return Ok(Borrowed::NotThisLender);
    }
    // THE FLAG THE DEADLINE READS. `timeout` cancels `borrow_once` at an await
    // point and takes its return value with it, so "had the body crossed?"
    // cannot be answered from what it returns. It is written by the one line
    // that writes the body frame, and read here by both the timeout arm and
    // the error arm.
    let delivered = AtomicBool::new(false);
    match tokio::time::timeout(
        deadline,
        borrow_once(lender, lease, ask, headers, store, &delivered),
    )
    .await
    {
        Ok(Ok(outcome)) => Ok(outcome),
        // A protocol failure AFTER the body crossed is the same fact as a
        // deadline after it: this Mac does not know whether the request ran,
        // so it is not sent again. Reported at warn rather than returned as an
        // `Err`, because an `Err` is what the caller reads as "ask the next
        // lender".
        Ok(Err(err)) if delivered.load(Ordering::SeqCst) => {
            tracing::warn!(
                peer = %lender.display(),
                error = %err,
                path = ask.path,
                "peer serve: the borrowed request was delivered and the exchange then \
                 failed; it is NOT offered to another lender, because it may have run"
            );
            Ok(Borrowed::DeliveredUnknown)
        }
        Ok(Err(err)) => Err(err),
        Err(_) if delivered.load(Ordering::SeqCst) => {
            tracing::warn!(
                peer = %lender.display(),
                deadline_ms = deadline.as_millis(),
                path = ask.path,
                "peer serve: the borrowed request was delivered and the lender did not \
                 answer within the deadline; it is NOT offered to another lender, \
                 because it may have run"
            );
            Ok(Borrowed::DeliveredUnknown)
        }
        Err(_) => {
            tracing::warn!(
                peer = %lender.display(),
                deadline_ms = deadline.as_millis(),
                path = ask.path,
                "peer serve: the lender did not take the borrow within the deadline; \
                 nothing crossed, so the client is released and the next lender may \
                 be asked"
            );
            Ok(Borrowed::NotThisLender)
        }
    }
}

/// The borrow itself, from the first socket to the last body byte, everything
/// [`open_serve_within`]'s one deadline covers. See that function.
async fn borrow_once(
    lender: &PeerId,
    lease: &Lease,
    ask: &Ask<'_>,
    headers: &HeaderMap,
    store: &PeerStore,
    delivered: &AtomicBool,
) -> Result<Borrowed> {
    store.reload_if_changed();
    let Some(row) = store.row(lender) else {
        return Ok(Borrowed::NotThisLender);
    };
    if !row.allow.allow_disclose {
        tracing::debug!(
            peer = %lender.display(),
            "peer serve: this Mac has not granted `disclose` to that peer, so its requests \
             stay here"
        );
        return Ok(Borrowed::NotThisLender);
    }

    let request_id = random_u128()?;
    let request = serve_request_from(ask, headers, lease.lease_id, request_id)?;

    let key = NodeKey::load_or_mint(&node_key_dir(store))
        .context("peer serve: this node has no keypair to open a SERVE with")?;
    // `dial_peer_reaching` and not `dial_peer`: a lender whose own addresses
    // have all gone stale is still reachable through a Mac this node may ask
    // to carry, and the handshake below is unchanged either way, it runs
    // against the LENDER's pinned key over whatever stream came back, which is
    // what makes a forwarder blind.
    let Some(mut stream) = dial_peer_reaching(&row, store).await else {
        return Ok(Borrowed::NotThisLender);
    };
    let mut session = noise::dial_handshake(
        &mut stream,
        key.secret_bytes(),
        Handshake::Return,
        Some(&lender.0),
        None,
    )
    .await
    .context("peer serve: the handshake with the lender failed")?;

    let header = StreamHeader {
        kind: StreamKind::Serve,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id,
    };
    send_json(&mut stream, &mut session, &header).await?;
    send_json(&mut stream, &mut session, &request).await?;

    // THE ACK, AND WHY THE BODY WAITS FOR IT. Every refusal on the lender's
    // half happens after it has read this request frame: its header gate, its
    // path, method, version and request-id gates, and its lease. All of them
    // close the stream without an answer. A borrower that had already written
    // the body could not tell any of them from a lender that took the request
    // and died with it, so it told its client the request may have been billed
    // and skipped every remaining lender, over a request nothing had sent
    // anywhere. See [`ServeAck`].
    let ack: ServeAck = match recv_json(&mut stream, &mut session).await {
        Ok(ack) => ack,
        Err(err) => {
            // NOT an `Err` out of this function: "this lender answered nothing"
            // is a fact about the lender, and the body is still here. A build
            // older than [`SERVE_FLOW`] reads exactly like this, because it is
            // waiting for a body frame this one will not send until it has been
            // acknowledged, so the version is named here rather than guessed
            // at.
            tracing::debug!(
                peer = %lender.display(),
                error = %err,
                flow = SERVE_FLOW,
                "peer serve: the lender did not acknowledge taking this request, so it has \
                 none of it; a lender that speaks an older SERVE flow reads the same way"
            );
            return Ok(Borrowed::NotThisLender);
        }
    };
    if let ServeAck::Refused { refusal } = ack {
        tracing::info!(
            peer = %lender.display(),
            refusal = ?refusal,
            "peer serve: the lender refused this relayed request before its body was sent"
        );
        return Ok(Borrowed::NotThisLender);
    }

    send_body(&mut stream, &mut session, &ask.body).await?;
    // THE LINE THAT MAKES THIS REQUEST UNREPEATABLE. Everything above can fail
    // with nothing having left this Mac; from here on the lender holds the
    // body and may already have put it on its own account, so no later failure
    // may be reported as "not me". See [`Borrowed`].
    delivered.store(true, Ordering::SeqCst);

    let reply: ServeReply = recv_json(&mut stream, &mut session).await?;
    match reply {
        ServeReply::Refused { refusal } => {
            tracing::info!(
                peer = %lender.display(),
                refusal = ?refusal,
                "peer serve: the lender refused this relayed request"
            );
            // A REFUSAL IS "NOT ME" EVEN THOUGH THE BODY CROSSED, and it is
            // the one case where that is safe: the lender answers
            // `ServeReply::Refused` from `handle_serve_on` before it sends
            // anything upstream, so the request provably did not run. Every
            // other post-delivery outcome is `DeliveredUnknown`.
            Ok(Borrowed::NotThisLender)
        }
        ServeReply::Served {
            status,
            headers,
            body_bytes,
        } => {
            let body = recv_body(&mut stream, &mut session).await?;
            if body.len() != body_bytes {
                bail!(
                    "peer serve: the lender promised {body_bytes} body bytes and sent {}",
                    body.len()
                );
            }
            Ok(Borrowed::Served(response_from(status, &headers, body)?))
        }
    }
}

/// The lender's half: serve one relayed request on THIS node's own account.
///
/// Runs this node's OWN picker, attaches its OWN Bearer, pays its OWN per-org
/// throttle bucket, and debits the lease from the utilization rise it observes.
///
/// **This improves the one-sender-per-org property rather than degrading it**:
/// the lender is the only sender on its own account, through the one bucket
/// that already exists and is already counted, so a borrowed request is ADDED
/// to it. (A blind tunnel is the asymmetric case, it gives an org a sender it
/// did not have, from another address. That is the feature working as designed,
/// and it is why a gateway's caps are per-peer byte caps.)
///
/// Keeps the borrower's path refusal as a backstop. It is not the enforcement
/// point.
///
/// # This signature cannot serve a request, and that is reported rather than
/// worked around
///
/// A lender's half needs the stream the request arrived on, the session that
/// authenticated it, the ledger that funds it and somewhere to send it. This
/// signature has the peer and the peers file, so what it can answer is the
/// lender's ADMISSION question, may this peer open a SERVE here at all, which
/// is a real check with a real answer and is the first thing
/// [`handle_serve_on`] asks. [`handle_serve_on`] is the whole of the lender's
/// half, the way
/// `pair::mint_invite_as` is the whole of an invite the skeleton's `mint_invite`
/// could not build either.
pub async fn handle_serve(peer: &PeerId, store: &PeerStore) -> Result<()> {
    store.reload_if_changed();
    let Some(row) = store.row(peer) else {
        bail!(
            "peer serve: {} is not pinned here, so it may not open a SERVE",
            peer.display()
        );
    };
    if !row.allow.inspect {
        bail!(
            "peer serve: {} has not been granted `inspect` here, so SERVE is not on offer \
             at all (`tcr peer allow <peer> inspect on`)",
            peer.display()
        );
    }
    Ok(())
}

/// The lender's half, on the stream the request arrived on.
///
/// `upstream` is where the relayed request is sent, and it is **this node's own
/// proxy address**. That is the whole of "own picker, own Bearer, own bucket":
/// the request re-enters the same handler every local client uses, so account
/// selection, the pooled Bearer substitution and the per-organization GCRA
/// (`Manager::throttle_send`, keyed by `Manager::throttle_bucket_key` off the
/// SERVING account's org) all happen exactly once, in the one place they are
/// already written and already tested. There is no second picker and no second
/// bucket to keep in step.
///
/// Reads one [`ServeRequest`] plus its body frame, answers one [`ServeReply`]
/// plus its body frame, and returns. One request per stream: a stream that
/// carries two has to answer what happens to the second when the first is
/// refused, and the answer nobody has to reason about is that there is no
/// second.
///
/// # `utilization` is the measured debit, and it is why the signature grew
///
/// The skeleton debited through `Ledger::debit(.., 0.0)`, always
/// [`crate::peer::lease::MIN_DEBIT`], a constant whose own doc calls itself a
/// guess, because the delta needs the serving account's utilization before and
/// after and this signature held no way to read one. It holds one now: the
/// window is read through [`WindowUtilization`] on either side of the serve and
/// [`utilization_rise`] turns the pair into the figure the lease is charged.
/// The production reader is the lender's own `Manager`; nothing else on the
/// manager is reached from here.
///
/// # `header_request_id` is the id the STREAM was opened under
///
/// [`ServeRequest::request_id`] is documented as "the same id as the
/// [`StreamHeader::request_id`] that opened the stream" and the lender never
/// compared them, which the review names as an edge of H2: the stream gate's
/// dedup admits the HEADER's id, so a fresh header id carrying a repeated frame
/// id walked past it. They are compared here, where both are in scope, and a
/// mismatch is refused rather than reconciled, there is no honest way to pick
/// which of two ids a request is.
pub async fn handle_serve_on<S>(
    stream: &mut S,
    session: &mut noise::PeerSession,
    store: &PeerStore,
    serving: &crate::peer::listener::LeaseServing,
    header_request_id: u128,
    now_ms: i64,
    from: std::net::SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The three things a lender's half cannot derive, as the ONE struct the
    // listener already assembles them into. They were three parameters and the
    // request id made eight, which is one over clippy's bound, and an
    // `#[allow]` on a signature whose arguments really are a struct would be
    // silencing the lint rather than answering it.
    let ledger: &std::sync::Mutex<Ledger> = &serving.ledger;
    let upstream: &str = &serving.upstream;
    let utilization: &dyn WindowUtilization = serving.utilization.as_ref();
    handle_serve(&session.peer, store).await?;

    let request: ServeRequest = recv_json(stream, session).await?;

    if request.proto != PROTO_VERSION {
        bail!(
            "peer serve: this frame says wire version {} and this build speaks {}",
            request.proto,
            PROTO_VERSION
        );
    }
    // THE FLOW, REFUSED BY NAME AND NEVER GUESSED AT. A borrower older than
    // [`SERVE_FLOW`] writes its body immediately after this frame and waits for
    // a reply, so serving it would mean reading a body nobody acknowledged and
    // leaving that borrower to call every outcome "may have been billed". The
    // missing field reads 0, which is not a flow, and both numbers are in the
    // line so an operator knows which Mac to upgrade.
    if request.flow != SERVE_FLOW {
        bail!(
            "peer serve: {} speaks SERVE flow {} and this build speaks {SERVE_FLOW}; refused \
             rather than served, because the two disagree about when the body is written \
             (upgrade tcr on that Mac)",
            session.peer.display(),
            request.flow
        );
    }
    // THE BACKSTOP, not the enforcement point: the borrower refuses these paths
    // before it opens anything, which is the check that matters because a
    // lender-side refusal happens after the bytes are already in this process.
    // Kept because an older or a malicious borrower is exactly the case a
    // backstop is for.
    if !serve_is_allowed_for_path(&request.path) {
        bail!(
            "peer serve: {} refused for path {}, a relayed request may never carry a \
             client-credential or local-control path",
            session.peer.display(),
            request.path
        );
    }
    // The method is HONOURED, which for this build means exactly one method is
    // servable and every other one is refused here rather than rewritten into a
    // POST further down. A borrower whose client sent `GET` and whose request
    // was silently POSTed on someone else's account got an answer to a question
    // it never asked, paid for out of a lease.
    if !request.method.eq_ignore_ascii_case(SERVE_METHOD) {
        bail!(
            "peer serve: {} sent method {} and a relayed request is {SERVE_METHOD} or nothing; \
             refused rather than rewritten",
            session.peer.display(),
            request.method
        );
    }

    // The two ids must be ONE id. See this function's doc.
    if request.request_id != header_request_id {
        bail!(
            "peer serve: {} opened the stream under request id {:032x} and sent a frame for \
             {:032x}; a relayed request is one id and this one is refused rather than \
             reconciled",
            session.peer.display(),
            header_request_id,
            request.request_id
        );
    }

    // THE LEASE'S SCOPE, read first: it is what decides which of this Mac's
    // accounts may serve the request AND which accounts' headroom the owner
    // guard is measured against, so both questions below need it in hand.
    let (window, scope) = {
        let held = ledger.lock().map_err(|_| anyhow!("ledger lock poisoned"))?;
        (
            held.window_of(request.lease_id),
            held.scope_of(request.lease_id),
        )
    };
    // THE LENDING HOURS, RE-READ PER RELAY and not only at the mint.
    //
    // The `--between` / `--days` schedule was consulted once, by `Ledger::grant`,
    // and never again. A lease minted a minute before the window closed then
    // kept serving on the owner's account until its own TTL or its `until`
    // said otherwise, which for a `--for 7d` grant is days outside the hours
    // the operator lent. An operator who writes "22:00-08:00" means the
    // requests, not the paperwork.
    //
    // Here rather than in `Ledger::may_relay`: the schedule lives on the
    // operator's own `LendGrant` in the peers file and the ledger holds no copy
    // of it, so this is the first point on the serving path that can read the
    // grant at all. Read fresh every relay, which is also what makes an
    // operator's edit to the hours take effect on the next request rather than
    // on the next mint.
    //
    // Before `enter_relay`, so a refusal takes no in-flight slot, and answered
    // with the ack frame: nothing has been sent upstream, so the borrower still
    // holds its body and may ask the next lender.
    if let Some(window) = window {
        let outside = store
            .row(&session.peer)
            .and_then(|row| {
                row.grant_for(
                    window,
                    u64::try_from(now_ms / 1_000).unwrap_or(0),
                    &|scope| {
                        utilization.scope_restriction(scope) != ScopeRestriction::Unenforceable
                    },
                )
                .and_then(crate::peer::config::LendGrant::schedule)
            })
            .and_then(|schedule| {
                crate::peer::lease::schedule_refusal(
                    Some(&schedule),
                    time::OffsetDateTime::now_utc(),
                )
            });
        if let Some(refusal) = outside {
            tracing::info!(
                peer = %session.peer.display(),
                window = ?window,
                "peer serve: this relay arrived outside the hours its grant was lent for, so \
                 it is refused rather than served"
            );
            send_json(stream, session, &ServeAck::Refused { refusal }).await?;
            return Ok(());
        }
    }

    // The owner's guard on THIS SCOPE's accounts, measured now, before
    // `enter_relay` reads it. A measurement found nothing in production wrote a
    // per-scope figure, so `may_relay` decided a group-scoped relay against the
    // whole fleet's room. Read outside the ledger lock, the reader reaches the
    // manager's accounts, and holding both is how a later edit deadlocks the
    // two.
    let scoped_headroom = window.and_then(|window| utilization.lendable(&scope, window));
    let entered = {
        let mut held = ledger.lock().map_err(|_| anyhow!("ledger lock poisoned"))?;
        // Every scope, `All` included. See `Ledger::grant` for why a reader
        // holding the fleet at the moment of the decision beats a note up to
        // thirty seconds old, and why `None` leaves the ticker's figure alone.
        if let (Some(window), Some(measured)) = (window, scoped_headroom) {
            held.note_scope_headroom(&scope, window, measured);
        }
        held.enter_relay(request.lease_id, &session.peer, request.request_id, now_ms)
    };
    if let Err(refusal) = entered {
        // A [`ServeAck`] and no longer a [`ServeReply`]: this is decided before
        // the body has been asked for, so the borrower still holds it and the
        // next lender may be asked. Same word on the wire, one frame earlier.
        let ack = ServeAck::Refused {
            refusal: refusal.to_wire(),
        };
        send_json(stream, session, &ack).await?;
        return Ok(());
    }
    // THE SLOT, IN A GUARD FROM HERE ON. `enter_relay` took one and every path
    // out of this function owes it back; they were three hand-placed
    // `leave_relay` calls, and the `?` on the ack write below sat between two
    // of them. A write failure there, or a poisoned ledger lock on either of
    // the later releases, stranded the lease's in-flight count for its whole
    // TTL: the borrower could not use the lease again and nothing on this Mac
    // would ever put the slot back.
    let mut slot = RelaySlot {
        ledger,
        lease_id: request.lease_id,
        returned: false,
    };

    // THE PICKER RESTRICTION, decided before anything is sent: a
    // lease scoped to a group or to named accounts may only ever be served by
    // one of them, so a scope this build cannot hold the request inside is a
    // refusal and never a full-pool serve. The slot taken by `enter_relay`
    // above is released on this path too, a refusal that leaked a slot would
    // strand the lease for its whole TTL.
    let restriction = utilization.scope_restriction(&scope);
    if restriction == ScopeRestriction::Unenforceable {
        tracing::warn!(
            peer = %session.peer.display(),
            scope = %scope,
            "peer serve: this lease's scope cannot be enforced by this build's picker, so the \
             request is refused rather than served on an account outside it"
        );
        send_json(
            stream,
            session,
            &ServeAck::Refused {
                refusal: LeaseRefusal::Unsupported,
            },
        )
        .await?;
        return Ok(());
    }

    // THE ACK: every gate this build has is past, and nothing has been sent
    // upstream. From here on a failure really is "this may have run", which is
    // exactly the fact the borrower needs and could not have before this frame
    // existed. It is written BEFORE the body is asked for, so a borrower whose
    // request was refused above still holds its body and its next lender.
    send_json(stream, session, &ServeAck::Accepted).await?;

    let body = match recv_body(stream, session).await {
        Ok(body) if body.len() == request.body_bytes => body,
        // A body that does not match the promise, or that stopped arriving.
        // The slot `enter_relay` took is released the way every other refusal
        // on this path releases it, and the borrower is told: nothing has been
        // sent upstream, so this is still "not this lender" rather than an
        // outcome it has to treat as unknown.
        outcome => {
            tracing::warn!(
                peer = %session.peer.display(),
                promised = request.body_bytes,
                sent = outcome.as_ref().map(Vec::len).unwrap_or_default(),
                "peer serve: the borrower's body is not the body its frame promised, so \
                 nothing was sent upstream"
            );
            send_json(
                stream,
                session,
                &ServeReply::Refused {
                    refusal: LeaseRefusal::Unsupported,
                },
            )
            .await?;
            return Ok(());
        }
    };

    // The owner's own utilization on this lease's window, before this node
    // makes the request on its own account. Read OUTSIDE the ledger lock: the
    // reader reaches the manager's accounts, and holding both locks across it
    // is how a later edit introduces a deadlock between the two.
    let before = window
        .map(|window| utilization.read(window))
        .unwrap_or_default();

    let started = Instant::now();
    let served = serve_on_own_account(upstream, &request, body, &restriction).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // The same windows, after. `utilization_rise` is what turns the pair into
    // one figure, and an index unmeasured on either side contributes nothing.
    // See its doc for why that is the honest direction to round in.
    let after = window
        .map(|window| utilization.read(window))
        .unwrap_or_default();
    let rise = utilization_rise(&before, &after);

    // The slot is released on every path out of the serve, including the error
    // one: a slot leaked by a failed relay strands the lease for its whole TTL.
    // The debit shares this one lock with it, so nothing between them can move
    // the lease this line is about to describe.
    let debited = {
        let mut held = ledger.lock().map_err(|_| anyhow!("ledger lock poisoned"))?;
        // Under the SAME lock as the debit below, which is why this one release
        // is still written by hand: nothing between them may move the lease the
        // debit is about to describe. The guard is told so it does not give the
        // slot back a second time.
        held.leave_relay(request.lease_id);
        slot.returned = true;
        // Before the debit, so this request's tokens land on the path it
        // actually arrived over: `debit` is what charges the meter, and a note
        // written after it would credit the hour's first borrow to nowhere and
        // every later one to the path of the borrow before it. The newest note
        // wins, which is what lets a borrower that moved house keep its lease
        // and change its path. Unattributable stays unattributed: `locator_from`
        // answers `None` rather than naming a locator this connection did not
        // come over, and there is no clearing call, so a borrow this build
        // could not place leaves the previous one's attribution alone.
        if let Some(path) = store
            .row(&session.peer)
            .and_then(|row| row.locator_from(from))
        {
            held.note_lease_path(request.lease_id, path);
        }
        // `is_ok` is now exactly "the upstream answered", which is what the
        // charge has to follow. It used to be narrower without saying so:
        // `serve_on_own_account` returned `Err` for a reply larger than one
        // frame, AFTER the lender's own account had paid for it, so the lease
        // was charged 0.0 and the borrower re-asked elsewhere. Every failure
        // after the answer is an answer now (see `undeliverable_answer`), so
        // the only `Err` left is one where nothing was served and nothing is
        // owed.
        if served.is_ok() {
            held.debit(request.lease_id, request.request_id, rise)
        } else {
            0.0
        }
    };

    let reply = served?;

    // THE LENDER'S LOG LINE, and it carries exactly four fields: who, which
    // window, how many bytes, how many milliseconds.
    //
    // No path, no model, no body, no header, the prompts are in this process
    // because that is what borrowing an account means, and a log file is a copy
    // of them that outlives the request. `SERVED_LINE` is the message a gate
    // greps for, named here so the gate cannot drift from it
    // (`tests/peer_boot.rs`).
    //
    // The lease ACCOUNTING (the rise this node measured and what it charged)
    // is one line below at `debug!`, not folded in here. Both are honest
    // numbers and neither is a prompt, but the four fields above are the line
    // an operator reads to answer "who used my account and for how long", and a
    // line whose field set grows is a line whose next field nobody argues
    // about. The two figures are debug because they answer a different
    // question: whether the debit is measuring anything at all.
    tracing::info!(
        peer = %session.peer.display(),
        window = ?window,
        bytes = reply.body.len(),
        ms = elapsed_ms,
        "{SERVED_LINE}"
    );
    tracing::debug!(
        peer = %session.peer.display(),
        observed_rise = rise,
        debited,
        "peer serve: what that relayed request cost this Mac's own window"
    );

    send_json(
        stream,
        session,
        &ServeReply::Served {
            status: reply.status,
            headers: reply.headers,
            body_bytes: reply.body.len(),
        },
    )
    .await?;
    send_body(stream, session, &reply.body).await?;
    Ok(())
}

/// The in-flight slot [`Ledger::enter_relay`] took, given back when it goes out
/// of scope.
///
/// Every `Ok` from `enter_relay` owes exactly one `leave_relay`, and the pairing
/// used to be three hand-placed calls on the three paths anybody had thought of.
/// The `?` on the ack write sat between two of them, so a borrower that hung up
/// while the ack was being written left the slot held, and a poisoned ledger
/// lock on either of the later releases did the same. A lease whose in-flight
/// count never comes down is refused `InFlightFull` for the rest of its TTL and
/// nothing on this Mac puts it back.
///
/// A guard cannot be forgotten by a path added later, which is the whole reason
/// it is one: the failure this replaces was not a wrong release, it was a
/// release nobody wrote.
struct RelaySlot<'a> {
    ledger: &'a std::sync::Mutex<Ledger>,
    lease_id: u128,
    /// Set by the one caller that gives the slot back itself, under a lock it
    /// is already holding for the debit. Without it the guard would release a
    /// second slot that was never taken, and `leave_relay` saturates at zero,
    /// so the double release would be silent.
    returned: bool,
}

impl Drop for RelaySlot<'_> {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        // A POISONED LOCK STILL GIVES THE SLOT BACK. `into_inner` on the
        // poisoned guard is the honest call here: the alternative is the leak
        // this type exists to stop, and a panic in a `Drop` during an unwind
        // aborts the process. The ledger's other readers keep seeing the
        // poison, so nothing is being hidden.
        let mut held = match self.ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => {
                tracing::error!(
                    "peer serve: the ledger lock is poisoned, so this relay's in-flight slot                      is being given back through it rather than left held for the lease's                      whole life"
                );
                poisoned.into_inner()
            }
        };
        held.leave_relay(self.lease_id);
    }
}

/// What the lender's own proxy answered.
struct ServedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Send the relayed request through this node's OWN proxy. See
/// [`handle_serve_on`] for why that is the whole of the picker, the Bearer and
/// the bucket.
///
/// **Only [`LENDER_FORWARDED_HEADERS`] are copied onto that request.** This
/// replaced a loop that replayed every borrower-supplied header, which put a
/// borrower-chosen `cookie` (and any header a borrower invented) on the
/// lender's own outbound TLS session, `build_upstream_headers` removes
/// `authorization`, `x-api-key`, `accept-encoding` and the hop-by-hop names,
/// and `cookie` is none of those.
/// `restriction` is the scope, as the account-selection header the
/// lender's own picker reads for it.
///
/// # "Written last" was the wrong mechanism, and it was inverted
///
/// This doc used to say a borrower's own `x-tcr-group` could not widen the scope
/// because the lender writes its headers after the allowlist loop. A
/// measurement found the opposite: `RequestBuilder::header` **appends**, and
/// `src/proxy.rs` reads the scope with `headers.get(GROUP_HEADER_NAME)`, which
/// answers the FIRST value under a name. Writing last is therefore writing
/// second, and second loses.
///
/// The property is real and it rests on one thing only:
/// [`lender_forwards_header`] carries neither [`crate::proxy::GROUP_HEADER_NAME`]
/// nor [`crate::proxy::ACCOUNTS_HEADER_NAME`], so a borrower's copy is dropped
/// before the loop can append it. The loop below names the two REFUSALS
/// explicitly anyway, a second reader of the same fact, so a future widening of
/// the allowlist cannot hand a borrower the picker.
async fn serve_on_own_account(
    upstream: &str,
    request: &ServeRequest,
    body: Vec<u8>,
    restriction: &ScopeRestriction,
) -> Result<ServedResponse> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("peer serve: could not build the loopback client")?;
    // THE REVIEW'S H1, STRUCTURALLY. `handle_serve_on` has already refused
    // every path `relay_path_is_routable` rejects, so this is the second half of
    // that defence and the half that holds for a caller written later: the URL
    // is built by SETTING THE PATH on a parsed base, so the host, the port and
    // the scheme come from `upstream` and nothing the borrower wrote can reach
    // them. `format!("{upstream}{path}")` could: a path that does not begin with
    // `/` turned the authority into userinfo and sent the lender's own request
    // to a host the borrower named.
    let mut url = reqwest::Url::parse(upstream)
        .context("peer serve: this Mac's own proxy base is not a URL")?;
    url.set_path(&request.path);
    // The client's own query, put back on AFTER the path gates have had the
    // path alone. Dropping it sent the lender's own account a different
    // question from the one the borrower's client asked, and answered that one
    // instead; the direct path and the carry path both keep it.
    url.set_query(request.query.as_deref());
    // And then the reading is checked against the base, but NOT because it is
    // the thing that stops a borrower-named host. This comment used to claim
    // that: "delete the `relay_path_is_routable` refusal above and feed this
    // `//169.254.169.254/latest/meta-data` and this line is what stops it".
    // Measured on this repository's pinned `url`, with `upstream =
    // "http://127.0.0.1:3456"`, the readings are asserted in
    // `set_path_keeps_the_authority_and_the_under_base_check_is_not_what_refuses`
    // (`tests/peer_lease.rs`):
    //
    //   //169.254.169.254/latest/meta-data -> .../169.254.169.254/latest/meta-data
    //   /x/../_tcr/accounts/control        -> ..../_tcr/accounts/control
    //   @169.254.169.254/latest/meta-data  -> .../@169.254.169.254/latest/…
    //
    // All three stay under the base, so this line refuses NONE of them. The
    // authority is safe because `set_path` cannot move it, and the dot-segment
    // collapse onto this Mac's own privileged `/_tcr/` route is refused by
    // `relay_path_is_routable` and by nothing else here.
    //
    // What this check does catch is an `upstream` that is not a bare origin: a
    // base carrying a path (`http://127.0.0.1:3456/api`) has that path REPLACED
    // by `set_path`, and the result is a URL outside the base this Mac was
    // configured with. That is a misconfiguration rather than an attack, and it
    // is worth a refusal rather than a silent reroute.
    let base = upstream.trim_end_matches('/');
    if !url.as_str().starts_with(base) {
        bail!(
            "peer serve: path {} builds the URL {} , which is not under this Mac's own proxy \
             at {base}; refused rather than sent",
            request.path,
            url
        );
    }
    // ONE HOP, AND THIS IS HOW THE NEXT PROXY KNOWS. The request below goes
    // into this Mac's own proxy, which is a full proxy: if this fleet is dry it
    // walks to the dry-fleet terminal and consults a fallback provider of its
    // own, so a borrowed request could be borrowed onward, to a third Mac or
    // back to the Mac that sent it. Each hop looks locally reasonable and the
    // cycle is only visible from outside. See
    // [`crate::proxy::RELAYED_HEADER_NAME`] for why presence is the whole
    // signal and why a borrower that forges it can only refuse itself.
    let mut send = client
        .post(url)
        .header(crate::proxy::RELAYED_HEADER_NAME, "1")
        .body(body);
    for (name, value) in request
        .headers
        .iter()
        .filter(|(name, _)| lender_forwards_header(name))
        // The two account-selection names, refused by name as well as by
        // absence from the allowlist. See this function's doc: `header` appends
        // and the proxy reads the first value, so a borrower's copy arriving
        // FIRST would decide the scope.
        .filter(|(name, _)| !name.eq_ignore_ascii_case(crate::proxy::GROUP_HEADER_NAME))
        .filter(|(name, _)| !name.eq_ignore_ascii_case(crate::proxy::ACCOUNTS_HEADER_NAME))
    {
        send = send.header(name, value);
    }
    match restriction {
        ScopeRestriction::Group(group) => {
            send = send.header(crate::proxy::GROUP_HEADER_NAME, group);
        }
        ScopeRestriction::Accounts(labels) => {
            // Comma-separated, and that separator is safe by construction
            // rather than by hope: `tcr_peer_wire::sanitize_label` is a
            // character whitelist (alphanumeric, `-`, `_`, `.`, space), so no
            // label a lender can hold carries a comma for this join to be
            // ambiguous about.
            send = send.header(crate::proxy::ACCOUNTS_HEADER_NAME, labels.join(","));
        }
        // `Unenforceable` never reaches here: `handle_serve_on` refuses it
        // before a byte is sent. Matched rather than `_` so a fifth arm added
        // later cannot silently fall through to an unrestricted serve.
        ScopeRestriction::Unrestricted | ScopeRestriction::Unenforceable => {}
    }
    let response = send
        .send()
        .await
        .context("peer serve: this Mac's own proxy did not answer the relayed request")?;

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    // ===================================================================
    // EVERYTHING BELOW THIS LINE IS AFTER THE UPSTREAM ANSWERED, and that is
    // why none of it is an `Err` any more.
    //
    // An `Err` from here reached `handle_serve_on`'s `if served.is_ok()` and
    // debited 0.0: the lender's account had already paid for the request, and
    // the lease was charged nothing. Worse, the stream then died, so the
    // borrower saw no answer at all, timed out and asked another lender, which
    // ran the same request again. A reply over the frame size is not a rare
    // shape either: 65 519 bytes is any long completion.
    //
    // So both post-answer failures become an ANSWER: a small 502 the borrower
    // can read, carrying the reason. The borrower is told its request was
    // served and cannot be delivered, which is the truth, rather than being
    // left to re-ask.
    // ===================================================================
    let body = match response.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                status,
                "peer serve: this Mac's own proxy answered and the body could not be \
                 read; the lease is charged what was measured and the borrower is told"
            );
            return Ok(undeliverable_answer(
                "the answer could not be read from this Mac's own proxy",
            ));
        }
    };
    // AN ANSWER OVER ONE FRAME IS CARRIED, NOT REFUSED. This used to return a
    // 502 for any body over `MAX_BODY_BYTES`, which is 65 519 bytes and
    // therefore any long completion: the lender's account bought the answer,
    // the lease was debited for it, and the borrower got an error with no
    // content. The reply is chunked over as many frames as it takes now
    // ([`send_body`]), and the only bound left is the one on a whole body.
    if body.len() > MAX_RELAYED_BODY_BYTES {
        tracing::warn!(
            bytes = body.len(),
            cap = MAX_RELAYED_BODY_BYTES,
            "peer serve: the answer is larger than a relayed body may be; the lease is \
             charged what was measured and the borrower is told rather than left waiting"
        );
        return Ok(undeliverable_answer(&format!(
            "the answer is {} bytes and a relayed answer carries {MAX_RELAYED_BODY_BYTES}",
            body.len()
        )));
    }
    Ok(ServedResponse {
        status,
        headers,
        body,
    })
}

/// The answer a lender sends when its own account served the request and the
/// result cannot be carried back.
///
/// **502, and never an `Err`.** The request ran, so the account paid for it and
/// the lease is charged what was measured; an `Err` here charged nothing and
/// killed the stream, which sent the borrower to the next lender with the same
/// body. A borrower that reads this knows its request was served and that this
/// answer is all it gets, which is the one thing that stops it being run twice.
///
/// # `x-should-retry: false` is the half that reaches the client
///
/// The sentence above was true of the borrower and false of the SDK sitting
/// behind it. A 502 with no header on it is retryable by default
/// (`src/proxy.rs` sets this header on every answer it means as final, and
/// `lease.rs`'s and `egress.rs`'s own "may have been billed" answers both carry
/// it), so the one request this build is certain already ran and was debited
/// was also the one the client was free to send again. The header travels the
/// same way the rest of this answer does: the borrower rebuilds the response
/// from these pairs, and this name is not hop-by-hop, so it survives.
fn undeliverable_answer(why: &str) -> ServedResponse {
    let payload = serde_json::json!({
        "type": "error",
        "error": {
            "type": "proxy_error",
            "message": format!(
                "This request was served on a peer's account and its answer could not be \
                 carried back: {why}. It was NOT served again: retrying may pay for it twice."
            ),
        },
    });
    ServedResponse {
        status: 502,
        headers: vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-should-retry".to_string(), "false".to_string()),
        ],
        body: payload.to_string().into_bytes(),
    }
}

/// The peers file's own directory, which is also the node-key directory, the
/// same resolution `tcr peer id` uses, so one `--peers` argument points the
/// whole peer surface at a temp dir.
pub(crate) fn node_key_dir(store: &PeerStore) -> PathBuf {
    store.path().parent().map_or_else(
        crate::peer::id::default_config_dir,
        std::path::Path::to_path_buf,
    )
}

/// The runtime-state file that belongs with `peers_path`: the pairing window,
/// the knock queue, the mutes and the bans.
///
/// The DEFAULT peers file lives in the config directory and its state file in
/// the cache directory, a split [`crate::peer::state::default_path`] already
/// owns, so the default is never derived here. Only an explicit `--peers` (or a
/// `--config` that resolves a peers file beside it) moves the state file with
/// it, which is what makes one argument select a whole profile and what stops a
/// test writing the operator's real `peer-state.json`.
///
/// # There is a second copy of this, and it is not deleted
///
/// `src/main.rs`'s private `peer_state_path` is the same six lines, in the part
/// of that file the `ls`, `pair` and `pending` verbs read rather than
/// `lend`/`share`/`allow`. Nothing makes it call this one yet, so the two
/// agree line for line,
/// and this doc is where a reader learns there are two.
pub fn peer_state_path(peers_path: &std::path::Path) -> PathBuf {
    if peers_path == crate::peer::config::default_path() {
        return crate::peer::state::default_path();
    }
    match peers_path.parent() {
        Some(parent) => parent.join("peer-state.json"),
        None => crate::peer::state::default_path(),
    }
}

/// Say hello to one pinned peer: tell it where this node listens, and record
/// where it says it listens.
///
/// # Why this exists, and why nothing else could do its job
///
/// `Hello.addrs` had a producer and no sender. The listener ANSWERS a `Hello`
/// with its own ([`crate::peer::listener::hello_for_peer`]), so the address a
/// node announces only ever travels on the answering side, and the machine
/// that needs to announce a new address is the one that MOVED, which is the
/// side that dials. This is that frame.
///
/// Both halves of the refresh happen on one round trip. Outbound, this node's
/// listen socket reaches the peer, which records it against this node's row.
/// Inbound, the peer's own listen socket reaches this node, together with the
/// fact that the dial to this endpoint worked at all, so the endpoint that
/// answered is re-dated and leads the list next time.
///
/// Returns the peer's `Hello`, or `None` when the peer is not pinned or
/// nothing answered on any of its endpoints. A dial that fails is not an
/// error: a laptop asleep is the normal state of this fleet, and the endpoints
/// already on the row stay exactly as they were.
pub async fn say_hello(store: &PeerStore, peer: &PeerId) -> Result<Option<Hello>> {
    store.reload_if_changed();
    let Some(row) = store.row(peer) else {
        return Ok(None);
    };
    // The socket the dial actually landed on comes back with the stream: it is
    // the one fact here this node observed rather than believed, and a boxed
    // stream cannot be asked for it afterwards.
    let Some((reached, mut stream)) = dial_peer_with_endpoint(&row).await else {
        return Ok(None);
    };

    let key = crate::peer::id::NodeKey::load_or_mint(&node_key_dir(store))
        .context("peer hello: this node has no keypair to greet a peer with")?;
    let mut session = noise::dial_handshake(
        &mut stream,
        key.secret_bytes(),
        Handshake::Return,
        Some(&peer.0),
        None,
    )
    .await
    .context("peer hello: the handshake with the peer failed")?;

    // The dialling half of the same record the listener writes on its own side:
    // both ends derive this from the handshake they both ran, and each keeps
    // its own copy so either can still meet the other after a restart.
    if let Err(err) = crate::peer::config::observe_rendezvous_secret(
        store.path(),
        peer,
        crate::peer::reach::port_secret(&session.handshake_hash),
    ) {
        tracing::warn!(
            peer = %peer.display(),
            error = %err,
            "peer hello: could not record this pair's rendezvous secret",
        );
    }

    let header = StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: crate::peer::lease::random_id()?,
    };
    send_control(&mut stream, &mut session, &header).await?;

    let file = store.file();
    let mut facts = crate::peer::listener::NodeFacts::listening(key.id(), file.listen);
    facts.briefs = crate::peer::discovery::neighbor_briefs(&file.peers, *peer);
    // Deliberately NOT set on this side. `observed_you_at` means "the source
    // address your packets arrived from", which only the accepting end
    // observes; what a dialler holds is the address it CHOSE to dial, and
    // sending that would give the field two meanings and hand a peer its own
    // LAN address as a public one.
    let mine = crate::peer::listener::hello_for_peer(&facts, &row.allow.control);
    send_control(&mut stream, &mut session, &Control::Hello(mine)).await?;

    let answer: Control = recv_control(&mut stream, &mut session).await?;
    let Control::Hello(theirs) = answer else {
        bail!("peer hello: the peer answered a Hello with {answer:?}");
    };

    // What the far side sees this Mac at. The dialling half of the pair of
    // records the listener writes on its own side, and the only thing in this
    // function that could not be derived here: a Mac behind a NAT cannot see
    // its own public address, and the peer that just answered can.
    if let Some(told) = theirs.observed_you_at.as_deref() {
        match told.parse::<SocketAddr>() {
            Ok(addr) => {
                crate::peer::reach::remember_observed_self(*peer, addr);
                // Onto the row too: see the listener's own copy of this arm.
                if let Err(err) = crate::peer::config::observe_seen_address(
                    store.path(),
                    peer,
                    addr,
                    u64::try_from(crate::now_ms().max(0)).unwrap_or_default(),
                ) {
                    tracing::warn!(
                        peer = %peer.display(),
                        error = %err,
                        "peer hello: could not record where this peer sees us",
                    );
                }
            }
            Err(err) => tracing::debug!(
                peer = %peer.display(),
                told = %told,
                error = %err,
                "peer hello: this peer said it sees us at something that is not a socket                  address; ignoring the hint"
            ),
        }
    }
    // And where this Mac saw IT: the socket the dial landed on is the peer's
    // own address seen from here, which is what a punch back at it would aim
    // for when the recorded endpoints stop answering.
    crate::peer::reach::remember_observed_peer(*peer, reached);

    if let Some(briefs) = theirs.briefs.as_deref() {
        if let Err(err) = crate::peer::discovery::observe_neighbor_briefs(
            store.path(),
            peer,
            briefs,
            crate::now_ms(),
        ) {
            tracing::warn!(
                peer = %peer.display(),
                error = %err,
                "peer hello: could not record a neighbor brief from this peer",
            );
        }
    }

    let now = crate::now_ms();
    // The peer's own claimed addresses, plus the host this dial actually
    // reached on the port that hello announced: see
    // [`crate::peer::config::endpoints_and_connection_from_hello`] for why an
    // unconditional push of `reached` would leave a dead entry on the row next
    // to the address the peer already named.
    let learned =
        crate::peer::config::endpoints_and_connection_from_hello(&theirs.addrs, reached, now);
    // Every outcome is said out loud at debug, because two of the three are a
    // peer this node now holds no fresh way to reach, and a silent write path
    // makes that look identical to a successful one.
    match crate::peer::config::observe_endpoints(store.path(), peer, &learned)
        .context("peer hello: could not record where this peer answers")?
    {
        crate::peer::config::Observed::Written { added } => tracing::debug!(
            peer = %peer.display(),
            added,
            "peer hello: recorded where this peer answers",
        ),
        crate::peer::config::Observed::NothingDialable => tracing::debug!(
            peer = %peer.display(),
            "peer hello: nothing this peer named is an address anything can dial, so its row \
             keeps the endpoints it already had",
        ),
        crate::peer::config::Observed::NoRow => tracing::debug!(
            peer = %peer.display(),
            "peer hello: this peer is no longer pinned here, so nothing was recorded for it",
        ),
    }

    // The probe rides this session for free: it already paid the round trip a
    // fresh dial would cost, and the endpoint just learned above and the cost
    // of reaching it belong in the same write. The table is cloned out of its
    // lock rather than probed in place, because `probe::with_table` holds a
    // `std::sync::Mutex` and probing awaits the network between rounds; one
    // round, on the constants the module already declares.
    let locator = Locator::Direct { addr: reached };
    match crate::peer::probe::with_table(|table| table.clone()) {
        Ok(mut table) => {
            match crate::peer::probe::probe_session(
                &mut stream,
                &mut session,
                &mut table,
                locator,
                1,
                crate::peer::probe::PROBE_INTERVAL,
            )
            .await
            {
                Ok(run) => {
                    tracing::debug!(
                        peer = %peer.display(),
                        sent = run.sent,
                        stop = ?run.stop,
                        "peer hello: probed this session before it closed",
                    );
                    if let Err(err) =
                        crate::peer::probe::with_table(|installed| *installed = table.clone())
                    {
                        tracing::warn!(
                            peer = %peer.display(),
                            error = %err,
                            "peer hello: could not update the path table with this session's \
                             probe",
                        );
                    }
                    let state_path = peer_state_path(store.path());
                    if let Err(err) = crate::peer::state::save_paths(&state_path, &table.stats) {
                        tracing::warn!(
                            peer = %peer.display(),
                            error = %err,
                            "peer hello: could not persist the measured path to the state file",
                        );
                    }
                }
                Err(err) => tracing::debug!(
                    peer = %peer.display(),
                    error = %err,
                    "peer hello: probing this session failed, and the endpoints learned above \
                     still stand",
                ),
            }
        }
        Err(err) => tracing::warn!(
            peer = %peer.display(),
            error = %err,
            "peer hello: could not read the path table, so this session was not probed",
        ),
    }

    Ok(Some(theirs))
}

/// Everything a stream to a peer has to be, as ONE name.
///
/// `Box<dyn AsyncRead + AsyncWrite + Unpin + Send>` is what this wants to say
/// and Rust cannot spell it: a trait object takes one non-auto trait plus auto
/// traits, and `AsyncRead` and `AsyncWrite` are two. So the pair gets a name,
/// with a blanket impl so every existing stream type already satisfies it and
/// nothing has to be registered. Reported to the lead as the one place the
/// brief's literal signature is not expressible.
pub trait PeerTransport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> PeerTransport for T {}

/// One stream to a peer, whatever opened it.
///
/// **The transport seam.** Every handler below a dial already works on
/// `AsyncRead + AsyncWrite + Unpin` and none of them names a socket, so this
/// alias is the one place a second way of reaching a peer, a relay, a
/// forwarded carry, a different protocol, arrives without touching a single
/// handler. `Send` because a TUNNEL hands its stream to a pump that outlives
/// the frame that dialled it, and boxed because two transports are two types
/// and a caller choosing between them at runtime cannot be generic over both.
pub type PeerStream = Box<dyn PeerTransport>;

/// The order the endpoints on a row are tried in.
///
/// # Why the order is a function and not a `sort` inside the dial
///
/// It is the whole of this node's routing policy, and it is worth one thing a
/// test can call. Four keys decide it and they are stated once, each with its
/// reason, in [`crate::peer::probe::order_endpoints`]: the kind of path the
/// operator asked for, then the loss ceiling, then the measured round trip,
/// then the newest observation.
///
/// The two rules below are what REMAINS when nothing has been measured. A path
/// no probe has answered on carries no loss figure and no round trip, so every
/// such path ties on the two middle keys and falls through to these two, which
/// is how a fleet that has never been probed dials exactly as it did before
/// probing existed:
///
/// - **Newest first**, because an endpoint is a record of where a peer
///   answered and the most recent observation is the best guess about now. The
///   row already keeps this order ([`PeerRow::observe_endpoint`]); re-stating
///   it here means a caller reading this function knows the order without
///   having to go and check that the writer still sorts.
/// - **`Via` last**, because a forwarded hop spends a THIRD machine's bytes and
///   needs its consent. A direct socket that works is always the better answer,
///   so a hop is what is left when nothing direct answered, never a peer
///   chosen because its timestamp happened to be newer.
pub fn dial_order(row: &PeerRow) -> Vec<Endpoint> {
    crate::peer::probe::dial_order(row)
}

/// Dial the row's endpoints in [`dial_order`] and take the first that answers,
/// reporting WHICH endpoint that was.
///
/// The address is returned because the caller that re-dates an endpoint needs
/// it and the dial is the only thing that knows it: a boxed stream cannot be
/// asked for its peer address, and asking the row again would name the
/// endpoint this node tried first rather than the one that answered.
///
/// Endpoints are ROUTING ADVICE and nothing else: identity is re-proven by the
/// handshake against the pinned static key, so trying them in order costs
/// nothing but a connect timeout and buys a peer that moved.
pub async fn dial_peer_with_endpoint(row: &PeerRow) -> Option<(SocketAddr, PeerStream)> {
    dial_peer_within(row, borrow_timeout_default_ms()).await
}

/// [`BORROW_TIMEOUT`] in milliseconds, for the callers that have no peers file
/// in hand.
fn borrow_timeout_default_ms() -> u64 {
    u64::try_from(BORROW_TIMEOUT.as_millis()).unwrap_or(10_000)
}

/// How many connect attempts one endpoint's budget is divided between.
///
/// Three, because that is how many rendezvous ports a pair has in any one slot
/// ([`crate::peer::reach::accepted_ports`]): current, previous, next. The
/// recorded port is tried on the same per attempt bound, so an endpoint whose
/// host has gone away costs a bounded wait instead of the operating system's
/// own connect timeout, which on a route that blackholes is over a minute.
const DIAL_ATTEMPTS_PER_ENDPOINT: u32 = 3;

/// [`dial_peer_with_endpoint`] with the operator's borrow timeout handed in.
///
/// # What this adds over dialling the recorded endpoints
///
/// Nothing about the ORDER: that is [`dial_order`] and stays there, so a sort
/// key this function does not own can replace it without touching the loop
/// below. What is here is the second thing to try when an endpoint's recorded
/// PORT stops answering, which is the case the derived port
/// exists for: the peer is still at that address and its router mapping moved,
/// or it rebound. The pair's three rendezvous ports for the current slot are
/// tried against the same host, in [`crate::peer::reach::accepted_ports`]'s
/// order, so one slot of skew still meets.
///
/// # The bound
///
/// Each attempt gets `borrow_timeout_ms / DIAL_ATTEMPTS_PER_ENDPOINT`, so the
/// three rendezvous attempts against one host fit inside the timeout the
/// operator set for the borrow that is waiting on this dial. A borrower that
/// waited the full timeout on one dead port and never reached the port the
/// peer moved to would have spent the budget proving the endpoint it already
/// knew was wrong.
///
/// A process holding no port secret for this peer tries the recorded
/// endpoints and stops, exactly as this function did before: see
/// [`crate::peer::reach::rendezvous_ports`] for when that is.
pub async fn dial_peer_within(
    row: &PeerRow,
    borrow_timeout_ms: u64,
) -> Option<(SocketAddr, PeerStream)> {
    let per_attempt = per_attempt_bound(borrow_timeout_ms);
    let rendezvous = crate::peer::reach::rendezvous_ports(&row.node, now_unix_seconds());
    for endpoint in dial_order(row) {
        let Some(addr) = endpoint.direct_addr() else {
            // Logged rather than dropped silently, because "the row had a way
            // back and nothing used it" is exactly the failure the forward-dial
            // path exists to end. A caller here cannot follow it:
            // a hop needs the forwarder's own row and this node's keypair, and
            // this entry point is handed neither. [`dial_peer_reaching`] is.
            tracing::debug!(
                peer = %row.node.display(),
                "peer serve: this row's next endpoint is a forwarded hop and this dial was \
                 handed no peers file to read the forwarder's row from; skipping it"
            );
            continue;
        };
        if let Some(reached) = try_direct_endpoint(row, addr, &rendezvous, per_attempt).await {
            crate::peer::probe::clear_cooldown(row.node, endpoint.locator);
            return Some(reached);
        }
        crate::peer::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
    }
    None
}

/// Each attempt's share of the borrow timeout. See
/// [`DIAL_ATTEMPTS_PER_ENDPOINT`].
fn per_attempt_bound(borrow_timeout_ms: u64) -> Duration {
    Duration::from_millis((borrow_timeout_ms / u64::from(DIAL_ATTEMPTS_PER_ENDPOINT)).max(1))
}

/// One recorded socket, then this pair's rendezvous ports against the same
/// host, the whole of what "try this direct endpoint" means, in one place
/// because two dial entry points share it and a second copy of a retry loop is
/// a second answer to "how patient is a dial".
async fn try_direct_endpoint(
    row: &PeerRow,
    addr: SocketAddr,
    rendezvous: &[u16],
    per_attempt: Duration,
) -> Option<(SocketAddr, PeerStream)> {
    if let Some(stream) = connect_within(addr, per_attempt).await {
        return Some((addr, stream));
    }
    for port in rendezvous {
        let candidate = SocketAddr::new(addr.ip(), *port);
        if candidate == addr {
            // Already tried, as the recorded port.
            continue;
        }
        if let Some(stream) = connect_within(candidate, per_attempt).await {
            tracing::debug!(
                peer = %row.node.display(),
                addr = %candidate,
                "peer serve: the recorded port stopped answering and this pair's \
                 rendezvous port did"
            );
            return Some((candidate, stream));
        }
    }
    None
}

/// How a dial got through: a socket this node opened, or a pinned Mac that
/// carried it.
///
/// One value rather than an `Option<SocketAddr>` plus a flag, because every
/// caller that re-dates an endpoint has to write a
/// [`crate::peer::config::Locator`] and the two variants ARE that enum's two
/// variants. A caller that only wants the stream ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reached {
    /// The socket that answered.
    Direct(SocketAddr),
    /// The pinned Mac that carried the stream to the peer.
    Via(PeerId),
    /// A hole punched through both routers, at the socket it landed on.
    ///
    /// Its own variant and not a [`Self::Direct`] because the address is a
    /// TRANSIENT mapping: the port is this slot's derivation and the router
    /// holding it stops within minutes, so writing it onto the row as an
    /// endpoint would put a dead socket at the front of the next dial's order.
    /// A caller that re-dates endpoints skips this one; the pair meets again
    /// by deriving the next slot's port, not by remembering this one.
    Punched(SocketAddr),
}

/// [`dial_peer_within`], with the peers file that makes a forwarded hop
/// possible, and so two more ways home:
///
/// 1. A [`crate::peer::config::Locator::Via`] endpoint on the row is FOLLOWED
///    rather than logged and skipped. Its position in the list is
///    [`dial_order`]'s and not this function's, so an operator who set
///    `paths.prefer: via` gets the hop tried first and one who did not gets it
///    tried last. That is the whole of the policy and it is stated once, there.
/// 2. When nothing on the row answered at all, every Mac this node holds
///    [`crate::peer::config::Allow::carry`] on is asked to carry, in
///    [`forwarders_for`]'s order. This is the case the row cannot describe: a
///    peer that moved and whose every recorded address is stale has no `Via`
///    endpoint to follow, because nothing in this build writes one, and a
///    mutual friend that IS reachable is the only way home left.
///
/// Reports WHICH path answered for the same reason [`dial_peer_with_endpoint`]
/// reports which socket did: a boxed stream cannot be asked afterwards, and a
/// caller that re-dates the row needs to write the locator that worked.
pub async fn dial_peer_reaching_within(
    row: &PeerRow,
    store: &PeerStore,
    borrow_timeout_ms: u64,
) -> Option<(Reached, PeerStream)> {
    let per_attempt = per_attempt_bound(borrow_timeout_ms);
    let rendezvous = crate::peer::reach::rendezvous_ports(&row.node, now_unix_seconds());

    for endpoint in dial_order(row) {
        match endpoint.locator {
            Locator::Direct { addr } => {
                if let Some((reached, stream)) =
                    try_direct_endpoint(row, addr, &rendezvous, per_attempt).await
                {
                    crate::peer::probe::clear_cooldown(row.node, endpoint.locator);
                    return Some((Reached::Direct(reached), stream));
                }
                crate::peer::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
            }
            // Both go out through another Mac and by the same call: a
            // `Reverse` endpoint names the friend holding a carrier for this
            // target, and asking that friend to forward is exactly what spends
            // the carrier (`tunnel::reach_target` takes it off the desk before
            // it tries to dial). The difference between the two is on the
            // FRIEND's side, not here.
            Locator::Via { node } | Locator::Reverse { node } => {
                if let Some(stream) =
                    forward_through(&node, &row.node, store, borrow_timeout_ms).await
                {
                    crate::peer::probe::clear_cooldown(row.node, endpoint.locator);
                    return Some((Reached::Via(node), stream));
                }
                crate::peer::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
            }
        }
    }

    // Step 2 of the dial order: a hole punched straight through both routers.
    // Before the carry below and after the row's own endpoints, because a
    // direct socket that works is the better answer and a carry spends a THIRD
    // machine's bytes. It is bounded by the same borrow timeout everything
    // else here is: a punch waits for a slot boundary, so a dial with ten
    // seconds of patience takes the slots that fit inside it and no others.
    match punch_dial(row, store, borrow_timeout_ms).await {
        Ok((addr, stream)) => return Some((Reached::Punched(addr), stream)),
        Err(failure) => tracing::debug!(
            peer = %row.node.display(),
            failure = %failure,
            "peer serve: nothing on this peer's row answered and a punch did not get              through either; a carried stream is what is left"
        ),
    }

    for forwarder in forwarders_for(&row.node, store) {
        if let Some(stream) = forward_through(&forwarder, &row.node, store, borrow_timeout_ms).await
        {
            tracing::info!(
                peer = %row.node.display(),
                via = %forwarder.display(),
                "peer serve: nothing on this peer's own row answered, and a Mac this node \
                 may ask to carry reached it"
            );
            return Some((Reached::Via(forwarder), stream));
        }
    }
    None
}

/// [`dial_peer_reaching_within`] for the callers that only want the stream, on
/// the operator's own borrow timeout.
pub async fn dial_peer_reaching(row: &PeerRow, store: &PeerStore) -> Option<PeerStream> {
    dial_peer_reaching_within(row, store, borrow_timeout_default_ms())
        .await
        .map(|(_, stream)| stream)
}

/// One attempt to reach `target` through the pinned Mac `forwarder`.
///
/// A failure is a debug line and a `None`: the caller has more candidates to
/// try and a forwarder that refuses is the ordinary answer, not an error. The
/// refusal TEXT is kept, because every one of `authorize_forward`'s refusals
/// names which grant was missing, and an operator debugging "why will this Mac
/// not carry for me" has nowhere else to read it.
async fn forward_through(
    forwarder: &PeerId,
    target: &PeerId,
    store: &PeerStore,
    borrow_timeout_ms: u64,
) -> Option<PeerStream> {
    let row = store.row(forwarder)?;
    let key = match NodeKey::load_or_mint(&node_key_dir(store)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer serve: this node has no keypair to open a forwarded stream with"
            );
            return None;
        }
    };
    // This node's own id is stamped into `via` so the forwarder refuses a
    // route that comes back here, which is `authorize_forward`'s cycle check
    // reading the one fact only the requester can supply.
    match crate::peer::tunnel::open_forward_to(
        &row,
        key.secret_bytes(),
        target,
        &[key.id()],
        borrow_timeout_ms,
    )
    .await
    {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::debug!(
                peer = %target.display(),
                via = %forwarder.display(),
                error = %err,
                "peer serve: that Mac did not carry a stream to this peer; trying the next way"
            );
            None
        }
    }
}

/// Every Mac this node may ask to CARRY a stream to `target`, in the order to
/// ask them.
///
/// # The grant is `carry` and it is the only admission
///
/// [`crate::peer::config::Allow::carry`] is "we may ask this Mac to carry us",
/// the direction its own doc-comment separates from `gateway` ("this peer may
/// ask US"). One operator act must not hand out both directions, so this reads
/// the one that means what it needs, exactly as
/// [`crate::peer::egress::candidates_from`] does for the carry to an origin.
///
/// Three exclusions, each one a thing that cannot work rather than a policy:
/// the target itself (a Mac cannot carry a stream to itself, and
/// [`crate::peer::tunnel::open_forward_to`] refuses it again), a candidate with
/// no DIRECT endpoint (reaching a forwarder through a forwarder is a chain
/// nothing here builds), and nothing else.
///
/// # The order is PROBE's, when PROBE has measured anything
///
/// [`crate::peer::probe::carrier_key`] is the sort, and its third part is what
/// makes it safe: a carrier nobody has probed ties at `u32::MAX` and falls
/// through to most-recently-seen-first, which is the order this would have had
/// without a path table at all. `paths.viaAllow`, when the operator set one,
/// sorts a Mac that is not on it behind every Mac that is: behind, and still
/// tried, the same rule [`crate::peer::probe::order_endpoints`] applies to a
/// `Via` endpoint: a path that is the only path is the way home.
pub fn forwarders_for(target: &PeerId, store: &PeerStore) -> Vec<PeerId> {
    let file = store.file();
    let last_seen = last_seen_by_peer(store);
    let mut chosen: Vec<&PeerRow> = file
        .peers
        .iter()
        .filter(|row| row.allow.carry)
        .filter(|row| row.node != *target)
        .filter(|row| row.endpoints.iter().any(|end| end.direct_addr().is_some()))
        .collect();
    let order = |row: &PeerRow| {
        let seen = last_seen
            .iter()
            .find(|(peer, _)| *peer == row.node)
            .map(|(_, at)| *at);
        crate::peer::probe::with_table(|table| {
            (
                u8::from(
                    !table.config.via_allow.is_empty()
                        && !table.config.via_allow.contains(&row.node),
                ),
                crate::peer::probe::carrier_key(&row.node, seen, table),
            )
        })
        .unwrap_or((
            0,
            (0, u32::MAX, std::cmp::Reverse(seen.unwrap_or(i64::MIN))),
        ))
    };
    chosen.sort_by_key(|row| order(row));
    chosen.into_iter().map(|row| row.node).collect()
}

/// Whether this row has ANY way back: its own endpoints, or a Mac this node may
/// ask to carry to it.
///
/// The question [`crate::peer::lease::PeerLeaseProvider::try_serve`] asks
/// before it spends anything on a lender, spelled here beside the dial that
/// answers it for real: a filter that said "no endpoints, skip" would skip
/// exactly the peer a forwarder exists for.
pub fn has_a_way_back(row: &PeerRow, store: &PeerStore) -> bool {
    row.has_endpoint() || !forwarders_for(&row.node, store).is_empty()
}

/// When each pinned Mac was last heard from, out of the runtime state file
/// beside the peers file.
///
/// A missing or unreadable state file is an empty list and not a refusal: it
/// costs the candidates their ordering and nothing else.
fn last_seen_by_peer(store: &PeerStore) -> Vec<(PeerId, i64)> {
    let state_path = peer_state_path(store.path());
    match crate::peer::state::load(&state_path, crate::peer::pair::now_ms()) {
        Ok(state) => state.last_seen,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "peer serve: no runtime state to order the carriers by; asking them in file \
                 order"
            );
            Vec::new()
        }
    }
}

/// One connect attempt, bounded.
///
/// A timeout and a refusal are the same outcome to the caller, which is "try
/// the next one", and both are logged with the address so a reader can tell
/// which of the four attempts against one host it was.
async fn connect_within(addr: SocketAddr, within: Duration) -> Option<PeerStream> {
    match tokio::time::timeout(within, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => Some(Box::new(stream) as PeerStream),
        Ok(Err(err)) => {
            tracing::debug!(
                addr = %addr,
                error = %err,
                "peer serve: that address did not answer; trying the next one"
            );
            None
        }
        Err(_elapsed) => {
            tracing::debug!(
                addr = %addr,
                timeout_ms = within.as_millis(),
                "peer serve: that address did not answer in time; trying the next one"
            );
            None
        }
    }
}

/// Seconds since the unix epoch, for the port slot.
fn now_unix_seconds() -> u64 {
    u64::try_from(crate::now_ms().max(0)).unwrap_or_default() / 1_000
}

/// [`dial_peer_with_endpoint`] for a caller that only wants the stream.
pub async fn dial_peer(row: &PeerRow) -> Option<PeerStream> {
    dial_peer_with_endpoint(row).await.map(|(_, stream)| stream)
}

/// Write one frame of this stream: `serde_json`, then the Noise transport.
///
/// One framing for every stream kind and every frame on it, because a second
/// spelling of "write a frame" is where a length check goes missing. `pub` so a
/// test can stand in for the listener's dispatch arm, which is the one line of
/// this feature that lives outside this file.
pub async fn send_control<S, T>(
    stream: &mut S,
    session: &mut noise::PeerSession,
    value: &T,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    send_json(stream, session, value).await
}

/// One JSON frame, decrypted. See [`send_control`].
pub async fn recv_control<S, T>(stream: &mut S, session: &mut noise::PeerSession) -> Result<T>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    recv_json(stream, session).await
}

/// One JSON frame, encrypted.
async fn send_json<S, T>(stream: &mut S, session: &mut noise::PeerSession, value: &T) -> Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).context("peer serve: a frame did not serialize")?;
    noise::send_encrypted(stream, &mut session.transport, &bytes).await
}

/// One JSON frame, decrypted.
async fn recv_json<S, T>(stream: &mut S, session: &mut noise::PeerSession) -> Result<T>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let bytes = noise::recv_encrypted(stream, &mut session.transport).await?;
    serde_json::from_slice(&bytes).context("peer serve: a frame did not parse")
}

/// Rebuild an HTTP response from what the lender answered.
///
/// Hop-by-hop response headers are the lender's connection's business, not this
/// one's, so they are dropped here the way `src/proxy.rs` drops them on the way
/// back from upstream.
fn response_from(status: u16, headers: &[(String, String)], body: Vec<u8>) -> Result<Response> {
    let mut builder = Response::builder().status(
        StatusCode::from_u16(status)
            .with_context(|| format!("peer serve: {status} is not a status code"))?,
    );
    for (name, value) in headers {
        if crate::proxy::is_response_skip(name) {
            continue;
        }
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(body))
        .context("peer serve: could not build the response for the borrower's client")
}

/// 128 opaque bits for a request id, from the same source
/// [`crate::peer::lease::Ledger::grant`] mints a lease id with.
fn random_u128() -> Result<u128> {
    crate::peer::lease::random_id()
}

/// Step 2 of the dial order: punch a hole straight through both routers.
///
/// # The order of the three things this does
///
/// The ask goes out FIRST and the punch only runs if it was carried. A punch
/// is two sides in one slot; one side alone is a connect into a router that
/// was never told to hold a port open, and it costs the slot's full width to
/// learn that. So a peer that could not be told is a named refusal here
/// ([`crate::peer::reach::PunchFailure::NotAnnounced`]) rather than thirty
/// seconds of waiting.
///
/// The ask itself travels as a CONTROL frame over a carried stream: a Mac
/// nobody can dial is, by definition, a Mac that cannot be told anything
/// directly, and a mutual friend carries the frame BLIND, as a fresh
/// end-to-end Noise session nested inside a TUNNEL.
async fn punch_dial(
    row: &PeerRow,
    store: &PeerStore,
    borrow_timeout_ms: u64,
) -> Result<(SocketAddr, PeerStream), crate::peer::reach::PunchFailure> {
    use crate::peer::reach::{self, PunchFailure};

    let (peer_ip, secret) = reach::punch_target(&row.node)?;
    let Some(mine) = reach::self_address_for(&row.node) else {
        return Err(PunchFailure::SelfAddressUnknown);
    };

    let now = crate::now_ms();
    let budget = Duration::from_millis(borrow_timeout_ms);
    let plan = reach::punch_slots_within(
        &reach::punch_plan(&secret, now, reach::PUNCH_SLOTS),
        now,
        budget,
    );
    let Some(first) = plan.first() else {
        // Named, and immediate: this is the ordinary case for a short borrow
        // timeout, and spending the caller's whole budget waiting for a
        // boundary it cannot reach would be the worst of both.
        return Err(PunchFailure::NoSlotConnected {
            slots_tried: 0,
            ports: Vec::new(),
        });
    };

    if !announce_punch(row, store, first.slot, mine, borrow_timeout_ms).await {
        return Err(PunchFailure::NotAnnounced);
    }

    let punched = reach::punch(
        &reach::KernelPunchNet,
        &plan,
        peer_ip,
        reach::slot_window().min(budget),
    )
    .await?;
    Ok((SocketAddr::new(peer_ip, punched.port), punched.stream))
}

/// Tell `row`'s Mac to be in slot `slot`, through any Mac that will carry the
/// frame. Answers whether one did.
///
/// A boolean and not an error: every refusal on the way is a forwarder that
/// said no, which is the ordinary answer and is already logged with its own
/// reason by [`forward_through`]. What the caller needs is whether the peer
/// was told, because that is what decides whether the slot is worth waiting
/// for.
async fn announce_punch(
    row: &PeerRow,
    store: &PeerStore,
    slot: u64,
    public_addr: SocketAddr,
    borrow_timeout_ms: u64,
) -> bool {
    let key = match NodeKey::load_or_mint(&node_key_dir(store)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer punch: this node has no keypair to ask for a punch with"
            );
            return false;
        }
    };

    for forwarder in forwarders_for(&row.node, store) {
        let Some(mut stream) =
            forward_through(&forwarder, &row.node, store, borrow_timeout_ms).await
        else {
            continue;
        };
        // The handshake runs against the TARGET's pinned key over whatever
        // stream came back, which is what makes the forwarder blind to this
        // frame exactly as it is to a SERVE.
        let session = noise::dial_handshake(
            &mut stream,
            key.secret_bytes(),
            Handshake::Return,
            Some(&row.node.0),
            None,
        )
        .await;
        let mut session = match session {
            Ok(session) => session,
            Err(err) => {
                tracing::debug!(
                    peer = %row.node.display(),
                    via = %forwarder.display(),
                    error = %err,
                    "peer punch: the carried handshake failed; trying another carrier"
                );
                continue;
            }
        };
        let request_id = match random_u128() {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer punch: this node could not mint a request id"
                );
                return false;
            }
        };
        let header = StreamHeader {
            kind: StreamKind::Control,
            target: None,
            via: Vec::new(),
            hops_remaining: 1,
            request_id,
        };
        let asked = async {
            send_control(&mut stream, &mut session, &header).await?;
            send_control(
                &mut stream,
                &mut session,
                &Control::PunchAt {
                    slot,
                    public_addr: public_addr.to_string(),
                },
            )
            .await
        }
        .await;
        match asked {
            Ok(()) => {
                tracing::info!(
                    peer = %row.node.display(),
                    via = %forwarder.display(),
                    slot,
                    "peer punch: asked this peer to meet on the pair's derived port"
                );
                return true;
            }
            Err(err) => tracing::debug!(
                peer = %row.node.display(),
                via = %forwarder.display(),
                error = %err,
                "peer punch: the ask did not reach this peer through that carrier"
            ),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcr_peer_wire::{Lease, LeaseUnit};

    /// A live lease this Mac has granted, with room for one relay.
    fn lent(ledger: &mut Ledger, lease_id: u128, grantee: PeerId) {
        let now = crate::now_ms();
        ledger.record_scoped(
            Lease {
                lease_id,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.50),
                granted_at_ms: now,
                expires_at_ms: now + 300_000,
                spent: 0.0,
                max_inflight: 1,
                until: None,
            },
            grantee,
            tcr_peer_wire::LendScope::All,
        );
        ledger.note_owner_headroom(Window::SevenDay, 0.30);
    }

    /// **The in-flight slot comes back on every way out, including the two that
    /// used to leak it.**
    ///
    /// `enter_relay` takes a slot and it used to be given back by three
    /// hand-placed calls, with the `?` on the ack write sitting between two of
    /// them: a borrower that hung up while the ack was being written, or a
    /// poisoned ledger lock on either later release, left the slot held and the
    /// lease answered `InFlightFull` for the rest of its TTL.
    ///
    /// Both legs, because the poisoned one is the half a guard could easily get
    /// wrong: a `lock()` that returns `Err` and is quietly ignored leaks
    /// exactly what this replaces.
    ///
    /// Watched red: delete the `impl Drop for RelaySlot` body and the first
    /// assertion reads 1; make the poisoned arm return instead of calling
    /// `into_inner` and the second reads 1.
    #[test]
    fn a_relay_slot_is_given_back_when_its_guard_goes_out_of_scope() {
        let grantee = PeerId([11_u8; 32]);
        let lease_id = 0x5107_u128;
        let ledger = std::sync::Mutex::new(Ledger::new());
        {
            let mut held = ledger.lock().expect("ledger lock");
            lent(&mut held, lease_id, grantee);
            held.enter_relay(lease_id, &grantee, 1, crate::now_ms())
                .expect("the first relay is admitted");
            assert_eq!(held.inflight(lease_id), 1, "the slot is taken");
        }
        {
            let _slot = RelaySlot {
                ledger: &ledger,
                lease_id,
                returned: false,
            };
        }
        assert_eq!(
            ledger.lock().expect("ledger lock").inflight(lease_id),
            0,
            "a guard that goes out of scope gives the slot back, whatever path took it there"
        );

        // AND THROUGH A POISONED LOCK. The ledger is the only thing that can
        // put the slot back, so a guard that gives up on a poisoned lock leaks
        // exactly what it exists to stop.
        let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
        {
            let mut held = ledger.lock().expect("ledger lock");
            lent(&mut held, lease_id, grantee);
            held.enter_relay(lease_id, &grantee, 1, crate::now_ms())
                .expect("the first relay is admitted");
        }
        let poisoner = std::sync::Arc::clone(&ledger);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.lock().expect("ledger lock");
            panic!("a thread that dies holding the ledger");
        })
        .join();
        assert!(panicked.is_err(), "the fixture really did poison the lock");
        assert!(
            ledger.lock().is_err(),
            "positive control: the lock reads poisoned, or the leg below measures nothing"
        );
        {
            let _slot = RelaySlot {
                ledger: &ledger,
                lease_id,
                returned: false,
            };
        }
        let held = match ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(
            held.inflight(lease_id),
            0,
            "a poisoned ledger lock may not strand the lease's in-flight count for its whole \
             life"
        );
    }

    /// A slot the caller already gave back is not given back twice.
    ///
    /// The debit path releases under the same lock it debits on, so nothing
    /// between the two can move the lease. `leave_relay` saturates at zero, so
    /// a double release would be silent rather than loud, which is why this is
    /// asserted with a SECOND relay in flight: the second slot is what a double
    /// release would take away.
    ///
    /// Watched red: delete the `returned` early return from `Drop`.
    #[test]
    fn a_slot_the_caller_returned_is_not_returned_again() {
        let grantee = PeerId([12_u8; 32]);
        let lease_id = 0x5108_u128;
        let ledger = std::sync::Mutex::new(Ledger::new());
        {
            let mut held = ledger.lock().expect("ledger lock");
            let now = crate::now_ms();
            held.record_scoped(
                Lease {
                    lease_id,
                    window: Window::SevenDay,
                    unit: LeaseUnit::Fraction(0.50),
                    granted_at_ms: now,
                    expires_at_ms: now + 300_000,
                    spent: 0.0,
                    max_inflight: 4,
                    until: None,
                },
                grantee,
                tcr_peer_wire::LendScope::All,
            );
            held.note_owner_headroom(Window::SevenDay, 0.30);
            held.enter_relay(lease_id, &grantee, 1, now)
                .expect("the first relay is admitted");
            held.enter_relay(lease_id, &grantee, 2, now)
                .expect("the second relay is admitted");
            // The first request's own release, where the debit path writes it.
            held.leave_relay(lease_id);
        }
        {
            let _slot = RelaySlot {
                ledger: &ledger,
                lease_id,
                returned: true,
            };
        }
        assert_eq!(
            ledger.lock().expect("ledger lock").inflight(lease_id),
            1,
            "the second relay is still in flight: a guard must not give back a slot its \
             caller already returned"
        );
    }
}
