//! The client-identity check behind `controlAccount`.
//!
//! Everything a client does that is not inference — connectors, plugins,
//! skills, settings, the bootstrap — is bound to the client's OWN OAuth
//! identity: the connector transport (`mcp-proxy.anthropic.com`) never passes
//! through this proxy at all, and the bookkeeping paths are relayed with the
//! client's own bearer (`src/proxy.rs`, `CLIENT_CREDENTIAL_PREFIXES` and the
//! bearer rule in `relay_mode`). So "one account for MCP, many for quota" is
//! not something the proxy can arrange by picking an account; it is something
//! the CLIENT arranges by being logged in as that account, and the proxy can
//! only check. This module is that check.
//!
//! The proxy sees the client's bearer on every request (it strips it before
//! injecting a pooled one). On first sight of a bearer it resolves the identity
//! behind it once, via `/api/oauth/profile` on the configured upstream, caches
//! the answer under a hash of the bearer, and compares it to the control
//! account. A mismatch is refused (or only logged, per
//! [`crate::config::ControlIdentityMode`]). Measured before this existed
//! (2026-09-22): a keychain login that silently flipped to another account
//! produced two hours of wrong-org 403/404s and a `Server not found` on every
//! connector, with nothing naming the cause.
//!
//! Fail-open by design on anything that is not a verified mismatch: a profile
//! fetch that fails, or one that comes back with no email and no uuid, is
//! [`Verdict::Unknown`] and is NOT cached, so a transient upstream failure
//! neither blocks traffic nor pins a wrong answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex, OnceCell};

/// Bound on the resolved-identity cache. Every entry is one distinct bearer
/// the proxy has seen; a client refreshes its token every few hours, so even a
/// long-running fleet stays far below this. Reached only by a client that
/// mints a fresh bearer per request, and the answer to that is to drop the
/// cache wholesale rather than let it grow.
const CACHE_CAP: usize = 256;

/// The identity `controlAccount` resolves to, as the manager holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlIdentity {
    /// The account row's `name` — an email, optionally suffixed with `/org`
    /// (see [`crate::identity::email_of`]).
    pub name: String,
    /// The account's `accountUuid`, when the login that created the row
    /// captured one.
    pub account_uuid: Option<String>,
}

/// The identity a client bearer resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
}

impl ClientIdentity {
    /// The client as a human reads it in a log line or a refusal.
    pub fn display(&self) -> String {
        match (&self.email, &self.account_uuid) {
            (Some(email), _) => email.clone(),
            (None, Some(uuid)) => format!("account {uuid}"),
            (None, None) => "an unknown account".to_string(),
        }
    }
}

/// The outcome of comparing a resolved client identity to the control account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The client IS the control account.
    Match,
    /// The client is verifiably someone else.
    Mismatch(ClientIdentity),
    /// Nothing to compare on: the fetch failed, or neither side carries a
    /// field the other has. Never refused.
    Unknown,
}

/// Compare a resolved client identity to the control account. Pure.
///
/// Account uuid wins when both sides carry one — it is the identity key
/// everywhere else in this codebase (`crate::identity`). Otherwise the email
/// half of the control account's name is compared to the client's email, case
/// insensitively, but only when that name IS an email: a control account named
/// `work` with no uuid cannot be verified against anything and is `Unknown`,
/// never a spurious mismatch.
pub fn verdict(control: &ControlIdentity, client: &ClientIdentity) -> Verdict {
    if let (Some(c), Some(k)) = (
        control.account_uuid.as_deref(),
        client.account_uuid.as_deref(),
    ) {
        return if c == k {
            Verdict::Match
        } else {
            Verdict::Mismatch(client.clone())
        };
    }
    let control_email = crate::identity::email_of(&control.name);
    if !control_email.contains('@') {
        return Verdict::Unknown;
    }
    match client.email.as_deref() {
        Some(email) if email.eq_ignore_ascii_case(control_email) => Verdict::Match,
        Some(_) => Verdict::Mismatch(client.clone()),
        None => Verdict::Unknown,
    }
}

#[derive(Default)]
struct Entry {
    identity: OnceCell<ClientIdentity>,
    /// Set by the first request that logged this bearer's mismatch, so a
    /// client that fires ten bookkeeping calls at startup produces one line,
    /// not ten.
    warned: AtomicBool,
}

/// Resolved client identities, keyed by a SHA-256 of the bearer. The bearer
/// itself is never stored.
#[derive(Default)]
pub struct ClientIdentityCache {
    entries: Mutex<HashMap<[u8; 32], Arc<Entry>>>,
}

fn key_of(bearer: &str) -> [u8; 32] {
    Sha256::digest(bearer.as_bytes()).into()
}

impl ClientIdentityCache {
    pub fn new() -> Self {
        Self::default()
    }

    async fn entry(&self, bearer: &str) -> Arc<Entry> {
        let key = key_of(bearer);
        let mut entries = self.entries.lock().await;
        if entries.len() >= CACHE_CAP && !entries.contains_key(&key) {
            entries.clear();
        }
        entries.entry(key).or_default().clone()
    }

    /// The identity behind `bearer`, fetched from `profile_url` on first sight
    /// and cached afterwards. Concurrent first sights of one bearer share a
    /// single fetch (`OnceCell::get_or_try_init`). `None` — a fetch that failed
    /// or answered with no identity — is not cached, so the next request tries
    /// again.
    pub async fn resolve(&self, profile_url: &str, bearer: &str) -> Option<ClientIdentity> {
        let entry = self.entry(bearer).await;
        entry
            .identity
            .get_or_try_init(|| async {
                let profile = crate::oauth::fetch_profile_at(profile_url, bearer).await;
                let identity = ClientIdentity {
                    email: profile.email().map(str::to_string),
                    account_uuid: profile.account_uuid().map(str::to_string),
                };
                if identity.email.is_none() && identity.account_uuid.is_none() {
                    Err(())
                } else {
                    Ok(identity)
                }
            })
            .await
            .ok()
            .cloned()
    }

