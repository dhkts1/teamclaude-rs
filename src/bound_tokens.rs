//! Durable account-bound-token → minting account pins: the on-disk half of
//! `Manager::bound_tokens`.
//!
//! Anthropic's API hands back several kinds of state that only the account that produced them
//! can read again. Claude Code echoes every one of them in the conversation history on every
//! later turn, so serving one of those turns from another account fails that request:
//!
//! | what the history carries | what another account answers | what Claude Code does next |
//! |---|---|---|
//! | a `server_tool_use` id (`--advisor`) | `400 Advisor tool result content could not be processed` | strips the advisor blocks and retries (`retry:advisor-strip`) |
//! | a signed `thinking` block, or a `redacted_thinking` block's `data` | `400 thinking or redacted_thinking blocks in the latest assistant message cannot be modified` | strips every thinking block and retries (`retry:thinking-signature-strip`) |
//! | a `thread.previous_message_id` naming server-side thread state | `404 No thread state was found for the requested previous_message_id` (`thread_not_found`) | replays the full history as a new thread (`retry:tether-replay`) |
//!
//! Measured over 2026-09-20..22 on one fleet, 173k upstream calls: 8,322 of the first, 170 of
//! the second, 66 of the third — every cluster starting where a pinned conversation was
//! diverted onto another account (4,420 diverts in the same window). The third column was read
//! in the Claude Code 2.1.277..2.1.280 binaries on 2026-09-22: every rejection costs one extra
//! round trip and some context, never the turn. That is why the map holds a conversation to
//! its account only while that account can serve soon (`Manager::bound_account_holds`).
//!
//! This module is the file that carries the token → account map across a restart, which is
//! when a live conversation is most likely to be re-keyed. [`crate::manager::Manager`] owns the
//! live map; `src/proxy.rs` records from responses and enforces on requests.
//!
//! Three properties, each a way the naive version silently does the wrong thing:
//!
//! 1. **A token is stored HASHED, never verbatim.** A thinking signature is Anthropic's
//!    attestation over model output and a message id names a conversation; neither belongs in a
//!    cache file on disk. Only equality is ever needed, so [`hash_token`] (SHA-256, hex) is
//!    enough, and the file then carries nothing that can be replayed anywhere.
//! 2. **It stores an account IDENTITY, never the index.** The live map is keyed on positions
//!    into the account list; a position restored against a reordered list names a DIFFERENT
//!    account — and here that is worse than a cold cache, because it pins the conversation to
//!    the one account guaranteed to reject it. Anything that does not resolve to exactly one
//!    account is DROPPED. Identities are written ONCE into [`BoundTokenFile::accounts`] and
//!    referenced by slot, which is also what keeps the file small at [`BOUND_TOKEN_CAP`].
//! 3. **It is written atomically and can never take the proxy down.** Every read failure —
//!    missing, truncated, corrupt, wrong version — degrades to "no pins" plus a log line. A
//!    forgotten token is today's behaviour (the request routes normally); a panic is not.
//!
//! The file keeps its original name, `server-tool-pins.json` ([`FILE_NAME`]), because a live
//! proxy already writes one: the name is on the wire, the contents are not. Its FORMAT VERSION
//! is bumped instead, so a file written by the advisor-only build is ignored wholesale rather
//! than read as though its verbatim ids were hashes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::{Account, ConfigError};
use crate::identity::{self, Resolved};

/// How old a bound-token pin may be and still be honoured: **24 hours** since the response that
/// minted it.
///
/// Not a bet about a cache being warm — a token is unusable off its minting account forever, so
/// the "correct" TTL is the life of the conversation. The bound exists to keep the map from
/// growing without end. A Claude Code conversation idle for a day is over in every practical
/// sense, and the cost of being wrong is one rejected turn on a resumed conversation: exactly
/// the behaviour of a proxy without this file.
pub const BOUND_TOKEN_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Maximum tokens held in memory and written to disk.
///
/// 65,536, up from the advisor-only build's 4,096, because the token count per turn changed
/// shape: an advisor call is rare, while a signed `thinking` block comes back on nearly every
/// assistant turn — ~46k a day on this box. A cap below a day's mints would evict live
/// conversations while their TTL still had hours to run, which is the failure this map exists
/// to prevent. See the size measurement in this module's tests for what that costs on disk.
pub const BOUND_TOKEN_CAP: usize = 65_536;

