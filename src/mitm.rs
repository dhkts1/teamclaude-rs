//! MITM / forward-proxy "backwards mode": a hybrid listener that serves the
//! existing base-URL proxy **and** terminates `HTTPS_PROXY`-style `CONNECT`
//! tunnels on the same port, so a client with `HTTPS_PROXY=http://127.0.0.1:<port>`
//! is a drop-in against `tcr`.
//!
//! How a request flows (mirrors `teamclaude/src/mitm.js`, reusing `tcr`'s
//! account-selection + token-injection instead of reimplementing the forward):
//!   1. A client sends `CONNECT <host>:<port>`. We peek the request line.
//!   2. `api.anthropic.com` and `platform.claude.com` (Claude Code's own OAuth
//!      refresh host, `POST /v1/oauth/token`) are MITM-terminated **only when
//!      the leaf certificate actually loaded for this process covers that
//!      host** — see [`host_allowed`] (policy) and
//!      [`host_covered_by_loaded_leaf`] (SAN truthfulness) below, both of which
//!      must agree. When they do: we reply `200 Connection Established`,
//!      TLS-accept with our leaf (a cert the client already trusts via its
//!      CA), then serve HTTP over the TLS stream through the SAME axum router
//!      as base-URL mode — authenticate, select an account, inject the pooled
//!      `Bearer`, forward to the real upstream.
//!   3. Every OTHER host — and any policy-allowed host the loaded leaf does
//!      NOT cover — is **blind-tunneled**: a raw TCP byte-pipe to
//!      `<host>:<port>` with TLS left untouched (we never see plaintext and
//!      inject nothing). This matches the JS proxy's open tunnel for
//!      everything else Claude Code needs (its bridge, telemetry) through the
//!      same `HTTPS_PROXY`, and it is ALSO the anti-outage fallback for a host
//!      we intend to intercept but whose currently-loaded cert cannot present:
//!      claiming a host and then failing the TLS handshake is strictly worse
//!      than tunneling it untouched. Safe here because `tcr` binds
//!      `127.0.0.1` only — reachable solely by the local user, who can already
//!      open any connection directly, so it is no wider a surface than the
//!      shell itself.
//!
//! The decrypted request arrives in origin form (`POST /v1/messages` with
//! `Host: api.anthropic.com`), so the router routes it to `manager.upstream()`
//! verbatim — no proxy handler change is needed. The outbound client keeps
//! `.no_proxy()` (set in [`crate::manager`]) so we never loop our own upstream
//! back through an ambient `HTTPS_PROXY`.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt as _;

use crate::manager::Manager;

/// Hosts we intend to MITM-terminate + inject tokens for — a POLICY list, not
/// a firewall (everything else on `CONNECT` is blind-tunneled, no
/// interception). This is necessary but not sufficient for interception:
/// [`handle_connect`] additionally requires [`host_covered_by_loaded_leaf`],
/// because being on this list says nothing about whether the leaf actually
/// loaded for this process can present a valid certificate for the host — see
/// the module doc comment and [`load_tls_in`]'s design comment.
///
/// `console.anthropic.com` and `platform.anthropic.com` were removed 2026-08:
/// measured zero references in three consecutive Claude Code releases
/// (2.1.232-234) against the shipped binary, i.e. dead. `platform.claude.com`
/// was added in their place: it is the host Claude Code's own OAuth refresh
/// (`POST /v1/oauth/token`) targets — 2 occurrences in the same binary — and
/// `src/proxy.rs`'s `RelayMode::Raw` handling for that path was unreachable in
/// MITM mode without it.
pub const ALLOWED_HOSTS: &[&str] = &["api.anthropic.com", "platform.claude.com"];

/// SAN list for the rcgen fallback leaf (the policy allowlist + the JS test
/// host, kept so a regenerated chain still answers the credential-free
/// `www.example.org` probe). This is also the REQUIRED coverage set: a
/// persisted leaf whose sidecar (see [`load_tls_in`]) does not match this
/// exactly is treated as stale and re-minted.
const FALLBACK_LEAF_SANS: &[&str] = &[
    "api.anthropic.com",
    "platform.claude.com",
    "www.example.org",
];

/// Process-wide snapshot of the SAN coverage of whatever leaf [`load_tls`]
/// most recently loaded for this process, populated by [`load_tls_in`] as a
/// side effect on every successful return. This exists ONLY because the
/// `tls: Option<Arc<TlsAcceptor>>` threaded from `server.rs` through
/// `serve`/`serve_with_shutdown`/`handle_conn`/`handle_connect` carries just
/// the acceptor, not the `TlsAssets` it came from — so [`handle_connect`],
/// which has no other way to reach the coverage of the leaf it is about to
/// present, reads it here via [`host_covered_by_loaded_leaf`]. Tests do NOT
/// rely on this global: they read [`TlsAssets::covered_hosts`] directly off
/// the value `load_tls_in` returns, so they stay deterministic under parallel
/// test execution in the same binary. Starts empty — "nothing loaded yet" —
/// which is the safe default: under-claiming coverage means blind-tunnel,
/// never a broken handshake.
static COVERED_HOSTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn set_covered_hosts(hosts: &[String]) {
    if let Ok(mut guard) = COVERED_HOSTS.lock() {
        *guard = hosts.to_vec();
    }
}

