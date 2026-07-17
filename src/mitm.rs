//! MITM / forward-proxy "backwards mode": a hybrid listener that serves the
//! existing base-URL proxy **and** terminates `HTTPS_PROXY`-style `CONNECT`
//! tunnels on the same port, so a client with `HTTPS_PROXY=http://127.0.0.1:<port>`
//! is a drop-in against `tcr`.
//!
//! How a request flows (mirrors `teamclaude/src/mitm.js`, reusing `tcr`'s
//! account-selection + token-injection instead of reimplementing the forward):
//!   1. A client sends `CONNECT <host>:<port>`. We peek the request line.
//!   2. `api.anthropic.com` (+ the OAuth hosts) is MITM-terminated: we reply
//!      `200 Connection Established`, TLS-accept with our leaf (a cert the client
//!      already trusts via its CA), then serve HTTP/1.1 over the TLS stream
//!      through the SAME axum router as base-URL mode — authenticate, select an
//!      account, inject the pooled `Bearer`, forward to the real upstream.
//!   3. Every OTHER host is **blind-tunneled**: a raw TCP byte-pipe to
//!      `<host>:<port>` with TLS left untouched (we never see plaintext and
//!      inject nothing). This matches the JS proxy's open tunnel, which Claude
//!      Code depends on to reach `platform.claude.com`, its bridge, and telemetry
//!      through the same `HTTPS_PROXY`. Safe here because `tcr` binds `127.0.0.1`
//!      only — reachable solely by the local user, who can already open any
//!      connection directly, so it is no wider a surface than the shell itself.
//!
//! The decrypted request arrives in origin form (`POST /v1/messages` with
//! `Host: api.anthropic.com`), so the router routes it to `manager.upstream()`
//! verbatim — no proxy handler change is needed. The outbound client keeps
//! `.no_proxy()` (set in [`crate::manager`]) so we never loop our own upstream
//! back through an ambient `HTTPS_PROXY`.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt as _;

use crate::manager::Manager;

/// Hosts we MITM-terminate + inject tokens for. Anything else on `CONNECT` is
/// blind-tunneled (a raw byte-pipe, no interception), so this is the set we
/// decrypt — NOT a firewall: non-Anthropic hosts still pass straight through.
///
/// The reused `teamclaude-leaf.pem` has SAN only for `api.anthropic.com` (+ the
/// built-in test host), so in practice only `api.anthropic.com` completes a TLS
/// handshake; the OAuth hosts are listed because the design allows them and the
/// rcgen fallback mints a leaf covering all of them.
pub const ALLOWED_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "console.anthropic.com",
    "platform.anthropic.com",
];

/// SAN list for the rcgen fallback leaf (allowlist + the JS test host, kept so a
/// regenerated chain still answers the credential-free `www.example.org` probe).
const FALLBACK_LEAF_SANS: &[&str] = &[
    "api.anthropic.com",
    "console.anthropic.com",
    "platform.anthropic.com",
    "www.example.org",
];

const RESP_CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const RESP_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Loaded TLS material plus the CA path to advertise via `NODE_EXTRA_CA_CERTS`.
pub struct TlsAssets {
    pub acceptor: TlsAcceptor,
    /// The CA cert clients should trust. `None` when we reused the pre-existing
    /// leaf whose CA the client is already configured to trust.
    pub ca_path: Option<PathBuf>,
}

/// Is `host` one we will MITM-terminate? Case-insensitive.
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
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
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
    leaf_cert: PathBuf,
    leaf_key: PathBuf,
}

impl MintedPaths {
    fn in_dir(dir: &Path) -> Self {
        Self {
            ca: dir.join("tcr-ca.pem"),
            leaf_cert: dir.join("tcr-leaf.pem"),
            leaf_key: dir.join("tcr-leaf.key"),
        }
    }

    fn all_present(&self) -> bool {
        self.ca.is_file() && self.leaf_cert.is_file() && self.leaf_key.is_file()
    }
}