    /// `true` exactly once per bearer: the caller that gets it writes the log
    /// line, every later caller stays quiet.
    pub async fn first_warning(&self, bearer: &str) -> bool {
        let entry = self.entry(bearer).await;
        !entry.warned.swap(true, Ordering::Relaxed)
    }

    /// Distinct bearers currently cached (resolved or pending). Test hook.
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(name: &str, uuid: Option<&str>) -> ControlIdentity {
        ControlIdentity {
            name: name.to_string(),
            account_uuid: uuid.map(str::to_string),
        }
    }

    fn client(email: Option<&str>, uuid: Option<&str>) -> ClientIdentity {
        ClientIdentity {
            email: email.map(str::to_string),
            account_uuid: uuid.map(str::to_string),
        }
    }

    #[test]
    fn uuid_decides_when_both_sides_carry_one() {
        let c = control(
            "alice@example.com",
            Some("11111111-1111-1111-1111-111111111111"),
        );
        assert_eq!(
            verdict(
                &c,
                &client(
                    Some("bob@example.com"),
                    Some("11111111-1111-1111-1111-111111111111")
                )
            ),
            Verdict::Match,
            "a matching uuid wins over a differing email"
        );
        assert!(matches!(
            verdict(
                &c,
                &client(
                    Some("alice@example.com"),
                    Some("22222222-2222-2222-2222-222222222222")
                )
            ),
            Verdict::Mismatch(_)
        ));
    }

    #[test]
    fn email_decides_when_a_uuid_is_missing_and_ignores_case_and_org_suffix() {
        let c = control("Alice@Example.com/acme-corp", None);
        assert_eq!(
            verdict(&c, &client(Some("alice@example.com"), None)),
            Verdict::Match
        );
        assert_eq!(
            verdict(
                &c,
                &client(
                    Some("alice@example.com"),
                    Some("33333333-3333-3333-3333-333333333333")
                )
            ),
            Verdict::Match,
            "a client uuid with no control uuid to compare against falls back to email"
        );
        assert!(matches!(
            verdict(&c, &client(Some("bob@example.com"), None)),
            Verdict::Mismatch(_)
        ));
    }

    #[test]
    fn nothing_to_compare_on_is_unknown_never_a_mismatch() {
        assert_eq!(
            verdict(
                &control("work", None),
                &client(Some("bob@example.com"), None)
            ),
            Verdict::Unknown,
            "a non-email control name with no uuid cannot be verified"
        );
        assert_eq!(
            verdict(&control("alice@example.com", None), &client(None, None)),
            Verdict::Unknown
        );
        assert_eq!(
            verdict(
                &control(
                    "alice@example.com",
                    Some("11111111-1111-1111-1111-111111111111")
                ),
                &client(None, None)
            ),
            Verdict::Unknown
        );
    }

    #[test]
    fn display_prefers_email_then_uuid() {
        assert_eq!(
            client(Some("bob@example.com"), Some("u")).display(),
            "bob@example.com"
        );
        assert_eq!(client(None, Some("44444444")).display(), "account 44444444");
        assert_eq!(client(None, None).display(), "an unknown account");
    }

    #[tokio::test]
    async fn first_warning_fires_once_per_bearer() {
        let cache = ClientIdentityCache::new();
        assert!(cache.first_warning("tok-a").await);
        assert!(!cache.first_warning("tok-a").await);
        assert!(
            cache.first_warning("tok-b").await,
            "a different bearer warns on its own"
        );
    }

    #[tokio::test]
    async fn resolve_fetches_once_per_bearer_and_never_caches_a_failure() {
        use axum::{extract::State, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        #[derive(Clone)]
        struct Fake {
            hits: Arc<AtomicUsize>,
            fail_first: Arc<AtomicBool>,
        }
        async fn profile(State(f): State<Fake>) -> axum::response::Response {
            f.hits.fetch_add(1, Ordering::SeqCst);
            if f.fail_first.swap(false, Ordering::SeqCst) {
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Json(serde_json::json!({
                "account": {"uuid": "11111111-1111-1111-1111-111111111111", "email": "alice@example.com"},
                "organization": {"uuid": "22222222-2222-2222-2222-222222222222", "name": "acme-corp"}
            }))
            .into_response()
        }
        use axum::response::IntoResponse as _;

        let fake = Fake {
            hits: Arc::new(AtomicUsize::new(0)),
            fail_first: Arc::new(AtomicBool::new(true)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/oauth/profile", get(profile))
            .with_state(fake.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}/api/oauth/profile");

        let cache = ClientIdentityCache::new();
        assert_eq!(
            cache.resolve(&url, "tok-a").await,
            None,
            "a failed fetch resolves to nothing"
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);

        let expected = ClientIdentity {
            email: Some("alice@example.com".to_string()),
            account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        };
        assert_eq!(
            cache.resolve(&url, "tok-a").await.as_ref(),
            Some(&expected),
            "the failure was not cached: the retry fetched again and succeeded"
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 2);

        // Ten concurrent sightings of a SECOND bearer share one fetch.
        let cache = Arc::new(cache);
        let tasks: Vec<_> = (0..10)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let url = url.clone();
                tokio::spawn(async move { cache.resolve(&url, "tok-b").await })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap().as_ref(), Some(&expected));
        }
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            3,
            "one fetch for ten concurrent first sightings of one bearer"
        );
        assert_eq!(cache.len().await, 2);
    }
}