/// Is `host` present (case-insensitively) in `covered`, the SAN set of one
/// particular loaded leaf? Pure and parameterized so tests can assert against
/// [`TlsAssets::covered_hosts`] directly, without touching the process-wide
/// snapshot [`host_covered_by_loaded_leaf`] reads from in production.
fn leaf_covers(covered: &[String], host: &str) -> bool {
    covered.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// Is `host` covered by the SAN set of the leaf actually loaded for THIS
/// process, per the most recent [`load_tls`]/[`load_tls_in`] call? This is
/// independent of [`host_allowed`]'s policy question — a host can be
/// policy-allowed and still fail this, e.g. right after a SAN change and
/// before a re-mint completes, or when the reused JS leaf's coverage is
/// unknown (see the conservative branch-A default in [`load_tls_in`]).
fn host_covered_by_loaded_leaf(host: &str) -> bool {
    COVERED_HOSTS
        .lock()
        .is_ok_and(|hosts| leaf_covers(&hosts, host))
}

const RESP_CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const RESP_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Loaded TLS material plus the CA path to advertise via `NODE_EXTRA_CA_CERTS`.
pub struct TlsAssets {
    pub acceptor: TlsAcceptor,
    /// The CA cert clients should trust to accept [`Self::acceptor`]'s leaf.
    /// `None` only when we cannot name one — a reused leaf with no companion CA
    /// beside it. Callers that must *tell* a client what to trust (see-through
    /// mode in `tcr run`) treat `None` as "cannot do that", not as "no CA needed":
    /// an unnameable CA may still be trusted ambiently, but we cannot prove it.
    pub ca_path: Option<PathBuf>,
    /// SAN coverage of [`Self::acceptor`]'s leaf, as actually loaded — not
    /// derived from [`ALLOWED_HOSTS`] or [`FALLBACK_LEAF_SANS`], because which
    /// branch [`load_tls_in`] took determines what this really is (see its
    /// design comment). This is the field that makes the CONNECT interception
    /// decision SAN-truthful: [`host_covered_by_loaded_leaf`] reads a
    /// process-wide snapshot of it.
    pub covered_hosts: Vec<String>,
}

/// Is `host` one we intend to MITM-terminate, by POLICY? Case-insensitive.
///
/// This alone does not mean we WILL intercept `host` — the interception
/// decision in [`handle_connect`] additionally requires
/// [`host_covered_by_loaded_leaf`], because this function says nothing about
/// whether the leaf loaded for this process can actually present a valid
/// certificate for it. Kept as a standalone, TLS-independent check because
/// `src/proxy.rs`'s base-URL-mode misroute guard also calls it, on a plain-HTTP
/// path where no TLS handshake — and therefore no SAN coverage question —
/// exists at all.
pub fn host_allowed(host: &str) -> bool {
    ALLOWED_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// Parse the target of a `CONNECT host:port HTTP/1.1` request line. Returns the
/// `(host, port)` or `None` if the line is not a well-formed CONNECT.
pub fn parse_connect_target(line: &str) -> Option<(String, u16)> {
    let mut parts = line.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let authority = parts.next()?;
    // `host:port` — split on the LAST colon so a stray colon in the host (never
    // valid here, but be defensive) does not steal the port.
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// `~/.config` — the dir holding the drop-in config and the shared certs. Mirrors
/// [`crate::config::default_path`]'s parent so cert reuse lands where the JS proxy
/// (and Gil's already-trusting clients) put them.
fn config_dir() -> PathBuf {
    crate::config::default_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Ensure a process-wide rustls crypto provider is installed before building a
/// `ServerConfig`. Two providers may be compiled (ours + reqwest's), which makes
/// `ServerConfig::builder()` panic when no default is set; installing one first
/// (ignoring "already installed") makes the builder deterministic.
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The CA + leaf tcr mints for itself. Named in exactly one place so the loader
/// and the writer cannot drift apart (they did: the loader probed `teamclaude-*`
/// while the writer produced `tcr-*`, so a minted chain was never read back and
/// every restart re-minted the CA out from under `NODE_EXTRA_CA_CERTS`).
struct MintedPaths {
    ca: PathBuf,
    /// The CA's private key — persisted (mode `0600`) so a future leaf SAN
    /// change re-mints only the leaf under the SAME CA, never rotating
    /// `NODE_EXTRA_CA_CERTS` out from under a client that already trusts it.
    /// Absent on any chain minted before this field existed; that absence is
    /// exactly what forces one unavoidable CA rotation the first time this
    /// runs against an older-minted chain (see [`load_tls_in`]).
    ca_key: PathBuf,
    leaf_cert: PathBuf,
    leaf_key: PathBuf,
    /// One host per line, the SAN set [`leaf_cert`](Self::leaf_cert) was
    /// minted with. Not a certificate field we could read back — a sidecar,
    /// deliberately, so drift detection needs no x509 parser: see the design
    /// comment on [`load_tls_in`] for why an x509 parser was ruled out (no new
    /// dependency, and the parse-then-compare shape is strictly more code and
    /// more attack surface than a one-host-per-line text file we wrote
    /// ourselves). A missing sidecar means "minted before this change", which
    /// is treated the same as a mismatch: coverage unknown → drifted →
    /// re-mint.
    sans: PathBuf,
}

impl MintedPaths {
    fn in_dir(dir: &Path) -> Self {
        Self {
            ca: dir.join("tcr-ca.pem"),
            ca_key: dir.join("tcr-ca.key"),
            leaf_cert: dir.join("tcr-leaf.pem"),
            leaf_key: dir.join("tcr-leaf.key"),
            sans: dir.join("tcr-leaf.sans"),
        }
    }

    fn all_present(&self) -> bool {
        self.ca.is_file() && self.leaf_cert.is_file() && self.leaf_key.is_file()
    }
}

fn fallback_hosts_vec() -> Vec<String> {
    FALLBACK_LEAF_SANS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Write `hosts`, one per line, to `path` (mode `0644` — not secret, just a
/// coverage record).
fn write_sans_sidecar(path: &Path, hosts: &[&str]) -> io::Result<()> {
    let mut content = String::new();
    for host in hosts {
        content.push_str(host);
        content.push('\n');
    }
    write_file(path, content.as_bytes(), 0o644)
}

/// Read a SAN sidecar written by [`write_sans_sidecar`]. `None` on any read
/// failure (including "does not exist") — callers treat that identically to a
/// present-but-mismatched sidecar: coverage unknown, so stale.
fn read_sans_sidecar(path: &Path) -> Option<Vec<String>> {
    let data = std::fs::read_to_string(path).ok()?;
    Some(
        data.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Set-equality (case-insensitive, order-independent) between a sidecar's
/// recorded hosts and the currently `required` coverage.
fn sans_match(sidecar: &[String], required: &[&str]) -> bool {
    if sidecar.len() != required.len() {
        return false;
    }
    let mut have: Vec<String> = sidecar.iter().map(|h| h.to_ascii_lowercase()).collect();
    let mut want: Vec<String> = required.iter().map(|h| h.to_ascii_lowercase()).collect();
    have.sort();
    want.sort();
    have == want
}

/// Load the TLS material for MITM: reuse `~/.config/teamclaude-leaf.pem` +
/// `.key` if present and loadable (the primary path — a cert Gil's clients
/// already trust), else reuse the chain we minted on an earlier run, else mint a
/// fresh CA+leaf with rcgen and persist it.
pub fn load_tls() -> anyhow::Result<TlsAssets> {
    load_tls_in(&config_dir())
}

/// [`load_tls`] against an explicit config dir (for tests).
///
/// Three branches, in order, and this is the function that makes the whole
/// module SAN-truthful — every return path sets `covered_hosts` to what the
/// chosen leaf can ACTUALLY present, never to the policy list:
///
/// - **A: the JS proxy's `teamclaude-leaf.pem`.** Its SAN coverage is
///   unknowable here without an x509 parser (deliberately not added — see
///   below), so `covered_hosts` is the single conservative entry
///   `api.anthropic.com`: under-claiming means a blind tunnel for
///   `platform.claude.com` (today's safe behavior on this path), while
///   over-claiming would mean presenting this leaf for a host it may not
///   cover, breaking the TLS handshake outright — a live OAuth-refresh
///   outage. Asymmetric costs, so the conservative side is the only side.
/// - **B: our own previously-minted chain.** Reused ONLY when a sidecar file
///   beside it (`tcr-leaf.sans`, one host per line, written by every mint)
///   matches [`FALLBACK_LEAF_SANS`] exactly. A leaf minted before this SAN set
///   changed carries the OLD coverage — reusing it unconditionally is exactly
///   the bug this change exists to fix (see the module doc comment): it is
///   what let a "just add the host to a const" change silently break the
///   handshake. A sidecar, not a parsed certificate extension, because rcgen's
///   x509-parser feature is a new (transitive) dependency this change is not
///   approved to add, and re-deriving trust from a file we ourselves wrote is
///   simpler and no less trustworthy than parsing DER we ourselves wrote.
///   Mismatch (or a missing sidecar, meaning "minted before this field
///   existed") re-mints the leaf — reusing the persisted CA key when present,
///   so `NODE_EXTRA_CA_CERTS` survives (branch D); a chain minted before the
///   CA key was persisted forces exactly one more CA rotation, never more
///   than one.
/// - **C: mint fresh.** `covered_hosts` is `FALLBACK_LEAF_SANS`, known by
///   construction.
fn load_tls_in(dir: &Path) -> anyhow::Result<TlsAssets> {
    ensure_crypto_provider();
    let leaf_cert = dir.join("teamclaude-leaf.pem");
    let leaf_key = dir.join("teamclaude-leaf.key");

    if leaf_cert.is_file() && leaf_key.is_file() {
        match build_acceptor(&leaf_cert, &leaf_key) {
            Ok(acceptor) => {
                // The JS proxy writes its CA next to the leaf it signs. Report it
                // when it is there: a caller that has to TELL a client what to
                // trust (see-through mode passes it as NODE_EXTRA_CA_CERTS) cannot
                // derive the path itself, and reporting `None` here used to strand
                // it — on the very path Gil is actually on — with the answer
                // sitting one `join` away. Absent → `None`, same as before.
                let companion_ca = dir.join("teamclaude-ca.pem");
                let ca_path = companion_ca.is_file().then_some(companion_ca);
                // Conservative: this leaf's real SAN set is unknown, so claim
                // only the one host we know every such leaf has always carried.
                let covered_hosts = vec!["api.anthropic.com".to_string()];
                set_covered_hosts(&covered_hosts);
                tracing::info!(
                    cert = %leaf_cert.display(),
                    ca = ca_path.as_ref().map_or_else(String::new, |p| p.display().to_string()),
                    hosts = ?covered_hosts,
                    "MITM: reusing existing leaf certificate (conservative SAN coverage — unknown leaf)"
                );
                return Ok(TlsAssets {
                    acceptor,
                    ca_path,
                    covered_hosts,
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "MITM: reusing leaf failed — trying our own minted chain");
            }
        }
    }

    // Reuse the chain from an earlier mint, but only when its recorded SAN
    // coverage still matches what we require today — see the branch-B design
    // note above. Without the sidecar check this branch would (and did) serve
    // a stale leaf forever: `MintedPaths::all_present` says nothing about
    // WHAT the leaf covers, only that files exist.
    let minted = MintedPaths::in_dir(dir);
    if minted.all_present() {
        let fresh = read_sans_sidecar(&minted.sans)
            .is_some_and(|sans| sans_match(&sans, FALLBACK_LEAF_SANS));
        if fresh {
            match build_acceptor(&minted.leaf_cert, &minted.leaf_key) {
                Ok(acceptor) => {
                    let covered_hosts = fallback_hosts_vec();
                    set_covered_hosts(&covered_hosts);
                    tracing::info!(cert = %minted.leaf_cert.display(), hosts = ?covered_hosts, "MITM: reusing minted leaf certificate");
                    return Ok(TlsAssets {
                        acceptor,
                        ca_path: Some(minted.ca),
                        covered_hosts,
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "MITM: reusing minted leaf failed — re-minting");
                }
            }
        } else {
            tracing::info!(
                cert = %minted.leaf_cert.display(),
                "MITM: persisted leaf SAN coverage is stale (or from before this field existed) — re-minting"
            );
        }

        if let Some(assets) = remint_leaf_under_persisted_ca(&minted) {
            return Ok(assets);
        }
    }

    generate_and_persist(dir)
}

/// Branch D: sign a fresh leaf under the CA key already persisted beside
/// `minted`, when both the CA cert and CA key are on disk. `None` when there
/// is no persisted CA key to reuse (a chain minted before this field existed,
/// or any read/sign failure) — the caller falls through to
/// [`generate_and_persist`], which mints a brand-new CA. That fallback is the
/// one unavoidable CA rotation described on [`MintedPaths::ca_key`]; after it,
/// this branch is what keeps every later SAN change from rotating the CA
/// again.
fn remint_leaf_under_persisted_ca(minted: &MintedPaths) -> Option<TlsAssets> {
    if !(minted.ca.is_file() && minted.ca_key.is_file()) {
        return None;
    }
    let result = (|| -> anyhow::Result<TlsAssets> {
        let ca_key_pem = std::fs::read_to_string(&minted.ca_key)?;
        let ca = CaChain {
            cert_pem: None,
            key_pem: ca_key_pem,
        };
        let (leaf_pem, leaf_key_pem) = sign_leaf(FALLBACK_LEAF_SANS, &ca)?;
        write_file(&minted.leaf_cert, leaf_pem.as_bytes(), 0o644)?;
        write_file(&minted.leaf_key, leaf_key_pem.as_bytes(), 0o600)?;
        write_sans_sidecar(&minted.sans, FALLBACK_LEAF_SANS)?;
        let acceptor = build_acceptor(&minted.leaf_cert, &minted.leaf_key)?;
        let covered_hosts = fallback_hosts_vec();
        set_covered_hosts(&covered_hosts);
        tracing::info!(
            cert = %minted.leaf_cert.display(),
            hosts = ?covered_hosts,
            "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS stays valid"
        );
        Ok(TlsAssets {
            acceptor,
            ca_path: Some(minted.ca.clone()),
            covered_hosts,
        })
    })();
    match result {
        Ok(assets) => Some(assets),
        Err(err) => {
            tracing::warn!(error = %err, "MITM: re-mint under persisted CA failed — minting a fresh CA instead");
            None
        }
    }
}

/// Build a rustls `TlsAcceptor` presenting the leaf at `cert_path`/`key_path`.
fn build_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_certs(path: &Path) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let data = std::fs::read(path)?;
    let mut reader = std::io::BufReader::new(&data[..]);
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

fn load_key(path: &Path) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let data = std::fs::read(path)?;
    let mut reader = std::io::BufReader::new(&data[..]);
    rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path.display()))
}

/// rcgen fallback: mint a fresh CA + a leaf covering [`FALLBACK_LEAF_SANS`],
/// persist them to `~/.config/tcr-{ca,leaf}.pem` + `tcr-ca.key` + `tcr-leaf.key`
/// (both keys `0600`) plus the `tcr-leaf.sans` coverage sidecar, and hand back
/// an acceptor over the fresh leaf. The CA path is advertised so the user can
/// `export NODE_EXTRA_CA_CERTS=<ca>`.
///
/// Only ever mints a NEW CA — reusing an existing one is
/// [`remint_leaf_under_persisted_ca`]'s job, tried first by [`load_tls_in`].
/// This is the path that used to re-mint (and thus rotate) the CA on every
/// SAN change; persisting `tcr-ca.key` here is what makes that a one-time
/// cost instead of a recurring one.
fn generate_and_persist(dir: &Path) -> anyhow::Result<TlsAssets> {
    let ca = generate_ca()?;
    let ca_cert_pem = ca
        .cert_pem
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("generate_ca must always mint a cert_pem"))?;
    let (leaf_pem, leaf_key_pem) = sign_leaf(FALLBACK_LEAF_SANS, &ca)?;

    std::fs::create_dir_all(dir)?;
    let minted = MintedPaths::in_dir(dir);
    write_file(&minted.ca, ca_cert_pem.as_bytes(), 0o644)?;
    write_file(&minted.ca_key, ca.key_pem.as_bytes(), 0o600)?;
    write_file(&minted.leaf_cert, leaf_pem.as_bytes(), 0o644)?;
    write_file(&minted.leaf_key, leaf_key_pem.as_bytes(), 0o600)?;
    write_sans_sidecar(&minted.sans, FALLBACK_LEAF_SANS)?;

    let acceptor = build_acceptor(&minted.leaf_cert, &minted.leaf_key)?;
    let covered_hosts = fallback_hosts_vec();
    set_covered_hosts(&covered_hosts);
    eprintln!(
        "[tcr] minted a fresh MITM CA — trust it with:\n         export NODE_EXTRA_CA_CERTS={}",
        minted.ca.display()
    );
    tracing::info!(hosts = ?covered_hosts, "MITM: minted a fresh CA + leaf");
    Ok(TlsAssets {
        acceptor,
        ca_path: Some(minted.ca),
        covered_hosts,
    })
}

/// A CA's private key, PEM-encoded, plus its cert PEM when we happen to have
/// just minted it. `cert_pem` is carried ONLY for the freshly-minted case
/// ([`generate_ca`]) so [`generate_and_persist`] has something to write to
/// disk; re-signing a leaf under an already-persisted CA
/// ([`remint_leaf_under_persisted_ca`]) never reads or needs it — it re-derives
/// the issuer from [`ca_params`] instead, which is what lets that path skip
/// parsing the persisted cert back (no x509 parser dependency).
struct CaChain {
    cert_pem: Option<String>,
    key_pem: String,
}

/// The CA's parameters, reconstructed identically on every mint AND every
/// leaf re-mint — never parsed back from a persisted certificate. This is the
/// mechanism that lets [`sign_leaf`] sign a fresh leaf under a previously
/// generated CA key (branch D) with no x509 parser: [`Issuer::from_params`]
/// only needs a `CertificateParams` + the signing key, and as long as this
/// function is the ONLY place those params are constructed, a re-mint's
/// issuer always matches the CA already on disk — same distinguished name,
/// same key usages, same `is_ca` — because a self-signed cert's subject IS
/// those same params, whichever call minted it. (`Issuer::from_ca_cert_pem`
/// would let a re-mint parse the persisted CA cert directly instead, but that
/// method is gated behind rcgen's `x509-parser` feature, a new transitive
/// dependency not approved for this change.)
fn ca_params() -> anyhow::Result<rcgen::CertificateParams> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyUsagePurpose};
    let mut params = CertificateParams::new(Vec::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "tcr Local CA");
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    Ok(params)
}

/// Mint a brand-new CA key + self-signed cert.
fn generate_ca() -> anyhow::Result<CaChain> {
    let ca_key = rcgen::KeyPair::generate()?;
    let params = ca_params()?;
    let ca_cert = params.self_signed(&ca_key)?;
    Ok(CaChain {
        cert_pem: Some(ca_cert.pem()),
        key_pem: ca_key.serialize_pem(),
    })
}

/// Sign a fresh leaf for `hosts` under `ca`'s key, reconstructing the SAME CA
/// params [`generate_ca`] used (via [`ca_params`]) so the leaf's issuer
/// matches whatever CA certificate is already persisted on disk, without ever
/// parsing it back.
fn sign_leaf(hosts: &[&str], ca: &CaChain) -> anyhow::Result<(String, String)> {
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair, KeyUsagePurpose,
    };

    let ca_key = KeyPair::from_pem(&ca.key_pem)?;
    let issuer_params = ca_params()?;
    let issuer = Issuer::from_params(&issuer_params, ca_key);

    let leaf_key = KeyPair::generate()?;
    let san_list: Vec<String> = hosts.iter().map(|s| (*s).to_string()).collect();
    let mut leaf_params = CertificateParams::new(san_list)?;
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, hosts[0]);
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer)?;

    Ok((leaf_cert.pem(), leaf_key.serialize_pem()))
}

/// Generate a fresh CA + a leaf for `hosts` with rcgen. Returns
/// `(ca_cert_pem, leaf_cert_pem, leaf_key_pem, ca_key_pem)`. A thin
/// convenience over [`generate_ca`] + [`sign_leaf`] for call sites that want a
/// complete standalone chain in one call — test-only: production always goes
/// through [`generate_ca`]/[`sign_leaf`] directly ([`generate_and_persist`],
/// [`remint_leaf_under_persisted_ca`]) so a re-mint never needs a NEW CA cert.
#[cfg(test)]
fn generate_chain(hosts: &[&str]) -> anyhow::Result<(String, String, String, String)> {
    let ca = generate_ca()?;
    let ca_cert_pem = ca
        .cert_pem
        .clone()
        .ok_or_else(|| anyhow::anyhow!("generate_ca must always mint a cert_pem"))?;
    let (leaf_pem, leaf_key_pem) = sign_leaf(hosts, &ca)?;
    Ok((ca_cert_pem, leaf_pem, leaf_key_pem, ca.key_pem))
}

/// Write `data` to `path` at exactly `mode`, whether or not `path` already exists.
///
/// The `.mode()` on the open is what keeps a freshly-created key from ever being
/// visible at a wider mode, even momentarily. It is NOT sufficient on its own:
/// `OpenOptionsExt::mode()` applies only to the creating open and is ignored
/// outright when the file already exists, so a stale `tcr-leaf.key` left at `0644`
/// by an older build, an interrupted write, a restore, or a umask accident would
/// survive regeneration world-readable — a readable MITM private key lets anything
/// holding it impersonate every host this proxy intercepts. Hence the post-write
/// re-assert, the same belt-and-braces `crate::config` uses on its token file.
fn write_file(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(data)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Run the hybrid listener forever: accept, classify each connection
/// (`CONNECT` → MITM, else base-URL), and serve it on its own task. `tls` is
/// `None` only when TLS material could not be loaded at all — CONNECT then
/// answers `503`, while base-URL mode keeps working.
///
/// Never returns. A caller that needs to stop serving wants
/// [`serve_with_shutdown`]; this is the shape the in-process tests use, where
/// the loop dies with the test's runtime.
pub async fn serve(listener: TcpListener, manager: Arc<Manager>, tls: Option<Arc<TlsAcceptor>>) {
    serve_with_shutdown(listener, manager, tls, std::future::pending::<()>()).await;
}

/// [`serve`], plus a shutdown branch on the accept loop.
///
/// Returning drops the `listener`, so the port stops accepting the moment
/// `shutdown` resolves — no `abort()` from outside, which is what a library
/// caller could not rely on. Connections already accepted are NOT touched: each
/// runs on its own detached task and a proxied response can be a long stream, so
/// cutting one at shutdown would be strictly worse than letting it finish. Same
/// property the old `server.abort()` had, now stated instead of incidental.
///
/// `biased` so a pending shutdown wins over a ready accept: under load an
/// unbiased `select!` picks randomly, and shutdown must not be starved by
/// traffic.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut shutdown => {
                tracing::info!("accept loop shutting down; no longer accepting connections");
                return;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                // A transient accept error must not spin the loop hot.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let manager = manager.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, peer, manager, tls).await {
                tracing::debug!(error = %err, "connection ended with error");
            }
        });
    }
}