/// File name, written beside the affinity pin file — see [`path_beside`]. Unchanged from the
/// advisor-only build on purpose: a live proxy already writes this path.
pub const FILE_NAME: &str = "server-tool-pins.json";

/// Bumped whenever the meaning of a field changes; a file written by a different version is
/// ignored wholesale rather than half-read. **2** since tokens are stored hashed, carry a
/// [`BoundTokenKind`], and reference their account by slot — a version-1 file's verbatim ids
/// would otherwise be compared against hashes and match nothing, silently.
const FORMAT_VERSION: u32 = 2;

/// What kind of account-bound state a token is. Diagnostic rather than behavioural — every kind
/// pins identically — so a log line and a file can say WHICH binding held a conversation, and a
/// future kind can be added without guessing what the untyped entries were.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundTokenKind {
    /// A `server_tool_use` block's id (`--advisor`; `web_search` is the next to add).
    #[serde(rename = "tool")]
    ServerToolUse,
    /// A `thinking` block's `signature`.
    #[serde(rename = "sig")]
    ThinkingSignature,
    /// A `redacted_thinking` block's `data`.
    #[serde(rename = "redacted")]
    RedactedThinking,
    /// A response `message.id` (`msg_…`), which a later request may name as
    /// `previous_message_id`.
    #[serde(rename = "msg")]
    MessageId,
}

impl BoundTokenKind {
    /// The word this kind is logged as.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerToolUse => "server-tool-id",
            Self::ThinkingSignature => "thinking-signature",
            Self::RedactedThinking => "redacted-thinking",
            Self::MessageId => "message-id",
        }
    }
}

/// The hash a token is keyed by, everywhere: SHA-256, lowercase hex.
///
/// The live map uses it too, not only the file. A signature is several hundred bytes and there
/// is one per assistant turn, so hashing at the door bounds the map's memory at 64 bytes a key
/// — and means a heap dump of this process no longer carries Anthropic's attestations either.
pub fn hash_token(token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(token.as_bytes());
    let out = digest.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for byte in out {
        use std::fmt::Write as _;
        // Infallible on a String; the result is only read to satisfy the lint.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// One persisted token: its hash, its kind, the slot of the account that minted it, and when.
///
/// Field names are short because there are up to [`BOUND_TOKEN_CAP`] of these in one file and
/// the key is already a 64-character hash — see the size measurement in the tests.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StoredBoundToken {
    /// [`hash_token`] of the token as it appears in the conversation.
    pub hash: String,
    pub kind: BoundTokenKind,
    /// Index into [`BoundTokenFile::accounts`] — NOT into the live account list.
    pub account: usize,
    /// Epoch ms of the response that minted the token.
    pub ms: i64,
}

/// One account identity, written once and referenced by slot from every token it minted.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredAccount {
    /// Display name — also the identity fallback for records with no `account_uuid`, exactly as
    /// [`identity::same_identity`] falls back.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
}

impl StoredAccount {
    /// The identity probe this record resolves through.
    fn probe(&self) -> Account {
        identity::probe(
            &self.name,
            self.account_uuid.clone(),
            self.org_uuid.clone(),
            self.org_name.clone(),
        )
    }
}

/// The file itself.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BoundTokenFile {
    pub version: u32,
    pub saved_at_ms: i64,
    /// Every account any token in this file was minted on, once each.
    pub accounts: Vec<StoredAccount>,
    pub tokens: Vec<StoredBoundToken>,
}