/// Load the TLS material for MITM: reuse `~/.config/teamclaude-leaf.pem` +
/// `.key` if present and loadable (the primary path — a cert Gil's clients
/// already trust), else reuse the chain we minted on an earlier run, else mint a
/// fresh CA+leaf with rcgen and persist it.
pub fn load_tls() -> anyhow::Result<TlsAssets> {
    load_tls_in(&config_dir())
}

/// [`load_tls`] against an explicit config dir (for tests).
fn load_tls_in(dir: &Path) -> anyhow::Result<TlsAssets> {
    ensure_crypto_provider();
    let leaf_cert = dir.join("teamclaude-leaf.pem");
    let leaf_key = dir.join("teamclaude-leaf.key");

    if leaf_cert.is_file() && leaf_key.is_file() {
        match build_acceptor(&leaf_cert, &leaf_key) {
            Ok(acceptor) => {
                tracing::info!(cert = %leaf_cert.display(), "MITM: reusing existing leaf certificate");
                return Ok(TlsAssets {
                    acceptor,
                    ca_path: None,
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "MITM: reusing leaf failed — trying our own minted chain");
            }
        }
    }

    // Reuse the chain from an earlier mint. Without this the CA is re-minted on
    // every restart, silently invalidating the NODE_EXTRA_CA_CERTS the user was
    // told to export the first time. rcgen's default validity runs to 4096, so a
    // persisted leaf never ages out.
    let minted = MintedPaths::in_dir(dir);
    if minted.all_present() {
        match build_acceptor(&minted.leaf_cert, &minted.leaf_key) {
            Ok(acceptor) => {
                tracing::info!(cert = %minted.leaf_cert.display(), "MITM: reusing minted leaf certificate");
                return Ok(TlsAssets {
                    acceptor,
                    ca_path: Some(minted.ca),
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "MITM: reusing minted leaf failed — regenerating a fresh CA+leaf");
            }
        }
    }

    generate_and_persist(dir)
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

/// rcgen fallback: mint a CA + a leaf covering [`FALLBACK_LEAF_SANS`], persist
/// them to `~/.config/tcr-{ca,leaf}.pem` + `tcr-leaf.key` (key `0600`), and hand
/// back an acceptor over the fresh leaf. The CA path is advertised so the user
/// can `export NODE_EXTRA_CA_CERTS=<ca>`.
fn generate_and_persist(dir: &Path) -> anyhow::Result<TlsAssets> {
    let (ca_pem, leaf_pem, leaf_key_pem) = generate_chain(FALLBACK_LEAF_SANS)?;

    std::fs::create_dir_all(dir)?;
    let MintedPaths {
        ca: ca_path,
        leaf_cert: leaf_cert_path,
        leaf_key: leaf_key_path,
    } = MintedPaths::in_dir(dir);
    write_file(&ca_path, ca_pem.as_bytes(), 0o644)?;
    write_file(&leaf_cert_path, leaf_pem.as_bytes(), 0o644)?;
    write_file(&leaf_key_path, leaf_key_pem.as_bytes(), 0o600)?;

    let acceptor = build_acceptor(&leaf_cert_path, &leaf_key_path)?;
    eprintln!(
        "[tcr] minted a fresh MITM CA — trust it with:\n         export NODE_EXTRA_CA_CERTS={}",
        ca_path.display()
    );
    Ok(TlsAssets {
        acceptor,
        ca_path: Some(ca_path),
    })
}

/// Generate a CA + a leaf for `hosts` with rcgen. Returns
/// `(ca_cert_pem, leaf_cert_pem, leaf_key_pem)`.
fn generate_chain(hosts: &[&str]) -> anyhow::Result<(String, String, String)> {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };

    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "tcr Local CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let leaf_key = KeyPair::generate()?;
    let san_list: Vec<String> = hosts.iter().map(|s| s.to_string()).collect();
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

    Ok((ca_cert.pem(), leaf_cert.pem(), leaf_key.serialize_pem()))
}

fn write_file(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(data)?;
    Ok(())
}

/// Run the hybrid listener until the task is aborted: accept, classify each
/// connection (`CONNECT` → MITM, else base-URL), and serve it on its own task.
/// `tls` is `None` only when TLS material could not be loaded at all — CONNECT
/// then answers `503`, while base-URL mode keeps working.
pub async fn serve(listener: TcpListener, manager: Arc<Manager>, tls: Option<Arc<TlsAcceptor>>) {
    loop {
        let (stream, peer) = match listener.accept().await {
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
        // Base-URL mode: plain HTTP/1.1 straight through the router, unchanged.
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
/// TLS-accept, and serve the decrypted HTTP/1.1 through the router.
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

    // Non-allowlisted host → blind tunnel (raw byte-pipe, no interception). This
    // is the open-tunnel behavior the JS proxy provides and Claude Code relies on
    // to reach platform.claude.com, its bridge, and telemetry through the same
    // HTTPS_PROXY. TLS is never terminated here, so nothing is decrypted or
    // injected — just forwarded end-to-end.
    if !host_allowed(&host) {
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

/// Serve HTTP/1.1 over `io` (a raw TCP stream for base-URL mode, or a terminated
/// TLS stream for MITM) through the existing axum router — same auth, rotation,
/// injection, and streaming for both entry points.
async fn serve_http<I>(io: I, peer: SocketAddr, manager: Arc<Manager>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Session affinity (opt-in): when enabled, mint ONE session key for this whole
    // connection (= one `claude` session, since the server is HTTP/1.1: one CONNECT
    // tunnel is one process). This per-connection key is the fallback the proxy uses
    // when no stable identity (device_id + account_uuid) is present. The affinity map
    // itself is bounded by a size cap + LRU eviction in `Manager::select` — stable
    // pins intentionally survive reconnects — so there is no disconnect-release. When
    // disabled, no key is minted and nothing is injected, so `select` receives
    // `affinity = None` and the disabled path stays inert.
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
    if let Err(err) = hyper::server::conn::http1::Builder::new()
        .serve_connection(hyper_util::rt::TokioIo::new(io), service)
        .await
    {
        tracing::debug!(error = %err, "http connection error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The pre-existing JS leaf still wins over our own minted chain, and reports
    /// no CA to trust because its own CA is already trusted by the client.
    #[test]
    fn load_tls_prefers_the_preexisting_js_leaf() {
        let dir = std::env::temp_dir().join(format!("tcr-mitm-jsleaf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let (_ca, leaf, key) = generate_chain(FALLBACK_LEAF_SANS).expect("mint a stand-in JS leaf");
        write_file(&dir.join("teamclaude-leaf.pem"), leaf.as_bytes(), 0o644).expect("write leaf");
        write_file(&dir.join("teamclaude-leaf.key"), key.as_bytes(), 0o600).expect("write key");

        let assets = load_tls_in(&dir).expect("load");
        assert!(
            assets.ca_path.is_none(),
            "reusing the JS leaf must not advertise a CA"
        );
        assert!(
            !dir.join("tcr-leaf.pem").exists(),
            "the JS leaf path must not mint a competing chain"
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
        assert!(host_allowed("console.anthropic.com"));
        assert!(host_allowed("platform.anthropic.com"));
        // Everything else is NOT MITM-terminated — it is blind-tunneled instead
        // (host_allowed decides "decrypt + inject" vs "pass through", not allow
        // vs deny), so these all read false and take the tunnel path.
        assert!(!host_allowed("example.com"));
        assert!(!host_allowed("platform.claude.com"));
        assert!(!host_allowed("evil.api.anthropic.com.attacker.net"));
        assert!(!host_allowed("localhost"));
        assert!(!host_allowed(""));
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
        let (ca_pem, leaf_pem, leaf_key_pem) =
            generate_chain(FALLBACK_LEAF_SANS).expect("generate chain");
        assert!(ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf_key_pem.contains("PRIVATE KEY"));

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
}