/// Classify one accepted connection by peeking its first bytes, then dispatch.
async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
) -> io::Result<()> {
    if peek_is_connect(&stream).await? {
        handle_connect(stream, peer, manager, tls).await
    } else {
        // Base-URL mode: cleartext straight through the router. `serve_http`
        // auto-negotiates, so an h2-prior-knowledge client is served as h2 here
        // and everything else stays h1.
        serve_http(stream, peer, manager).await;
        Ok(())
    }
}

/// Peek (non-destructively) at the first bytes to decide if this is a `CONNECT`
/// tunnel. Leaves every byte in the socket buffer so the base-URL path hands an
/// untouched stream to hyper.
async fn peek_is_connect(stream: &TcpStream) -> io::Result<bool> {
    const NEEDLE: &[u8] = b"CONNECT ";
    let mut buf = [0u8; 8];
    // A localhost client delivers the request line in one segment; the bounded
    // retry only covers the pathological byte-dribble case without spinning.
    for _ in 0..64 {
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            return Ok(false); // EOF before any request line
        }
        let cmp = n.min(NEEDLE.len());
        if buf[..cmp] != NEEDLE[..cmp] {
            return Ok(false); // definitively not a CONNECT
        }
        if n >= NEEDLE.len() {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(false)
}

/// Handle a `CONNECT`: read the request head, enforce the allowlist, reply `200`,
/// TLS-accept, and serve the decrypted traffic through the router. `serve_http`
/// auto-negotiates, but this path is h1 in practice: [`build_acceptor`]'s rustls
/// `ServerConfig` advertises no ALPN protocols, so a client is never offered `h2`
/// on this handshake.
async fn handle_connect(
    mut stream: TcpStream,
    peer: SocketAddr,
    manager: Arc<Manager>,
    tls: Option<Arc<TlsAcceptor>>,
) -> io::Result<()> {
    let head = read_request_head(&mut stream).await?;
    let first_line = head.lines().next().unwrap_or("");
    let Some((host, port)) = parse_connect_target(first_line) else {
        stream.write_all(RESP_BAD_REQUEST).await?;
        stream.flush().await?;
        return Ok(());
    };

    // A non-policy-allowed host → blind tunnel (raw byte-pipe, no
    // interception). This is the open-tunnel behavior the JS proxy provides
    // and Claude Code relies on to reach its bridge and telemetry through the
    // same HTTPS_PROXY. TLS is never terminated here, so nothing is decrypted
    // or injected — just forwarded end-to-end.
    //
    // A policy-allowed host the LOADED LEAF does not cover takes the same
    // fallback — this is the SAN-truthfulness check (see the module doc
    // comment and `load_tls_in`'s design comment). Presenting a leaf for a
    // host it cannot cover fails the client's TLS handshake outright; blind-
    // tunneling it is strictly safer, and it is exactly today's behavior for
    // an uncovered host, not a new failure mode.
    if !host_allowed(&host) {
        return tunnel(stream, &host, port).await;
    }
    if !host_covered_by_loaded_leaf(&host) {
        tracing::debug!(
            %host,
            "MITM: host is policy-allowed but the loaded leaf does not cover it — blind-tunneling instead of a broken handshake"
        );
        return tunnel(stream, &host, port).await;
    }

    let Some(acceptor) = tls else {
        tracing::warn!(%host, "MITM: no TLS material — cannot terminate CONNECT");
        stream.write_all(RESP_UNAVAILABLE).await?;
        stream.flush().await?;
        return Ok(());
    };

    stream.write_all(RESP_CONNECT_OK).await?;
    stream.flush().await?;

    let tls_stream = match acceptor.accept(stream).await {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%host, error = %err, "MITM: TLS handshake failed");
            return Ok(());
        }
    };
    serve_http(tls_stream, peer, manager).await;
    Ok(())
}