/// What a [`load`] made of the file — every field except `tokens` exists so the caller can say
/// out loud what it threw away.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Restored tokens, in the in-memory map's own shape: hash → (account index, kind, mint ms).
    pub tokens: HashMap<String, (usize, BoundTokenKind, i64)>,
    /// Dropped because they were minted longer ago than the TTL.
    pub expired: usize,
    /// Dropped because no live account carries that identity (removed, renamed), or because the
    /// entry named an account slot the file does not have.
    pub unresolved: usize,
    /// Dropped because two or more live accounts carry that identity. Refused, never guessed.
    pub ambiguous: usize,
    /// Set when the file was ignored ENTIRELY — unreadable, corrupt, truncated, or a version
    /// this build does not understand. `None` on a clean read and on "no file yet".
    pub degraded: Option<String>,
}

/// This file's path, given the session-affinity pin file's path: the same directory, a
/// different name.
///
/// Derived rather than resolved independently so the two caches can never land in different
/// places — and so an embedder that points `ServeOptions::affinity_path` at a disposable
/// directory (which the field's own doc tells it to do) gets this one redirected with it,
/// instead of quietly writing into the live proxy's cache dir.
pub fn path_beside(affinity_path: &Path) -> PathBuf {
    affinity_path.with_file_name(FILE_NAME)
}

/// Write `tokens` to `path` atomically at `0600`, keeping at most [`BOUND_TOKEN_CAP`] of them
/// (newest first). `tokens` pairs each record with the identity of the account that minted it;
/// identities are de-duplicated into the file's account table.
///
/// Serialized COMPACT, not pretty: at the cap this file is megabytes, and pretty-printing one
/// token per four lines roughly triples it for a file no one reads by eye. Returns how many
/// tokens landed. The caller decides what a failure means; it is never fatal here.
pub fn save(
    path: &Path,
    tokens: &[(String, BoundTokenKind, StoredAccount, i64)],
    now_ms: i64,
) -> Result<usize, ConfigError> {
    let mut tokens = tokens.to_vec();
    if tokens.len() > BOUND_TOKEN_CAP {
        tokens.sort_by_key(|(_, _, _, ms)| std::cmp::Reverse(*ms));
        tokens.truncate(BOUND_TOKEN_CAP);
    }
    // Stable order keeps the write byte-identical when nothing changed.
    tokens.sort_by(|a, b| a.0.cmp(&b.0));

    let mut accounts: Vec<StoredAccount> = Vec::new();
    let mut stored: Vec<StoredBoundToken> = Vec::with_capacity(tokens.len());
    for (hash, kind, account, ms) in tokens {
        let slot = match accounts.iter().position(|a| *a == account) {
            Some(slot) => slot,
            None => {
                accounts.push(account);
                accounts.len() - 1
            }
        };
        stored.push(StoredBoundToken {
            hash,
            kind,
            account: slot,
            ms,
        });
    }
    let count = stored.len();
    let file = BoundTokenFile {
        version: FORMAT_VERSION,
        saved_at_ms: now_ms,
        accounts,
        tokens: stored,
    };
    crate::config::write_atomic(path, &serde_json::to_string(&file)?)?;
    Ok(count)
}