/// Blind-tunnel a non-allowlisted `CONNECT`: open a raw TCP connection to
/// `host:port` and pipe bytes both directions until either side closes. TLS is
/// never terminated, so the client's end-to-end encryption to the real host is
/// untouched and no credentials are involved — a plain forward tunnel, the same
/// open-tunnel behavior the JS `teamclaude` proxy provides (and Claude Code needs
/// to reach `platform.claude.com`, its bridge, and telemetry). Only reachable
/// from `127.0.0.1`, so it is no wider a network surface than the local shell.
async fn tunnel(mut client: TcpStream, host: &str, port: u16) -> io::Result<()> {
    let mut upstream = match TcpStream::connect((host, port)).await {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%host, port, error = %err, "MITM: tunnel connect failed");
            client.write_all(RESP_UNAVAILABLE).await?;
            client.flush().await?;
            return Ok(());
        }
    };
    client.write_all(RESP_CONNECT_OK).await?;
    client.flush().await?;
    // Pipe until EOF in either direction; a mid-tunnel error is non-fatal — log
    // it at debug and let both halves drop.
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((c2u, u2c)) => tracing::debug!(%host, port, c2u, u2c, "MITM: tunnel closed"),
        Err(err) => tracing::debug!(%host, port, error = %err, "MITM: tunnel error"),
    }
    Ok(())
}