/// Read `path` and resolve each token's account against `accounts`, dropping anything stale or
/// not resolvable to exactly one account.
///
/// Infallible by construction — every failure mode returns an empty map with `degraded` set.
pub fn load(path: &Path, accounts: &[Account], now_ms: i64, ttl_ms: i64) -> LoadReport {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // First boot, and the boot right after this feature ships. Not a degradation.
            return LoadReport::default();
        }
        Err(err) => {
            return LoadReport {
                degraded: Some(format!("unreadable: {err}")),
                ..LoadReport::default()
            };
        }
    };

    // The VERSION is read first, on its own, and a foreign one is reported as a version rather
    // than as corruption. Parsing the whole struct first gets this backwards: the advisor-only
    // build's file has neither `accounts` nor `tokens`, so it fails as "corrupt: missing field"
    // — the right refusal for exactly the wrong reason, and the reason is what an operator
    // reads when deciding whether their cache file is damaged or merely old.
    #[derive(Deserialize)]
    struct VersionPeek {
        version: u32,
    }
    match serde_json::from_str::<VersionPeek>(&data) {
        Ok(peek) if peek.version != FORMAT_VERSION => {
            return LoadReport {
                degraded: Some(format!(
                    "format version {} is not {FORMAT_VERSION}",
                    peek.version
                )),
                ..LoadReport::default()
            };
        }
        Ok(_) => {}
        Err(err) => {
            return LoadReport {
                degraded: Some(format!("corrupt: {err}")),
                ..LoadReport::default()
            };
        }
    }

    let file: BoundTokenFile = match serde_json::from_str(&data) {
        Ok(file) => file,
        Err(err) => {
            return LoadReport {
                degraded: Some(format!("corrupt: {err}")),
                ..LoadReport::default()
            };
        }
    };

    // Resolve each identity in the file's table ONCE, however many tokens reference it.
    let resolved: Vec<Resolved> = file
        .accounts
        .iter()
        .map(|stored| identity::resolve(accounts.iter().enumerate(), &stored.probe()))
        .collect();

    let mut report = LoadReport::default();
    for token in file.tokens {
        if now_ms.saturating_sub(token.ms) > ttl_ms {
            report.expired += 1;
            continue;
        }
        match resolved.get(token.account) {
            Some(Resolved::One(index)) => {
                report
                    .tokens
                    .insert(token.hash, (*index, token.kind, token.ms));
            }
            Some(Resolved::Many) => report.ambiguous += 1,
            // A slot the file does not have is as unresolvable as an account that is gone.
            Some(Resolved::None) | None => report.unresolved += 1,
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, uuid: &str) -> Account {
        identity::probe(
            name,
            Some(uuid.to_string()),
            Some("org-1".to_string()),
            None,
        )
    }

    fn stored_account(name: &str, uuid: &str) -> StoredAccount {
        StoredAccount {
            name: name.to_string(),
            account_uuid: Some(uuid.to_string()),
            org_uuid: Some("org-1".to_string()),
            org_name: None,
        }
    }

    fn entry(
        token: &str,
        kind: BoundTokenKind,
        account: StoredAccount,
        ms: i64,
    ) -> (String, BoundTokenKind, StoredAccount, i64) {
        (hash_token(token), kind, account, ms)
    }

    /// A unique path per test: the suite runs tests in parallel threads of ONE process, so a
    /// pid-only name collides between them.
    fn tmp(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tcr-bound-tokens-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(FILE_NAME)
    }

    /// The hash is the key everywhere, and the raw token must never be recoverable from what is
    /// written — the property that lets a thinking signature be remembered at all.
    #[test]
    fn a_token_is_written_hashed_and_never_verbatim() {
        let path = tmp("hashed");
        let now = 1_000_000;
        save(
            &path,
            &[entry(
                "sig_fake_1",
                BoundTokenKind::ThinkingSignature,
                stored_account("a@example.com", "uuid-a"),
                now,
            )],
            now,
        )
        .expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(
            !raw.contains("sig_fake_1"),
            "the signature itself must not be on disk: {raw}"
        );
        assert!(
            raw.contains(&hash_token("sig_fake_1")),
            "its hash is what keys the entry: {raw}"
        );
        assert_eq!(
            hash_token("sig_fake_1"),
            hash_token("sig_fake_1"),
            "and the hash is stable, or nothing ever matches"
        );
        assert_ne!(hash_token("sig_fake_1"), hash_token("sig_fake_2"));
    }

    /// Every kind round-trips, against an account list reordered between boots — a restore by
    /// POSITION would answer the wrong account for all of them.
    #[test]
    fn every_kind_resolves_back_to_its_account_by_identity() {
        let path = tmp("round-trip");
        let now = 1_000_000;
        let b = stored_account("b@example.com", "uuid-b");
        save(
            &path,
            &[
                entry(
                    "srvtoolu_fake_1",
                    BoundTokenKind::ServerToolUse,
                    b.clone(),
                    now,
                ),
                entry(
                    "sig_fake_1",
                    BoundTokenKind::ThinkingSignature,
                    b.clone(),
                    now,
                ),
                entry(
                    "data_fake_1",
                    BoundTokenKind::RedactedThinking,
                    b.clone(),
                    now,
                ),
                entry("msg_fake_1", BoundTokenKind::MessageId, b, now),
            ],
            now,
        )
        .expect("save");

        // Next boot lists b FIRST, so a restore by position would answer 1, not 0.
        let accounts = [
            account("b@example.com", "uuid-b"),
            account("a@example.com", "uuid-a"),
        ];
        let report = load(&path, &accounts, now, BOUND_TOKEN_TTL_MS);
        assert_eq!(report.degraded, None);
        for (token, kind) in [
            ("srvtoolu_fake_1", BoundTokenKind::ServerToolUse),
            ("sig_fake_1", BoundTokenKind::ThinkingSignature),
            ("data_fake_1", BoundTokenKind::RedactedThinking),
            ("msg_fake_1", BoundTokenKind::MessageId),
        ] {
            assert_eq!(
                report.tokens.get(&hash_token(token)).copied(),
                Some((0, kind, now)),
                "{token} must follow b's identity to position 0, keeping its kind"
            );
        }
    }

    /// One identity, four tokens, ONE account record in the file: the de-duplication that keeps
    /// the file from carrying 65,536 copies of the same email and uuids.
    #[test]
    fn identities_are_written_once_and_referenced_by_slot() {
        let path = tmp("dedupe");
        let now = 1_000_000;
        let a = stored_account("a@example.com", "uuid-a");
        save(
            &path,
            &[
                entry("t1", BoundTokenKind::MessageId, a.clone(), now),
                entry("t2", BoundTokenKind::MessageId, a.clone(), now),
                entry("t3", BoundTokenKind::MessageId, a.clone(), now),
                entry("t4", BoundTokenKind::MessageId, a, now),
            ],
            now,
        )
        .expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            raw.matches("a@example.com").count(),
            1,
            "the identity belongs in the account table, once: {raw}"
        );
    }

    #[test]
    fn a_token_past_the_ttl_is_dropped() {
        let path = tmp("expiry");
        let now = BOUND_TOKEN_TTL_MS * 3;
        let a = stored_account("a@example.com", "uuid-a");
        save(
            &path,
            &[
                entry("fresh", BoundTokenKind::MessageId, a.clone(), now - 60_000),
                entry(
                    "stale",
                    BoundTokenKind::MessageId,
                    a,
                    now - BOUND_TOKEN_TTL_MS - 60_000,
                ),
            ],
            now,
        )
        .expect("save");

        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            now,
            BOUND_TOKEN_TTL_MS,
        );
        assert_eq!(report.expired, 1);
        assert!(report.tokens.contains_key(&hash_token("fresh")));
        assert!(!report.tokens.contains_key(&hash_token("stale")));
    }

    /// An account that is gone takes its tokens with it. Restoring them onto whoever now holds
    /// that slot would pin those conversations to an account guaranteed to reject them.
    #[test]
    fn a_removed_account_drops_its_tokens_instead_of_mis_resolving() {
        let path = tmp("removed");
        let now = 1_000_000;
        save(
            &path,
            &[entry(
                "t1",
                BoundTokenKind::ThinkingSignature,
                stored_account("gone@example.com", "uuid-gone"),
                now,
            )],
            now,
        )
        .expect("save");

        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            now,
            BOUND_TOKEN_TTL_MS,
        );
        assert_eq!(report.unresolved, 1);
        assert!(report.tokens.is_empty());
    }

    #[test]
    fn a_corrupt_file_degrades_to_no_tokens_without_panicking() {
        let path = tmp("corrupt");
        std::fs::write(&path, "{\"version\":2,\"tokens\":[{\"hash\":").expect("write truncated");
        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            1_000_000,
            BOUND_TOKEN_TTL_MS,
        );
        assert!(report.degraded.is_some());
        assert!(report.tokens.is_empty());
    }

    /// The advisor-only build's file stored VERBATIM ids under a different schema. Reading it
    /// as though those ids were hashes would match nothing while reporting a clean restore, so
    /// the version bump must reject it out loud.
    #[test]
    fn a_version_one_file_is_refused_rather_than_read_as_hashes() {
        let path = tmp("v1");
        std::fs::write(
            &path,
            r#"{"version":1,"savedAtMs":1,"pins":[{"id":"srvtoolu_fake_1","name":"a@example.com","mintedAtMs":1}]}"#,
        )
        .expect("write a v1 file");
        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            1_000_000,
            BOUND_TOKEN_TTL_MS,
        );
        assert!(
            report
                .degraded
                .as_deref()
                .is_some_and(|reason| reason.contains("format version 1")),
            "the reason must name the version: {:?}",
            report.degraded
        );
        assert!(report.tokens.is_empty());
    }

    #[test]
    fn a_missing_file_is_not_a_degradation() {
        let path = tmp("missing").with_file_name("no-such-file.json");
        let report = load(&path, &[], 1_000_000, BOUND_TOKEN_TTL_MS);
        assert_eq!(report, LoadReport::default());
    }

    #[test]
    fn the_file_sits_beside_the_affinity_pins() {
        let affinity = Path::new("/tmp/example/session-affinity.json");
        assert_eq!(
            path_beside(affinity),
            PathBuf::from("/tmp/example/server-tool-pins.json")
        );
    }

    /// What a FULL map costs on disk, measured rather than estimated — the number the cap has
    /// to be judged against, since this file is rewritten whole on every flush.
    ///
    /// The assertion is a ceiling with room in it, not the measurement: the exact size is
    /// printed so a reader can see it move. If a change pushes it past the ceiling, that is a
    /// decision about the cap and the flush interval, not a number to quietly raise.
    #[test]
    fn a_full_map_is_measured_on_disk() {
        let path = tmp("at-cap");
        let now = 1_000_000;
        let accounts: Vec<StoredAccount> = (0..8)
            .map(|n| stored_account(&format!("account{n}@example.com"), &format!("uuid-{n}")))
            .collect();
        let tokens: Vec<(String, BoundTokenKind, StoredAccount, i64)> = (0..BOUND_TOKEN_CAP)
            .map(|n| {
                entry(
                    &format!("sig_fake_{n}"),
                    BoundTokenKind::ThinkingSignature,
                    accounts[n % accounts.len()].clone(),
                    now - n as i64,
                )
            })
            .collect();
        assert_eq!(save(&path, &tokens, now).expect("save"), BOUND_TOKEN_CAP);

        let bytes = std::fs::metadata(&path).expect("stat").len();
        println!("bound-token file at cap ({BOUND_TOKEN_CAP} tokens): {bytes} bytes");
        assert!(
            bytes < 8 * 1024 * 1024,
            "a full map is {bytes} bytes, past the 8 MB the cap was chosen against"
        );

        // And it still reads back whole.
        let live: Vec<Account> = (0..8)
            .map(|n| account(&format!("account{n}@example.com"), &format!("uuid-{n}")))
            .collect();
        let report = load(&path, &live, now, BOUND_TOKEN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(report.tokens.len(), BOUND_TOKEN_CAP);
    }

    /// Over the cap, the OLDEST mints are the ones that do not reach disk.
    #[test]
    fn saving_past_the_cap_keeps_the_newest() {
        let path = tmp("over-cap");
        let now = 1_000_000_000;
        let a = stored_account("a@example.com", "uuid-a");
        let tokens: Vec<(String, BoundTokenKind, StoredAccount, i64)> = (0..(BOUND_TOKEN_CAP + 2))
            .map(|n| {
                entry(
                    &format!("t{n}"),
                    BoundTokenKind::MessageId,
                    a.clone(),
                    now - (BOUND_TOKEN_CAP + 2 - n) as i64,
                )
            })
            .collect();
        assert_eq!(save(&path, &tokens, now).expect("save"), BOUND_TOKEN_CAP);
        let report = load(
            &path,
            &[account("a@example.com", "uuid-a")],
            now,
            BOUND_TOKEN_TTL_MS,
        );
        assert!(!report.tokens.contains_key(&hash_token("t0")));
        assert!(!report.tokens.contains_key(&hash_token("t1")));
        assert!(report
            .tokens
            .contains_key(&hash_token(&format!("t{}", BOUND_TOKEN_CAP + 1))));
    }
}