/// Read an HTTP request head (request line + headers) up to and including the
/// blank-line terminator, byte-by-byte so we never consume into the body / the
/// following TLS ClientHello. Bounded at 16 KiB.
async fn read_request_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Serve HTTP over `io` through the existing axum router — same auth, rotation,
/// injection, and streaming for both entry points.
///
/// THE single serving function: both call sites land here — [`handle_conn`]'s
/// base-URL branch with a raw TCP stream, and [`handle_connect`] with a
/// TLS-terminated MITM stream. Anything changed here changes both paths.
///
/// The protocol is auto-negotiated (h1, or h2 via ALPN / cleartext prior
/// knowledge), not fixed at HTTP/1.1. In practice the two paths differ:
/// base-URL clients can reach h2 through prior knowledge, while MITM-terminated
/// CONNECT traffic stays h1 because the rustls `ServerConfig` built in
/// [`build_acceptor`] advertises no ALPN protocols, so `h2` is never offered on
/// that handshake.
async fn serve_http<I>(io: I, peer: SocketAddr, manager: Arc<Manager>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Session affinity (opt-in): when enabled, mint ONE session key for this whole
    // connection (= one `claude` session: one CONNECT tunnel is one process). The
    // "one connection is one session" premise used to rest on the server being
    // HTTP/1.1; it now rests on the connection, which is the property that actually
    // matters — every request multiplexed over one connection shares this key, on h1
    // and h2 alike. The MITM path is h1 regardless (no ALPN advertised, see
    // `build_acceptor`); a base-URL h2 client simply pins all its streams together.
    // The key's PRESENCE is what switches affinity on for
    // this connection's requests; the routing key itself is always derived from a
    // stable client identity in `proxy::stable_session_key` (no stable identity →
    // the request routes unpinned). The affinity map is bounded by a size cap + LRU
    // eviction in `Manager::select` — stable pins intentionally survive reconnects —
    // so there is no disconnect-release. When disabled, no key is minted and nothing
    // is injected, so `select` receives `affinity = None` and the path stays inert.
    let session_key = manager
        .session_affinity_enabled()
        .then(|| manager.next_session_key());

    let router = crate::proxy::app(manager);
    let service =
        hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
            // Tag the request with the client's address so the auth layer can exempt
            // loopback clients from the api-key gate (see proxy::ClientAddr).
            req.extensions_mut().insert(crate::proxy::ClientAddr(peer));
            // When affinity is on, tag every request on this connection with the
            // connection's session key so selection pins to one account.
            if let Some(key) = session_key {
                req.extensions_mut().insert(crate::proxy::SessionKey(key));
            }
            // The router is always ready; `oneshot` drives readiness + call and is
            // Infallible, so no `.unwrap()` sits on the request hot path.
            router.clone().oneshot(req.map(axum::body::Body::new))
        });
    // Auto-negotiating builder (h1 or h2 via ALPN/prior-knowledge detection) so
    // base-URL clients that support h2 aren't forced down to h1.
    //
    // Resource limit worth knowing about: the h2 half carries hyper's default
    // `max_concurrent_streams: Some(200)` (hyper 1.11.0
    // `src/proto/h2/server.rs:69`, applied at :143), which hyper-util's `auto`
    // builder inherits by constructing a stock `http2::Builder`. So a single h2
    // connection is capped at 200 concurrent streams — a ceiling the base-URL path
    // did not have while it was h1-only, where concurrency was bounded by the
    // client's connection count instead. Left at hyper's default deliberately;
    // raise it with `.http2().max_concurrent_streams(n)` if a client ever hits it.
    if let Err(err) =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(hyper_util::rt::TokioIo::new(io), service)
            .await
    {
        tracing::debug!(error = %err, "http connection error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_file`'s `mode` must be enforced when the destination ALREADY exists,
    /// not only when it is created. `OpenOptionsExt::mode()` applies solely to the
    /// creating open, so without a post-write `set_permissions` a pre-existing
    /// `tcr-leaf.key` at `0644` — an older build, an interrupted write, a restore,
    /// a umask accident — survives regeneration world-readable while the call site
    /// reads as though it asked for `0600`. That file is the MITM leaf's private
    /// key: anything that can read it can impersonate every intercepted host.
    #[test]
    fn write_file_enforces_mode_on_an_existing_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "tcr-mitm-mode-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tcr-leaf.key");

        // Pre-create the destination at a looser mode, as a stale artifact would be.
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_file(&path, b"fresh key material", 0o600).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "an overwritten private key must end up at 0600, got {mode:o}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh key material");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minted chain must be reused on the next load. Re-minting silently
    /// invalidates the `NODE_EXTRA_CA_CERTS` the user exported after the first
    /// run, so the fallback path breaks on its second use.
    #[test]
    fn load_tls_reuses_a_minted_chain_across_restarts() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_tls_in(&dir).expect("first load mints a chain");
        let advertised_ca = first.ca_path.expect("a minted chain advertises its CA");
        let leaf_as_minted = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");

        let second = load_tls_in(&dir).expect("second load reuses the chain");
        let leaf_after_reload = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");

        assert_eq!(
            second.ca_path.as_deref(),
            Some(advertised_ca.as_path()),
            "the same CA must stay advertised across restarts"
        );
        assert_eq!(
            leaf_as_minted, leaf_after_reload,
            "the persisted leaf must be reused, not re-minted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pre-existing JS leaf still wins over our own minted chain, and with no
    /// companion CA beside it there is no CA we can name.
    #[test]
    fn load_tls_prefers_the_preexisting_js_leaf() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-jsleaf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let (_ca, leaf, key, _ca_key) =
            generate_chain(FALLBACK_LEAF_SANS).expect("mint a stand-in JS leaf");
        write_file(&dir.join("teamclaude-leaf.pem"), leaf.as_bytes(), 0o644).expect("write leaf");
        write_file(&dir.join("teamclaude-leaf.key"), key.as_bytes(), 0o600).expect("write key");

        let assets = load_tls_in(&dir).expect("load");
        assert!(
            assets.ca_path.is_none(),
            "with no companion CA on disk there is none to advertise"
        );
        assert!(
            !dir.join("tcr-leaf.pem").exists(),
            "the JS leaf path must not mint a competing chain"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reusing the JS leaf must advertise the CA sitting beside it. This is the
    /// path Gil's install is on, and see-through mode (`tcr run` handing claude a
    /// first-party base URL + `NODE_EXTRA_CA_CERTS`) is unavailable without it —
    /// the CA the client needs is the one that signed the leaf we present.
    #[test]
    fn load_tls_advertises_the_companion_ca_next_to_a_reused_leaf() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-jsca-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let (ca, leaf, key, _ca_key) =
            generate_chain(FALLBACK_LEAF_SANS).expect("mint a stand-in JS chain");
        write_file(&dir.join("teamclaude-leaf.pem"), leaf.as_bytes(), 0o644).expect("write leaf");
        write_file(&dir.join("teamclaude-leaf.key"), key.as_bytes(), 0o600).expect("write key");
        write_file(&dir.join("teamclaude-ca.pem"), ca.as_bytes(), 0o644).expect("write ca");

        let assets = load_tls_in(&dir).expect("load");
        assert_eq!(
            assets.ca_path.as_deref(),
            Some(dir.join("teamclaude-ca.pem").as_path()),
            "the CA beside the reused leaf must be advertised"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_connect_accepts_host_port() {
        assert_eq!(
            parse_connect_target("CONNECT api.anthropic.com:443 HTTP/1.1"),
            Some(("api.anthropic.com".to_string(), 443))
        );
        // Method match is case-insensitive; trailing version optional.
        assert_eq!(
            parse_connect_target("connect example.com:8443"),
            Some(("example.com".to_string(), 8443))
        );
    }

    #[test]
    fn parse_connect_rejects_malformed() {
        assert_eq!(parse_connect_target("GET / HTTP/1.1"), None);
        assert_eq!(parse_connect_target("CONNECT api.anthropic.com"), None); // no port
        assert_eq!(parse_connect_target("CONNECT :443 HTTP/1.1"), None); // no host
        assert_eq!(parse_connect_target("CONNECT host:notaport"), None);
        assert_eq!(parse_connect_target(""), None);
    }

    #[test]
    fn host_allowlist_is_case_insensitive_and_closed() {
        assert!(host_allowed("api.anthropic.com"));
        assert!(host_allowed("API.Anthropic.COM"));
        // platform.claude.com replaced the two dead OAuth hosts below — see
        // ALLOWED_HOSTS's doc comment for the measured evidence.
        assert!(host_allowed("platform.claude.com"));
        assert!(host_allowed("Platform.Claude.COM"));
        // console.anthropic.com / platform.anthropic.com are dead in current
        // Claude Code (measured zero references across three releases) and must
        // no longer be policy-allowed.
        assert!(!host_allowed("console.anthropic.com"));
        assert!(!host_allowed("platform.anthropic.com"));
        // Everything else is NOT MITM-terminated — it is blind-tunneled instead
        // (host_allowed decides "decrypt + inject" vs "pass through", not allow
        // vs deny), so these all read false and take the tunnel path.
        assert!(!host_allowed("example.com"));
        assert!(!host_allowed("evil.api.anthropic.com.attacker.net"));
        assert!(!host_allowed("localhost"));
        assert!(!host_allowed(""));
    }

    /// The load-bearing property: `platform.claude.com` is policy-allowed AND
    /// covered by a freshly-minted leaf (branch C, [`FALLBACK_LEAF_SANS`]), so
    /// it is intercepted.
    #[test]
    fn platform_claude_com_is_intercepted_when_the_loaded_leaf_covers_it() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-covers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let assets = load_tls_in(&dir).expect("mints a fresh chain covering FALLBACK_LEAF_SANS");
        assert!(
            host_allowed("platform.claude.com"),
            "platform.claude.com must be policy-allowed"
        );
        assert!(
            leaf_covers(&assets.covered_hosts, "platform.claude.com"),
            "a freshly minted leaf must cover platform.claude.com: {:?}",
            assets.covered_hosts
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The anti-outage property, and the most important test in this file: a
    /// policy-allowed host whose coverage is UNKNOWN (branch A, the reused JS
    /// leaf) must read as NOT covered, so `handle_connect` blind-tunnels it
    /// instead of presenting a leaf that will fail the TLS handshake.
    #[test]
    fn platform_claude_com_is_not_intercepted_when_the_loaded_leaf_lacks_it() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-uncovered-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // Stand in for a foreign/JS leaf whose SAN coverage tcr cannot verify —
        // realistically one that covers only api.anthropic.com.
        let (_ca, leaf, key, _ca_key) =
            generate_chain(&["api.anthropic.com"]).expect("mint a narrow stand-in leaf");
        write_file(&dir.join("teamclaude-leaf.pem"), leaf.as_bytes(), 0o644).expect("write leaf");
        write_file(&dir.join("teamclaude-leaf.key"), key.as_bytes(), 0o600).expect("write key");

        let assets = load_tls_in(&dir).expect("load");
        assert!(
            host_allowed("platform.claude.com"),
            "policy still allows it — the point is the SAN check catches what policy cannot"
        );
        assert!(
            !leaf_covers(&assets.covered_hosts, "platform.claude.com"),
            "branch A must not claim coverage it cannot verify: {:?}",
            assets.covered_hosts
        );
        assert!(
            leaf_covers(&assets.covered_hosts, "api.anthropic.com"),
            "branch A's one known-safe claim must still hold"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A persisted leaf whose sidecar SAN set does not match what we require
    /// today (or has no sidecar at all — a leaf minted before this field
    /// existed) must be re-minted, not served stale.
    #[test]
    fn a_stale_or_missing_sidecar_forces_a_remint() {
        let dir =
            std::env::temp_dir().join(format!("tcr-mitm-stale-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_tls_in(&dir).expect("first load mints a chain");
        let leaf_before = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");

        // Simulate a leaf minted under the OLD (pre-change) SAN set.
        write_sans_sidecar(
            &dir.join("tcr-leaf.sans"),
            &[
                "api.anthropic.com",
                "console.anthropic.com",
                "platform.anthropic.com",
                "www.example.org",
            ],
        )
        .expect("write stale sidecar");

        let second = load_tls_in(&dir).expect("second load re-mints on SAN drift");
        let leaf_after_drift = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
        assert_ne!(
            leaf_before, leaf_after_drift,
            "a SAN-drifted sidecar must trigger a fresh leaf, not a stale reuse"
        );
        assert!(leaf_covers(&second.covered_hosts, "platform.claude.com"));
        assert_eq!(
            first.ca_path, second.ca_path,
            "a leaf re-mint must not change which CA path is advertised"
        );

        // Now simulate a leaf minted before the sidecar field existed at all.
        std::fs::remove_file(dir.join("tcr-leaf.sans")).expect("remove sidecar");
        let third = load_tls_in(&dir).expect("third load re-mints on a missing sidecar");
        let leaf_after_missing = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
        assert_ne!(
            leaf_after_drift, leaf_after_missing,
            "a missing sidecar (pre-change leaf) must re-mint, not silently reuse unknown coverage"
        );
        assert!(leaf_covers(&third.covered_hosts, "platform.claude.com"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A persisted leaf whose sidecar MATCHES the required coverage is reused
    /// as-is — no gratuitous re-mint (already covered by
    /// `load_tls_reuses_a_minted_chain_across_restarts` above; this asserts
    /// the sidecar specifically is what's being checked, not just presence).
    #[test]
    fn a_matching_sidecar_is_reused_without_a_remint() {
        let dir =
            std::env::temp_dir().join(format!("tcr-mitm-fresh-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let _first = load_tls_in(&dir).expect("first load mints a chain + matching sidecar");
        let leaf_before = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");
        let ca_key_before =
            std::fs::read(dir.join("tcr-ca.key")).expect("CA key persisted on first mint");

        let _second = load_tls_in(&dir).expect("second load reuses the chain");
        let leaf_after = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
        let ca_key_after = std::fs::read(dir.join("tcr-ca.key")).expect("CA key still there");

        assert_eq!(
            leaf_before, leaf_after,
            "a matching sidecar must not trigger a re-mint"
        );
        assert_eq!(
            ca_key_before, ca_key_after,
            "no CA rotation should happen when nothing drifted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Branch D end-to-end: after a SAN-drift re-mint, the CA cert stays byte-
    /// identical (no rotation), AND a client trusting ONLY the persisted CA
    /// completes a real TLS handshake against the newly re-minted leaf — the
    /// strongest available proof the re-mint actually reused the persisted CA
    /// key rather than silently minting a new CA.
    #[tokio::test]
    async fn remint_after_san_drift_reuses_the_persisted_ca_key() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-ca-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_tls_in(&dir).expect("first load mints CA + leaf");
        let ca_pem_before = std::fs::read(dir.join("tcr-ca.pem")).expect("CA cert persisted");

        // Force a SAN drift so the next load must re-mint the leaf.
        write_sans_sidecar(&dir.join("tcr-leaf.sans"), &["api.anthropic.com"])
            .expect("write drifted sidecar");
        let second = load_tls_in(&dir).expect("second load re-mints under the SAME CA");
        let ca_pem_after = std::fs::read(dir.join("tcr-ca.pem")).expect("CA cert still there");

        assert_eq!(
            ca_pem_before, ca_pem_after,
            "a leaf re-mint must not rotate the CA cert"
        );
        assert_eq!(first.ca_path, second.ca_path);

        // Cryptographic proof: a client trusting ONLY the persisted CA must
        // complete a real TLS handshake against the re-minted leaf.
        ensure_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        let mut reader = std::io::BufReader::new(&ca_pem_after[..]);
        for cert in rustls_pemfile::certs(&mut reader) {
            roots
                .add(cert.expect("parse persisted CA cert"))
                .expect("add CA to root store");
        }
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = second.acceptor;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            acceptor
                .accept(stream)
                .await
                .expect("server-side handshake over the re-minted leaf must succeed")
        });

        let client_stream = TcpStream::connect(addr).await.unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("api.anthropic.com")
            .expect("valid DNS name")
            .to_owned();
        let client_tls = connector
            .connect(server_name, client_stream)
            .await
            .expect("a client trusting the persisted CA must accept the re-minted leaf");
        drop(client_tls);
        server.await.expect("server task must complete");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The blind tunnel pipes bytes to the requested upstream and back, and first
    /// answers `200 Connection Established` — proving a non-allowlisted CONNECT
    /// (e.g. platform.claude.com) passes straight through instead of being
    /// refused, which is what Claude Code needs from the forward proxy.
    #[tokio::test]
    async fn tunnel_pipes_bytes_through_to_upstream() {
        // Upstream: a one-shot echo server.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = echo.accept().await {
                let mut buf = [0u8; 64];
                if let Ok(n) = s.read(&mut buf).await {
                    let _ = s.write_all(&buf[..n]).await;
                }
            }
        });

        // A front socket standing in for the client<->tcr connection.
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        let mut client = TcpStream::connect(front_addr).await.unwrap();
        let (server_side, _) = front.accept().await.unwrap();

        // Tunnel the accepted (tcr) side out to the echo upstream.
        tokio::spawn(async move {
            let _ = tunnel(server_side, "127.0.0.1", echo_port).await;
        });

        // The tunnel first writes the CONNECT-OK line.
        let mut ok = [0u8; RESP_CONNECT_OK.len()];
        client.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, RESP_CONNECT_OK);

        // Bytes we send are piped to the upstream and echoed straight back.
        client.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
    }

    /// Cert load path (no network): mint a chain with the rcgen fallback, persist
    /// it, and prove `build_acceptor` loads the leaf into a rustls `ServerConfig`.
    #[test]
    fn generated_chain_loads_into_acceptor() {
        ensure_crypto_provider();
        let (ca_pem, leaf_pem, leaf_key_pem, ca_key_pem) =
            generate_chain(FALLBACK_LEAF_SANS).expect("generate chain");
        assert!(ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf_key_pem.contains("PRIVATE KEY"));
        assert!(ca_key_pem.contains("PRIVATE KEY"));

        let dir = std::env::temp_dir().join(format!("tcr-mitm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("leaf.pem");
        let key_path = dir.join("leaf.key");
        write_file(&cert_path, leaf_pem.as_bytes(), 0o644).unwrap();
        write_file(&key_path, leaf_key_pem.as_bytes(), 0o600).unwrap();

        // A single leaf cert parses out, and the acceptor builds without error.
        assert_eq!(load_certs(&cert_path).unwrap().len(), 1);
        assert!(build_acceptor(&cert_path, &key_path).is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `Manager` with one never-expiring dummy account and no proxy api-key,
    /// so a loopback GET on `/_tcr/status` is answered locally with no upstream
    /// call and no auth gate — the cheapest real route through `serve_http`'s
    /// router to observe which HTTP version `serve_connection` negotiated.
    fn dummy_manager() -> Arc<Manager> {
        let config = crate::config::Config {
            quarantined_accounts: Vec::new(),
            proxy: crate::config::ProxyConfig {
                port: 0,
                api_key: None,
                extra: serde_json::Map::new(),
            },
            upstream: "http://127.0.0.1:1".to_string(),
            switch_threshold: 0.90,
            pacing: crate::config::PacingConfig::default(),
            throttle: crate::config::ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            http1_only: false,
            accounts: vec![crate::config::Account {
                name: "dummy".to_string(),
                account_type: "oauth".to_string(),
                account_uuid: None,
                org_uuid: None,
                org_name: None,
                access_token: "at-dummy".to_string(),
                refresh_token: Some("rt-dummy".to_string()),
                expires_at: Some(crate::now_ms() + 3_600_000),
                priority: Some(0),
                switch_threshold: None,
                disabled: None,
                groups: None,
                extra: serde_json::Map::new(),
            }],
            group_settings: std::collections::HashMap::new(),
            extra: serde_json::Map::new(),
        };
        Manager::new(
            config,
            Arc::new(crate::oauth::NoRefresh),
            Arc::new(crate::probe::LiveUsageProber::new()),
            Arc::new(crate::warmer::LiveWarmer::new()),
            None,
        )
    }

    /// Start a base-URL-mode listener (`handle_conn`'s non-CONNECT branch, i.e.
    /// `serve_http` over a raw TCP stream — the exact path this change touches)
    /// and return its address.
    async fn spawn_base_url_listener() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let manager = dummy_manager();
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let manager = manager.clone();
                tokio::spawn(async move {
                    // Same shape as the production accept loop in
                    // `serve_with_shutdown`: a connection error is non-fatal but is
                    // never swallowed silently, so a test that fails because the
                    // connection died has the reason in the captured log rather
                    // than presenting as an unexplained client-side error.
                    if let Err(err) = handle_conn(stream, peer, manager, None).await {
                        tracing::debug!(error = %err, "connection ended with error");
                    }
                });
            }
        });
        addr
    }

    /// Discriminating control: an ordinary h1 client must still work after the
    /// swap to the auto-negotiating builder. If this regresses, the h2 test
    /// below is measuring a broken server, not a real negotiation.
    #[tokio::test]
    async fn base_url_mode_still_serves_http1() {
        let addr = spawn_base_url_listener().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client
            .get(format!("http://{addr}/_tcr/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    }

    /// The behavior this change exists to add: a base-URL client that speaks
    /// h2 via cleartext prior knowledge (no ALPN/TLS involved — this is the
    /// plain-TCP `serve_http` call site at `handle_conn`'s non-CONNECT branch)
    /// negotiates HTTP/2 instead of being forced onto HTTP/1.1.
    #[tokio::test]
    async fn base_url_mode_negotiates_http2_prior_knowledge() {
        let addr = spawn_base_url_listener().await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/_tcr/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.version(), reqwest::Version::HTTP_2);
    }
}
