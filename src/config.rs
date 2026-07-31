//! Drop-in config on `~/.config/teamclaude.json`.
//!
//! The file is the SAME one the JS proxy uses, so the structs mirror its
//! camelCase shape and stay tolerant of fields we do not model (`routes`, `sx`,
//! `quotaProbeSeconds`, `warmupSeconds`, …). Every struct carries a flattened
//! `extra` map so an unknown key survives a load→save round-trip untouched.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Monotonic counter making each [`save`] temp filename unique, so concurrent
/// saves never collide on one temp path (finding #6).
static SAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Typed config-layer errors: I/O vs malformed JSON stay distinguishable so a
/// caller can tell "no config yet" from "config is corrupt".
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

fn default_port() -> u16 {
    3456
}
fn default_upstream() -> String {
    "https://api.anthropic.com".to_string()
}
fn default_switch_threshold() -> f64 {
    0.95
}
/// Default pacing when the `pacing` key is absent: OFF — no in-flight cap, no
/// min-spacing, so an unconfigured proxy runs the no-pacing selection path.
///
/// A per-account concurrency cap trades prompt-cache locality for load spread:
/// every request it diverts lands on an account whose prefix is cold. On a
/// single-user proxy the cache is the scarce resource and the accounts are not,
/// so the trade is the wrong way round and the cap ships off. It stays a
/// supported knob — set `"pacing": {"maxInFlightPerAccount": N}` (and/or
/// `"minSpacingMs"`) to turn it back on, with exactly the behaviour it has today.
///
/// This is NOT covered by the global egress throttle ([`default_throttle`],
/// `src/manager/throttle.rs`): that is a RATE limiter (min-spacing + burst over
/// the aggregate send site), not a concurrency bound, and it is deliberately not
/// a substitute for one. Turning the cap off leaves per-account concurrency
/// genuinely unbounded.
fn default_pacing() -> PacingConfig {
    PacingConfig {
        max_in_flight_per_account: None,
        min_spacing_ms: None,
    }
}
/// Default global outbound throttle: ON. Absent `throttle` key → these
/// evidence-anchored starting values; `"throttle": {}` → off (escape hatch).
fn default_throttle() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(4),
    }
}
fn default_account_type() -> String {
    "oauth".to_string()
}

/// Proxy-level settings (`proxy` object in the file).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Any `proxy.*` keys we do not model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            api_key: None,
            extra: Map::new(),
        }
    }
}

/// One rotatable upstream account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: String,
    #[serde(rename = "type", default = "default_account_type")]
    pub account_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Epoch **milliseconds** at which `access_token` expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Any per-account keys we do not model (e.g. `models`, `upstream`, `sx`).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Per-account request pacing (opt-in; default OFF).
///
/// Both knobs are `Option`: absent in the config file → `None` → pacing is inert,
/// so an unconfigured proxy behaves byte-for-byte as before. When set, pacing can
/// only ever DELAY/SPREAD selection across the fleet — never turn a servable
/// request into a failure (the soft fallback in [`crate::manager::Manager::select`]).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PacingConfig {
    /// Cap on requests concurrently being served on one account. An account at or
    /// over the cap is temporarily skipped in selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight_per_account: Option<u32>,
    /// Minimum gap (ms) between two selects of the SAME account. An account
    /// selected less than this ago is temporarily skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spacing_ms: Option<u64>,
}

impl PacingConfig {
    /// The in-flight cap, treating a configured `0` as "disabled" (identical to
    /// leaving it unset). A literal `Some(0)` would make `in_flight >= 0` true for
    /// every account, holding out the ENTIRE fleet permanently — collapsing every
    /// request onto the least-loaded soft fallback and flooding the pacing log with
    /// a "skip in selection" line per account per request. Normalising it here keeps
    /// that footgun out of every read site.
    pub fn effective_max_in_flight(&self) -> Option<u32> {
        match self.max_in_flight_per_account {
            Some(0) => None,
            other => other,
        }
    }

    /// Whether either knob is configured. When `false`, pacing is fully inert and
    /// eligibility/selection are byte-identical to the no-pacing build. A cap of
    /// `0` counts as unset (see [`Self::effective_max_in_flight`]).
    pub fn is_active(&self) -> bool {
        self.effective_max_in_flight().is_some() || self.min_spacing_ms.is_some()
    }
}

/// Global (fleet-wide) outbound request-initiation throttle (opt-in; default OFF).
///
/// A GCRA token bucket over the SINGLE upstream send site: `burst` requests admit
/// instantly after idle, then one per `minSpacingMs`. Unlike [`PacingConfig`] (which
/// is PER-ACCOUNT and cannot damp a cross-account burst), this paces the AGGREGATE
/// egress that Anthropic's shared IP/client_id burst limiter actually keys on —
/// mirroring the probe path's `PROBE_SPACING`. Ships ON by default
/// ([`default_throttle`]): absent `throttle` key → `minSpacingMs: 350, burst: 4`;
/// `"throttle": {}` (empty object present) → all `None` → inert (escape hatch).
///
/// 350ms mirrors the σ5-proven probe-path aggregate rate (PROBE_SPACING); burst 4
/// covers a normal within-turn fan-out (main+haiku+quota) untaxed while staying far
/// below a ~15-20 cold-start fan-out so the throttle engages on the burst. Both are
/// evidence-anchored STARTING values, tunable live (docs/plans/throttle-live-sweep-runbook.md).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleConfig {
    /// Steady-state emission interval T (ms): after the burst budget is spent,
    /// at most one upstream send is initiated per this many ms across the WHOLE fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spacing_ms: Option<u64>,
    /// Bucket capacity B: how many sends may fire instantly after an idle period.
    /// Absent → treated as 1 (strict spacing). Keep it BELOW the cold fan-out size
    /// so the burst is actually paced, ABOVE the normal within-turn fan-out (~3) so
    /// interactive turns are never delayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
}

impl ThrottleConfig {
    /// Emission interval, treating `Some(0)` as unset (mirrors
    /// [`PacingConfig::effective_max_in_flight`]'s footgun normalization).
    pub fn effective_min_spacing(&self) -> Option<u64> {
        match self.min_spacing_ms {
            Some(0) => None,
            other => other,
        }
    }
    /// Bucket capacity, clamped to >= 1 (B=1 ⇒ strict min-spacing).
    pub fn effective_burst(&self) -> u32 {
        self.burst.unwrap_or(1).max(1)
    }
    /// Whether the throttle does anything. `min_spacing_ms` is the required knob —
    /// a burst without a spacing interval is meaningless. When false the throttle is
    /// fully inert (see [`crate::manager::Manager::throttle_send`]).
    pub fn is_active(&self) -> bool {
        self.effective_min_spacing().is_some()
    }
}

/// Top-level config document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default = "default_upstream")]
    pub upstream: String,
    #[serde(default = "default_switch_threshold")]
    pub switch_threshold: f64,
    /// Per-account request pacing. Absent in JSON → [`default_pacing`] → all knobs
    /// `None`, i.e. OFF: a per-account concurrency cap trades prompt-cache locality
    /// for load spread, and on a single-user proxy the cache is the scarce resource.
    /// Set `"pacing": {"maxInFlightPerAccount": N}` to opt back in. The global
    /// [`ThrottleConfig`] is a RATE limiter and is deliberately not a substitute for
    /// a concurrency bound.
    #[serde(default = "default_pacing")]
    pub pacing: PacingConfig,
    /// Global outbound throttle. Absent → [`default_throttle`] (ON:
    /// `minSpacingMs: 350, burst: 4`). Set `"throttle": {}` to disable (all knobs
    /// `None`), or override the knobs to tune the live rate (read at boot).
    #[serde(default = "default_throttle")]
    pub throttle: ThrottleConfig,
    /// Hard account lock: when set to an account `name`, ALL traffic is pinned to
    /// that one account — LRU rotation, session affinity, and load-balancing
    /// migration are ALL bypassed. Absent → normal routing (default). Tradeoff:
    /// a locked account has NO failover; if it is throttled/disabled/down, requests
    /// fail rather than rotating. Set to the exact `accounts[].name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_account: Option<String>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Any top-level keys we do not model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The default config path: `$HOME/.config/teamclaude.json`.
///
/// Deliberately NOT the platform config dir (`directories` would pick
/// `~/Library/Application Support` on macOS) — the JS proxy hard-codes
/// `~/.config`, and this binary is a drop-in for it.
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".config").join("teamclaude.json")
}

/// Load and parse the config at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let data = fs::read_to_string(path)?;
    let config = serde_json::from_str(&data)?;
    Ok(config)
}

/// Persist `config` to `path` atomically (temp file in the same dir + rename),
/// with `0600` permissions so refreshed tokens never land world-readable.
///
/// Same-directory temp + rename keeps the swap atomic (rename is atomic within a
/// filesystem); a crash mid-write leaves the original intact.
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    write_atomic(path, &serde_json::to_string_pretty(config)?)
}

/// The atomic 0600 write itself, shared by [`save`] and [`save_tokens`] so both
/// paths get the same durability and permission guarantees.
fn write_atomic(path: &Path, json: &str) -> Result<(), ConfigError> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));

    // Ensure the parent dir exists (a freshly-provisioned box may lack
    // `~/.config`), so a token-refresh save never fails with ENOENT and drops the
    // rotated refresh token. Fails loudly on a real perms error (finding #2).
    fs::create_dir_all(dir)?;

    let file_name = path.file_name().map_or_else(
        || "teamclaude.json".to_string(),
        |f| f.to_string_lossy().into_owned(),
    );
    // A per-call unique temp name: two concurrent saves (e.g. `probe_all`
    // refreshing several expired accounts at once) must not open and truncate the
    // SAME temp file and interleave into a corrupt write (finding #6).
    let seq = SAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp, path)?;
    // Re-assert perms after rename in case the destination pre-existed with a
    // looser mode (rename keeps the source inode, but be explicit).
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Persist ONLY the per-account credential state from `memory` into the file at
/// `path`, leaving every user-owned setting on disk exactly as the user left it.
///
/// The running server's `Config` is a BOOT-TIME snapshot: it goes stale the
/// moment the user edits the file, and only the credential fields
/// (`access_token` / `refresh_token` / `expires_at`) are ever mutated in memory
/// afterwards. Writing that whole snapshot back — which is what a plain [`save`]
/// does — therefore reverts every edit made while the proxy runs (observed live
/// 2026-07-25: a deleted `pacing` key was restored by the shutdown flush and read
/// back by the next boot, three restarts running). So persisting is a
/// read-modify-write: the FILE is the authority for everything except tokens.
///
/// Accounts are matched by identity ([`crate::identity::same_identity`], which
/// reduces to name equality for the current config shape) and never by index —
/// indices shift when the user adds or removes an account. Iterating the on-disk
/// list and pulling tokens IN gives the two removal semantics for free: an
/// account the user deleted from the file is not resurrected, and an account on
/// disk that the server never loaded is left untouched.
///
/// An unreadable file, a malformed one, or one carrying no usable `accounts`
/// list falls back to writing the in-memory config: a just-rotated refresh token
/// is single-use, so dropping it strands that account on `invalid_grant`
/// forever, which is strictly worse than overwriting a file whose account list
/// we cannot find anyway. Every fallback logs a warning naming the cause.
///
/// A credential that cannot be placed on a SPECIFIC entry — the entry is
/// malformed, or its identity matches nothing the server loaded — is never a
/// reason to fail the whole write: every other account's token still lands, and
/// [`merge_tokens`] hands back what it could not place so each miss is warned
/// about by name here. Silence was the old defect: the merge skipped, the write
/// succeeded, `Ok(())` came back, and the caller's error branch never ran.
///
/// The merge runs on the file's raw JSON document, NOT on a `Config` round-trip:
/// deserializing would materialize every serde default back into the file, so a
/// key the user just DELETED would reappear as its default (`"pacing": {}`) and a
/// key they never wrote would appear for the first time. Editing the parsed
/// document leaves the file byte-identical apart from the credential fields.
pub fn save_tokens(path: &Path, memory: &Config) -> Result<(), ConfigError> {
    let mut doc = match read_document(path) {
        Ok(doc) => doc,
        Err(ConfigError::Io(err)) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "config unreadable at persist time; falling back to writing the in-memory config"
            );
            return save(path, memory);
        }
        Err(ConfigError::Parse(err)) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "config on disk is malformed JSON at persist time; falling back to writing the in-memory config"
            );
            return save(path, memory);
        }
    };
    let report = match merge_tokens(&mut doc, memory) {
        Ok(report) => report,
        Err(reason) => {
            tracing::warn!(
                reason = %reason,
                path = %path.display(),
                "config on disk has no usable accounts list at persist time; falling back to writing the in-memory config so the rotated tokens are not lost"
            );
            return save(path, memory);
        }
    };
    report.warn_unpersisted(path);
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)
}

/// Read the config file as a raw JSON object. Parsing as a MAP (not a bare
/// `Value`) is deliberate: a file that is valid JSON but not an object — `[]`,
/// `null`, a half-written fragment — must take the malformed fallback rather
/// than be written back verbatim with the fresh tokens silently dropped.
fn read_document(path: &Path) -> Result<Map<String, Value>, ConfigError> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

/// The identity fields of an on-disk account entry — the only part of a stored
/// account this layer needs to read. Deliberately narrower than [`Account`]: an
/// entry the user is mid-edit on (no `accessToken` yet) still gets matched
/// rather than skipped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskIdentity {
    name: String,
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    org_uuid: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
}

/// The mutable credential triple, carrying the SAME serde renames as
/// [`Account`] so the merged keys are spelled exactly as the account struct
/// spells them — one source of truth for the wire names.
///
/// The `Option`s skip rather than clear: memory holds `None` only when the file
/// had no such field at boot, so an absent value means "nothing to say about
/// this key", never "delete what the user has since written there".
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Credentials<'a> {
    access_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

/// Why the on-disk `accounts` list could not be merged into AT ALL. Not one
/// account's problem but the whole document's, so [`save_tokens`] answers it with
/// the same whole-config fallback an unparseable file takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmergeable {
    /// No `accounts` key in the document.
    Missing,
    /// `accounts` is present but is not a JSON array.
    NotAnArray,
}

impl std::fmt::Display for Unmergeable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "the document has no accounts key",
            Self::NotAnArray => "the accounts key is not an array",
        })
    }
}

/// Why ONE on-disk account entry did not receive its rotated credentials. The
/// other entries in the same document are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The array element is not a JSON object (a string, a number, `null`).
    NotAnObject,
    /// The element is an object but carries no readable identity — no string
    /// `name`, which every match needs.
    NoIdentity,
    /// The entry is well-formed but no loaded account shares its identity. The
    /// signature of an account renamed on disk while the proxy was running,
    /// which is exactly the live-edit workflow [`save_tokens`] exists to support.
    NoMemoryMatch,
    /// The credential triple would not serialize. Structurally unreachable —
    /// reported rather than swallowed precisely because reaching it would mean an
    /// assumption this module rests on has broken.
    CredentialEncoding,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAnObject => "the entry is not a JSON object",
            Self::NoIdentity => "the entry has no readable account name",
            Self::NoMemoryMatch => "no loaded account has that identity (renamed on disk?)",
            Self::CredentialEncoding => "the credentials would not serialize",
        })
    }
}

/// One on-disk entry the merge could not write into. It keeps whatever
/// credential it already held — which, after a rotation, is a consumed one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedEntry {
    /// Position in the on-disk `accounts` array — the only handle the user has on
    /// an entry too malformed to carry a name.
    index: usize,
    /// The entry's `name` when one is readable, which it is for the common
    /// [`SkipReason::NoMemoryMatch`] case.
    name: Option<String>,
    reason: SkipReason,
}

impl SkippedEntry {
    /// How the entry is named in a log line: its `name`, else its position.
    fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("accounts[{}]", self.index))
    }
}

/// What a merge could not persist. Both lists are REPORTS, not failures: the
/// merge places every credential it can and hands the rest back, so
/// [`save_tokens`] logs each miss with the config path attached instead of the
/// helper logging blind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MergeReport {
    /// On-disk entries left holding whatever credential they already had.
    skipped: Vec<SkippedEntry>,
    /// Names of loaded accounts with no on-disk entry to write into. Benign when
    /// the user deleted the account; a token loss when they renamed it.
    absent_from_disk: Vec<String>,
}

impl MergeReport {
    /// Emit one line per credential that did NOT reach the file. A rotated
    /// refresh token is single-use, so a skip means that token is now consumed
    /// and unrecoverable — the account has to be re-authed. Nothing else in the
    /// stack can say this: the write itself succeeds and returns `Ok(())`.
    fn warn_unpersisted(&self, path: &Path) {
        for entry in &self.skipped {
            tracing::warn!(
                account = %entry.label(),
                index = entry.index,
                reason = %entry.reason,
                path = %path.display(),
                "rotated credential not persisted for this account; it may need `tcr login`"
            );
        }
        // A loaded account with no on-disk entry is USUALLY the user deleting it
        // from the file — correct, expected, and not worth a warning on every
        // persist for the rest of the process's life. Paired with an unmatched
        // on-disk entry in the same write it is instead the signature of a
        // RENAME, where a rotated credential really was dropped. The pairing is
        // what makes the two cases distinguishable, so only it escalates.
        let renamed = self
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::NoMemoryMatch);
        for name in &self.absent_from_disk {
            if renamed {
                tracing::warn!(
                    account = %name,
                    path = %path.display(),
                    "loaded account has no entry on disk while another entry matched nothing; a rename would drop its rotated credential, and it may need `tcr login`"
                );
            } else {
                tracing::debug!(
                    account = %name,
                    path = %path.display(),
                    "loaded account has no entry on disk; nothing persisted for it (removed from the file?)"
                );
            }
        }
    }
}

/// Overwrite the credential fields of every account present in BOTH `memory` and
/// the on-disk `doc`, matched by identity and never by position. Nothing else in
/// the document is touched.
///
/// Iterating the ON-DISK list and pulling tokens in gives both removal semantics
/// for free: an account the user deleted from the file is never resurrected (it
/// has no entry to write into), and an account on disk the server never loaded
/// is left alone (no memory match).
///
/// Every path that declines to write a credential is REPORTED, never silent: the
/// per-entry misses come back in the [`MergeReport`], and a document with no
/// usable `accounts` list comes back as [`Unmergeable`] so the caller can fall
/// back to writing the config whole rather than lose every rotated token in the
/// one write. A skipped entry is not an error for the others — the merge runs to
/// the end of the list either way.
fn merge_tokens(doc: &mut Map<String, Value>, memory: &Config) -> Result<MergeReport, Unmergeable> {
    let Some(accounts) = doc.get_mut("accounts") else {
        return Err(Unmergeable::Missing);
    };
    let Some(accounts) = accounts.as_array_mut() else {
        return Err(Unmergeable::NotAnArray);
    };

    let mut report = MergeReport::default();
    // Which loaded accounts found a home, so the ones that did not can be named
    // afterwards — a rename shows up here as well as in `skipped`.
    let mut placed = vec![false; memory.accounts.len()];

    for (index, entry) in accounts.iter_mut().enumerate() {
        let Some(object) = entry.as_object_mut() else {
            report.skipped.push(SkippedEntry {
                index,
                name: None,
                reason: SkipReason::NotAnObject,
            });
            continue;
        };
        // Read the name BEFORE the identity parse, so a half-edited entry that
        // fails to deserialize can still be named in the warning.
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Ok(stored) = serde_json::from_value::<DiskIdentity>(Value::Object(object.clone()))
        else {
            report.skipped.push(SkippedEntry {
                index,
                name,
                reason: SkipReason::NoIdentity,
            });
            continue;
        };
        let probe = crate::identity::probe(
            &stored.name,
            stored.account_uuid,
            stored.org_uuid,
            stored.org_name,
        );
        let Some((position, fresh)) = memory
            .accounts
            .iter()
            .enumerate()
            .find(|(_, a)| crate::identity::same_identity(a, &probe))
        else {
            report.skipped.push(SkippedEntry {
                index,
                name,
                reason: SkipReason::NoMemoryMatch,
            });
            continue;
        };
        let credentials = Credentials {
            access_token: &fresh.access_token,
            refresh_token: fresh.refresh_token.as_deref(),
            expires_at: fresh.expires_at,
        };
        let Ok(Value::Object(fields)) = serde_json::to_value(&credentials) else {
            report.skipped.push(SkippedEntry {
                index,
                name,
                reason: SkipReason::CredentialEncoding,
            });
            continue;
        };
        object.extend(fields);
        if let Some(seen) = placed.get_mut(position) {
            *seen = true;
        }
    }

    report.absent_from_disk = memory
        .accounts
        .iter()
        .zip(&placed)
        .filter(|(_, seen)| !**seen)
        .map(|(account, _)| account.name.clone())
        .collect();
    Ok(report)
}

/// What a targeted [`save_disabled`] did to the on-disk document. Reported
/// rather than swallowed so the caller can say, in one line, whether a benched
/// account will actually still be benched after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledWrite {
    /// The flag was set (or dropped) on the matching entry and the file rewritten.
    Updated,
    /// The document already said exactly this, so nothing was written and the
    /// file is byte-identical. A file holding single-use refresh tokens is
    /// rewritten only when it must be.
    Unchanged,
    /// Nothing on disk carries that identity — the entry was deleted or renamed
    /// while the proxy ran — or the document has no usable `accounts` array.
    /// Nothing was written, so the flag will NOT survive a restart.
    NoEntry,
    /// MORE than one entry carries that identity, so no entry can be chosen
    /// without guessing. Nothing was written.
    ///
    /// [`crate::identity::same_identity`] falls back to name equality when either
    /// side lacks a uuid and treats an unknown org as a match, so two entries
    /// sharing a name both match either runtime row. The caller selects by ROW
    /// INDEX (the TUI does), so silently taking the first match lands the flag on
    /// whichever entry happens to be earlier — benching a healthy account and
    /// returning an exhausted one to rotation, with the TUI showing the opposite.
    /// Refusing is the same posture the CLI takes on an ambiguous query.
    Ambiguous,
}

impl std::fmt::Display for DisabledWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::NoEntry => "no matching entry on disk",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY the `disabled` flag of the one account matching `target`'s
/// identity into the file at `path`, leaving every other key — and every other
/// account — exactly as the user left it.
///
/// Same read-modify-write shape as [`save_tokens`], and for the same reason: the
/// running server's `Config` is a boot-time snapshot, so writing it whole (what
/// [`save`] does) reverts every setting the user edited while the proxy ran. The
/// edit therefore runs on the file's RAW JSON document and never on a `Config`
/// round-trip, so a key the user just deleted cannot reappear as its serde
/// default.
///
/// `disabled == false` REMOVES the key rather than writing `false` — matching
/// the CLI contract pinned by `cli::tests::set_enabled_false_drops_the_disabled_key`
/// and the JS `delete account.disabled` it was ported from. A stale `false`
/// already on disk is dropped for the same reason.
///
/// Unlike [`save_tokens`] there is deliberately NO whole-config fallback when the
/// file is unreadable or malformed. A rotated refresh token is single-use, so
/// losing one is unrecoverable and worth the clobber risk; a lost `disabled` flag
/// costs one un-benched account and is fixed by pressing `d` again. Writing a
/// whole boot-time snapshot over a file we could not even parse is the exact
/// clobber this module exists to prevent, so the error comes back instead.
///
/// An identity matching MORE than one entry writes nothing and reports
/// [`DisabledWrite::Ambiguous`] — see [`find_account_entry`] for why guessing the
/// first match is worse than refusing.
pub fn save_disabled(
    path: &Path,
    target: &Account,
    disabled: bool,
) -> Result<DisabledWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_disabled(&mut doc, target, disabled);
    if outcome == DisabledWrite::Updated {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

/// Set or remove `disabled` on the one entry in `doc` matching `target`. Reports
/// whether the document actually changed, so the caller can skip a pointless
/// rewrite of a credential file.
fn merge_disabled(doc: &mut Map<String, Value>, target: &Account, disabled: bool) -> DisabledWrite {
    let entry = match find_account_entry(doc, target) {
        Ok(entry) => entry,
        // No entry, or too many to choose between — either way nothing is written.
        Err(refusal) => return refusal,
    };
    // `true` writes the key; `false` DROPS it (never a `false` literal).
    let desired = disabled.then_some(Value::Bool(true));
    if entry.get("disabled").cloned() == desired {
        return DisabledWrite::Unchanged;
    }
    match desired {
        Some(value) => entry.insert("disabled".to_string(), value),
        None => entry.remove("disabled"),
    };
    DisabledWrite::Updated
}

/// Where the ONE `accounts` entry carrying `target`'s identity lives — or why
/// there is no one entry to write into. Separate from [`find_account_entry`] so
/// the whole array can be scanned immutably (counting matches) before any mutable
/// borrow is taken; a single mutable pass cannot both count and hand back a
/// reference.
enum EntryMatch {
    /// Exactly one entry matches, at this index of the `accounts` array.
    One(usize),
    /// No usable `accounts` array, or nothing in it carries that identity.
    None,
    /// Two or more entries match.
    Many,
}

fn locate_account_entry(doc: &Map<String, Value>, target: &Account) -> EntryMatch {
    let Some(entries) = doc.get("accounts").and_then(Value::as_array) else {
        return EntryMatch::None;
    };
    let mut found: Option<usize> = None;
    for (index, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let Ok(stored) = serde_json::from_value::<DiskIdentity>(Value::Object(object.clone()))
        else {
            continue;
        };
        let probe = crate::identity::probe(
            &stored.name,
            stored.account_uuid,
            stored.org_uuid,
            stored.org_name,
        );
        if crate::identity::same_identity(target, &probe) {
            if found.is_some() {
                return EntryMatch::Many;
            }
            found = Some(index);
        }
    }
    match found {
        Some(index) => EntryMatch::One(index),
        None => EntryMatch::None,
    }
}

/// The on-disk `accounts` entry whose identity matches `target`, or the
/// [`DisabledWrite`] refusal explaining why there is no single entry to write.
///
/// Matching reuses the [`DiskIdentity`] probe + [`crate::identity::same_identity`]
/// pairing that [`merge_tokens`] uses, so a rotated credential and a disabled flag
/// can never land on two different entries.
///
/// An AMBIGUOUS identity is refused, not resolved to the first match. The old
/// first-match-wins rested on "the CLI already refuses an ambiguous query", but
/// the TUI is a caller that never goes through that check — it selects by row
/// index — and `config::load` validates no uniqueness, so nothing upstream makes
/// the identity unique. `same_identity` falls back to name equality when either
/// side lacks a uuid, so two entries sharing a name match either runtime row: the
/// flag would land on whichever is earlier in the file, benching a healthy account
/// while the TUI shows the other one disabled.
fn find_account_entry<'a>(
    doc: &'a mut Map<String, Value>,
    target: &Account,
) -> Result<&'a mut Map<String, Value>, DisabledWrite> {
    let index = match locate_account_entry(doc, target) {
        EntryMatch::One(index) => index,
        EntryMatch::None => return Err(DisabledWrite::NoEntry),
        EntryMatch::Many => return Err(DisabledWrite::Ambiguous),
    };
    // The immutable scan above proved this path resolves; `NoEntry` here is the
    // total-function tail, not a reachable outcome.
    doc.get_mut("accounts")
        .and_then(Value::as_array_mut)
        .and_then(|entries| entries.get_mut(index))
        .and_then(Value::as_object_mut)
        .ok_or(DisabledWrite::NoEntry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"{
      "proxy": { "port": 3456, "apiKey": "sk-proxy-secret", "customFlag": true },
      "upstream": "https://api.anthropic.com",
      "switchThreshold": 0.9,
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        {
          "name": "acct-a",
          "type": "oauth",
          "accountUuid": "uuid-a",
          "orgName": "Org A",
          "accessToken": "at-a",
          "refreshToken": "rt-a",
          "expiresAt": 1893456000000,
          "priority": 0,
          "models": ["claude-fable-5"]
        }
      ]
    }"#;

    #[test]
    fn load_parses_known_fields() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(config.proxy.port, 3456);
        assert_eq!(config.proxy.api_key.as_deref(), Some("sk-proxy-secret"));
        assert_eq!(config.switch_threshold, 0.9);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "acct-a");
        assert_eq!(config.accounts[0].expires_at, Some(1893456000000));
    }

    #[test]
    fn save_round_trip_preserves_unknown_fields() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-{}.json", std::process::id()));
        save(&tmp, &config).unwrap();
        let reloaded = fs::read_to_string(&tmp).unwrap();
        let value: Value = serde_json::from_str(&reloaded).unwrap();

        // Unmodelled top-level keys survive.
        assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
        assert!(value["routes"].is_array());
        // Unmodelled proxy key survives.
        assert_eq!(value["proxy"]["customFlag"], serde_json::json!(true));
        // Unmodelled per-account key survives.
        assert_eq!(
            value["accounts"][0]["models"],
            serde_json::json!(["claude-fable-5"])
        );

        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn save_writes_owner_only_permissions() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-perm-{}.json", std::process::id()));
        save(&tmp, &config).unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn defaults_apply_to_minimal_config() {
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert_eq!(config.proxy.port, 3456);
        assert_eq!(config.upstream, "https://api.anthropic.com");
        assert_eq!(config.switch_threshold, 0.95);
        // Absent `pacing` key → pacing OFF: no in-flight cap, no min-spacing.
        assert_eq!(config.pacing.max_in_flight_per_account, None);
        assert_eq!(config.pacing.min_spacing_ms, None);
        assert!(!config.pacing.is_active());
    }

    #[test]
    fn default_pacing_ships_off() {
        // Guards the DEFAULT itself, not just deserialization: a per-account
        // concurrency cap costs prompt-cache locality, so it must stay opt-in.
        // If this flips, pacing was silently turned back on for every user.
        let pacing = default_pacing();
        assert_eq!(pacing.max_in_flight_per_account, None);
        assert_eq!(pacing.min_spacing_ms, None);
        assert_eq!(pacing.effective_max_in_flight(), None);
        assert!(!pacing.is_active());
    }

    #[test]
    fn empty_pacing_object_disables_pacing() {
        // `"pacing": {}` spells out what the default already is: both knobs
        // None → inert. Kept so a config that writes the key stays supported.
        let config: Config = serde_json::from_str(r#"{ "accounts": [], "pacing": {} }"#).unwrap();
        assert_eq!(config.pacing.max_in_flight_per_account, None);
        assert_eq!(config.pacing.min_spacing_ms, None);
        assert!(!config.pacing.is_active());
    }

    #[test]
    fn explicit_pacing_overrides_the_default() {
        let config: Config = serde_json::from_str(
            r#"{ "accounts": [], "pacing": { "maxInFlightPerAccount": 5, "minSpacingMs": 200 } }"#,
        )
        .unwrap();
        assert_eq!(config.pacing.max_in_flight_per_account, Some(5));
        assert_eq!(config.pacing.min_spacing_ms, Some(200));
    }

    #[test]
    fn absent_throttle_defaults_on() {
        // No `throttle` key → default_throttle → ON with evidence-anchored knobs.
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert!(config.throttle.is_active());
        assert_eq!(config.throttle.effective_min_spacing(), Some(350));
        assert_eq!(config.throttle.effective_burst(), 4);
    }

    #[test]
    fn empty_throttle_object_disables_throttle() {
        // `"throttle": {}` is the escape hatch: empty object → both knobs None → inert.
        let config: Config = serde_json::from_str(r#"{ "accounts": [], "throttle": {} }"#).unwrap();
        assert_eq!(config.throttle.min_spacing_ms, None);
        assert_eq!(config.throttle.burst, None);
        assert!(!config.throttle.is_active());
    }

    #[test]
    fn explicit_throttle_enables() {
        let config: Config = serde_json::from_str(
            r#"{ "accounts": [], "throttle": { "minSpacingMs": 350, "burst": 5 } }"#,
        )
        .unwrap();
        assert!(config.throttle.is_active());
        assert_eq!(config.throttle.effective_min_spacing(), Some(350));
        assert_eq!(config.throttle.effective_burst(), 5);
    }

    #[test]
    fn lock_account_parses_when_present() {
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "lockAccount": "acme" }"#).unwrap();
        assert_eq!(config.lock_account, Some("acme".to_string()));
    }

    #[test]
    fn lock_account_absent_defaults_none() {
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert_eq!(config.lock_account, None);
    }

    /// A unique temp path per test — the suite runs tests in parallel threads of
    /// ONE process, so a pid-only name collides.
    fn tmp_path(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tcr-{tag}-{}-{seq}.json", std::process::id()))
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read persisted config"))
            .expect("persisted config is valid JSON")
    }

    /// One account, tokens as given, written as the file the server booted from.
    fn one_account_file(access: &str, refresh: &str, expires: i64) -> String {
        format!(
            r#"{{ "accounts": [ {{ "name": "acct-a", "accessToken": "{access}", "refreshToken": "{refresh}", "expiresAt": {expires} }} ] }}"#
        )
    }

    /// THE regression guard. Reproduces what happened live on 2026-07-25: the
    /// server booted with `pacing.maxInFlightPerAccount = 3`, the user deleted
    /// the key while it ran, and the next persist stamped the boot-time snapshot
    /// back over the file — so the deleted setting returned and the next boot
    /// read it. Persisting must write the rotated tokens and NOTHING else.
    #[test]
    fn persist_does_not_clobber_a_user_edit() {
        let path = tmp_path("persist-user-edit");
        fs::write(
            &path,
            r#"{ "pacing": { "maxInFlightPerAccount": 3 },
                 "accounts": [ { "name": "acct-a", "accessToken": "at-old", "refreshToken": "rt-old", "expiresAt": 1 } ] }"#,
        )
        .unwrap();

        // The server's boot-time snapshot still carries the setting…
        let mut memory = load(&path).unwrap();
        assert_eq!(memory.pacing.max_in_flight_per_account, Some(3));
        // …the user deletes it while the proxy runs…
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        // …and a token rotates, triggering a persist.
        memory.accounts[0].access_token = "at-new".to_string();
        memory.accounts[0].refresh_token = Some("rt-new".to_string());
        memory.accounts[0].expires_at = Some(2);
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert!(
            value.get("pacing").is_none(),
            "the server restored a key the user deleted: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(2));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_preserves_unknown_top_level_keys() {
        let path = tmp_path("persist-unknown");
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        let mut memory = load(&path).unwrap();

        // The user adds keys the server does not model — including one it has
        // never seen — then a token rotates.
        fs::write(
            &path,
            r#"{ "quotaProbeSeconds": 120,
                 "routes": [{ "name": "r1", "match": "*fable*" }],
                 "accounts": [ { "name": "acct-a", "accessToken": "at-old", "refreshToken": "rt-old", "expiresAt": 1, "models": ["claude-fable-5"] } ] }"#,
        )
        .unwrap();
        memory.accounts[0].access_token = "at-new".to_string();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["quotaProbeSeconds"], json!(120));
        assert!(value["routes"].is_array());
        assert_eq!(value["accounts"][0]["models"], json!(["claude-fable-5"]));
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_falls_back_when_file_is_unreadable() {
        let path = tmp_path("persist-missing");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // No file at all (deleted under the running server, or a first-boot path
        // that has yet to create it): the rotated tokens must still land.
        assert!(!path.exists());
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(7));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_falls_back_when_file_is_malformed() {
        let path = tmp_path("persist-malformed");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // A single-use refresh token is worth more than an unparseable file.
        fs::write(&path, "{ this is not json").unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][0]["refreshToken"],
            json!("rt-new")
        );

        // Valid JSON that is not an object takes the same path.
        fs::write(&path, "[]").unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][0]["refreshToken"],
            json!("rt-new")
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_matches_accounts_by_name_not_index() {
        let path = tmp_path("persist-by-name");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 },
                   { "name": "acct-b", "accessToken": "at-b-new", "refreshToken": "rt-b-new", "expiresAt": 22 } ] }"#,
        )
        .unwrap();
        // The user reorders the accounts on disk while the proxy runs. Index 0 in
        // memory is acct-a; index 0 on disk is now acct-b.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-b", "accessToken": "at-b-old" },
                   { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["name"], json!("acct-b"));
        assert_eq!(
            value["accounts"][0]["accessToken"],
            json!("at-b-new"),
            "tokens landed by position, not identity"
        );
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-b-new"));
        assert_eq!(value["accounts"][1]["name"], json!("acct-a"));
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-a-new"));
        assert_eq!(value["accounts"][1]["expiresAt"], json!(11));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_does_not_resurrect_an_account_the_user_removed() {
        let path = tmp_path("persist-removed");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a" },
                   { "name": "acct-gone", "accessToken": "at-gone" } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        let accounts = value["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1, "a removed account came back: {value}");
        assert_eq!(accounts[0]["name"], json!("acct-a"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_leaves_an_account_the_server_never_loaded_untouched() {
        let path = tmp_path("persist-added");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-new" } ] }"#,
        )
        .unwrap();
        // The user added a second account by hand after boot.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-old" },
                   { "name": "acct-new", "accessToken": "at-new", "refreshToken": "rt-new" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-a-new"));
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][1]["refreshToken"], json!("rt-new"));
        fs::remove_file(&path).ok();
    }

    /// A document that parses but carries no `accounts` list at all — a
    /// truncated hand-edit, or a file another tool rewrote. The merge used to
    /// return before writing ANYTHING while `save_tokens` still reported `Ok`, so
    /// one write consumed and dropped the freshly rotated refresh token of every
    /// account at once. It is a malformed document, and takes the same
    /// whole-config fallback an unparseable one does.
    #[test]
    fn merge_with_missing_accounts_array_falls_back_and_warns() {
        let path = tmp_path("persist-no-accounts-key");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        let malformed = r#"{ "proxy": { "port": 3456 } }"#;
        fs::write(&path, malformed).unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(malformed).unwrap();
        assert_eq!(
            merge_tokens(&mut doc, &memory),
            Err(Unmergeable::Missing),
            "the caller must be told, not handed a silently untouched document"
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-new"),
            "a single-use rotated token was dropped: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(7));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the fallback must not loosen permissions on a token file"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn merge_with_non_array_accounts_falls_back() {
        let path = tmp_path("persist-accounts-not-array");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // `accounts` present but the wrong JSON type: nothing to merge into, and
        // writing the document back verbatim would drop the rotated token.
        let malformed = r#"{ "accounts": {} }"#;
        fs::write(&path, malformed).unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(malformed).unwrap();
        assert_eq!(
            merge_tokens(&mut doc, &memory),
            Err(Unmergeable::NotAnArray)
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        fs::remove_file(&path).ok();
    }

    /// The live-edit workflow `save_tokens` exists to support, in its one losing
    /// shape: renaming an account leaves its on-disk entry matching nothing, so
    /// its rotated credential cannot be placed. That is unavoidable — being
    /// silent about it is not. Both halves of the rename must be reported, and
    /// the OTHER account's token must still land.
    #[test]
    fn renamed_account_is_reported_not_silently_skipped() {
        let path = tmp_path("persist-renamed");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 },
                   { "name": "acct-b", "accessToken": "at-b-new", "refreshToken": "rt-b-new", "expiresAt": 22 } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a-renamed", "accessToken": "at-a-old", "refreshToken": "rt-a-old" },
                   { "name": "acct-b", "accessToken": "at-b-old", "refreshToken": "rt-b-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory).expect("the document has an accounts array");
        assert_eq!(
            report.skipped,
            vec![SkippedEntry {
                index: 0,
                name: Some("acct-a-renamed".to_string()),
                reason: SkipReason::NoMemoryMatch,
            }],
            "the unmatched on-disk entry must come back named"
        );
        assert_eq!(
            report.absent_from_disk,
            vec!["acct-a".to_string()],
            "the memory side of the rename must be visible too — that is what makes it a rename and not a deletion"
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-a-old"),
            "a token landed on an entry that is not its own: {value}"
        );
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-b-new"));
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-b-new"),
            "one skipped entry must not cost the other accounts their tokens"
        );
        assert_eq!(value["accounts"][1]["expiresAt"], json!(22));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn undeserializable_entry_does_not_block_its_siblings() {
        let path = tmp_path("persist-junk-entry");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 33 } ] }"#,
        )
        .unwrap();
        // Two entries a mid-edit file can hold — a bare string and an object with
        // no name — ahead of the real account.
        fs::write(
            &path,
            r#"{ "accounts": [
                   "acct-a",
                   { "accessToken": "at-orphan" },
                   { "name": "acct-a", "accessToken": "at-a-old", "refreshToken": "rt-a-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory).expect("the document has an accounts array");
        assert_eq!(
            report.skipped,
            vec![
                SkippedEntry {
                    index: 0,
                    name: None,
                    reason: SkipReason::NotAnObject,
                },
                SkippedEntry {
                    index: 1,
                    name: None,
                    reason: SkipReason::NoIdentity,
                },
            ]
        );
        assert!(
            report.absent_from_disk.is_empty(),
            "the loaded account did find its entry"
        );
        // A nameless entry is still addressable by the user: they can count rows.
        assert_eq!(report.skipped[0].label(), "accounts[0]");

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0],
            json!("acct-a"),
            "a malformed entry was rewritten instead of left alone"
        );
        assert_eq!(value["accounts"][1], json!({ "accessToken": "at-orphan" }));
        assert_eq!(value["accounts"][2]["accessToken"], json!("at-a-new"));
        assert_eq!(
            value["accounts"][2]["refreshToken"],
            json!("rt-a-new"),
            "junk ahead of a good entry blocked its rotated token: {value}"
        );
        assert_eq!(value["accounts"][2]["expiresAt"], json!(33));
        fs::remove_file(&path).ok();
    }

    /// The benign twin of the rename: an account deleted from the file is
    /// reported as absent, with NO unmatched on-disk entry beside it. That
    /// pairing is the only thing distinguishing a correct deletion from a
    /// rename that just cost an account its refresh token.
    #[test]
    fn account_removed_from_disk_is_reported_without_a_skip() {
        let path = tmp_path("persist-removed-report");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new" },
                   { "name": "acct-gone", "accessToken": "at-gone-new" } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory).expect("the document has an accounts array");
        assert!(
            report.skipped.is_empty(),
            "a deletion leaves no unmatched on-disk entry: {report:?}"
        );
        assert_eq!(report.absent_from_disk, vec!["acct-gone".to_string()]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn save_tokens_writes_owner_only_permissions() {
        let path = tmp_path("persist-perm");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // Both paths through save_tokens must land 0600: the merge…
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // …and the fallback.
        fs::remove_file(&path).unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).ok();
    }

    // --- save_disabled (the TUI's `d`/`e`, made durable) -------------------

    /// A file carrying keys the server does not model, plus a second account, so
    /// a targeted flag write can be checked for collateral damage.
    const DISABLE_SAMPLE: &str = r#"{
      "warmupSeconds": 900,
      "pacing": { "maxInFlightPerAccount": 3 },
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "acct-a", "type": "oauth", "accessToken": "at-a",
          "refreshToken": "rt-a", "expiresAt": 1, "models": ["claude-fable-5"] },
        { "name": "acct-b", "type": "oauth", "accessToken": "at-b",
          "refreshToken": "rt-b", "expiresAt": 2 }
      ]
    }"#;

    /// An identity probe in the legacy (no-uuid) shape every real config uses,
    /// where `same_identity` reduces to name equality.
    fn by_name(name: &str) -> Account {
        crate::identity::probe(name, None, None, None)
    }

    /// Disabling writes `"disabled": true` onto the right entry and changes
    /// NOTHING else — the whole point of editing the raw document instead of
    /// round-tripping a boot-time `Config`. Strip the one key we asked for and
    /// the document must be identical to what the user had.
    #[test]
    fn disable_writes_the_flag_and_changes_nothing_else() {
        let path = tmp_path("disable-write");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), true).unwrap(),
            DisabledWrite::Updated
        );

        let mut after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        after["accounts"][0]
            .as_object_mut()
            .expect("the entry is an object")
            .remove("disabled");
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "the write touched something other than the disabled flag"
        );
        fs::remove_file(&path).ok();
    }

    /// Re-enabling DROPS the key rather than writing `false`, matching the CLI
    /// contract pinned by `cli::tests::set_enabled_false_drops_the_disabled_key`.
    /// A full disable→enable round trip must leave the file as it started.
    #[test]
    fn re_enable_drops_the_key_and_round_trips_the_document() {
        let path = tmp_path("disable-roundtrip");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "re-enable must DROP the disabled key, not write false: {after}"
        );
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "a disable→enable round trip must leave the document as it started"
        );
        fs::remove_file(&path).ok();
    }

    /// The unrelated account and the unmodelled keys (`warmupSeconds`, `pacing`,
    /// `routes`, per-account `models`) survive the write untouched. Named
    /// separately from the equality check above so weakening that one still trips
    /// a gate on the preserve-unknown-keys guarantee.
    #[test]
    fn disable_leaves_other_accounts_and_unmodelled_keys_untouched() {
        let path = tmp_path("disable-collateral");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();

        let after = read_json(&path);
        assert_eq!(after["warmupSeconds"], json!(900));
        assert_eq!(after["pacing"]["maxInFlightPerAccount"], json!(3));
        assert!(after["routes"].is_array());
        assert_eq!(after["accounts"][0]["models"], json!(["claude-fable-5"]));
        // The account we did NOT name keeps its credentials and gains no flag.
        assert_eq!(after["accounts"][1]["name"], json!("acct-b"));
        assert_eq!(after["accounts"][1]["accessToken"], json!("at-b"));
        assert_eq!(after["accounts"][1]["refreshToken"], json!("rt-b"));
        assert!(
            after["accounts"][1].get("disabled").is_none(),
            "the unrelated account was flagged too: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// A redundant write — the document already says exactly this — reports
    /// `Unchanged` and does not rewrite the file. A file holding single-use
    /// refresh tokens is not rewritten to say what it already said.
    #[test]
    fn redundant_write_reports_unchanged_and_leaves_the_file_alone() {
        let path = tmp_path("disable-noop");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        // Already enabled (no key at all) → re-enabling is a no-op.
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Unchanged
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            DISABLE_SAMPLE,
            "an Unchanged write must leave the file byte-identical, not reformat it"
        );

        // And once disabled, disabling again is a no-op too.
        save_disabled(&path, &by_name("acct-a"), true).unwrap();
        let disabled_text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), true).unwrap(),
            DisabledWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), disabled_text);
        fs::remove_file(&path).ok();
    }

    /// A `"disabled": false` already on disk is normalized away on re-enable
    /// rather than left sitting there — same end state the CLI produces.
    #[test]
    fn stale_disabled_false_on_disk_is_dropped() {
        let path = tmp_path("disable-stale-false");
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a", "disabled": false } ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "a stale false must be dropped, not kept: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// No on-disk entry carries that identity (deleted or renamed while the proxy
    /// ran): report `NoEntry` and write NOTHING, so the caller can warn that the
    /// flag will not survive a restart instead of silently believing it landed.
    #[test]
    fn disable_with_no_matching_entry_writes_nothing() {
        let path = tmp_path("disable-no-entry");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-gone"), true).unwrap(),
            DisabledWrite::NoEntry
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            DISABLE_SAMPLE,
            "a no-match write must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// A document with no usable `accounts` list is `NoEntry`, never a fallback
    /// that writes a whole config over it — the clobber `save_tokens` accepts for
    /// a single-use token is NOT worth it for a flag that can be re-set by hand.
    #[test]
    fn disable_with_no_usable_accounts_list_writes_nothing() {
        for document in [r#"{ "upstream": "x" }"#, r#"{ "accounts": "nope" }"#] {
            let path = tmp_path("disable-unusable");
            fs::write(&path, document).unwrap();
            assert_eq!(
                save_disabled(&path, &by_name("acct-a"), true).unwrap(),
                DisabledWrite::NoEntry
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), document);
            fs::remove_file(&path).ok();
        }
    }

    /// An unreadable or malformed file surfaces the ERROR rather than taking
    /// `save_tokens`' whole-config fallback. Writing a boot-time snapshot over a
    /// file we could not parse is the clobber this module exists to prevent.
    #[test]
    fn disable_surfaces_errors_instead_of_clobbering() {
        let missing = tmp_path("disable-missing");
        assert!(!missing.exists());
        assert!(matches!(
            save_disabled(&missing, &by_name("acct-a"), true),
            Err(ConfigError::Io(_))
        ));
        assert!(
            !missing.exists(),
            "a missing config must not be created by a flag write"
        );

        let malformed = tmp_path("disable-malformed");
        fs::write(&malformed, "{ not json").unwrap();
        assert!(matches!(
            save_disabled(&malformed, &by_name("acct-a"), true),
            Err(ConfigError::Parse(_))
        ));
        assert_eq!(
            fs::read_to_string(&malformed).unwrap(),
            "{ not json",
            "a malformed config must be left exactly as found"
        );
        fs::remove_file(&malformed).ok();
    }

    /// The flag lands on the right ORG when one email is logged into two — the
    /// same identity matching `merge_tokens` uses, so a rotated credential and a
    /// disabled flag can never land on two different entries.
    #[test]
    fn disable_picks_the_right_entry_when_one_email_has_two_orgs() {
        let path = tmp_path("disable-two-orgs");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-b", "accountUuid": "uuid-person",
                  "orgUuid": "org-b", "orgName": "Org B" }
            ] }"#,
        )
        .unwrap();

        let target = crate::identity::probe(
            "me@example.com",
            Some("uuid-person".to_string()),
            Some("org-b".to_string()),
            Some("Org B".to_string()),
        );
        assert_eq!(
            save_disabled(&path, &target, true).unwrap(),
            DisabledWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "the flag landed on the wrong org: {after}"
        );
        assert_eq!(after["accounts"][1]["disabled"], json!(true));
        fs::remove_file(&path).ok();
    }

    /// An AMBIGUOUS identity is refused, never resolved to the first match. Two
    /// entries sharing a name both satisfy `same_identity` (it falls back to name
    /// equality when either side lacks a uuid), and the TUI selects by ROW INDEX —
    /// so guessing lands the flag on whichever entry is earlier in the file,
    /// benching a healthy account while the TUI renders the other one disabled.
    /// Nothing is written and the ambiguity is reported distinctly.
    #[test]
    fn disable_refuses_an_ambiguous_identity_and_writes_nothing() {
        let path = tmp_path("disable-ambiguous");
        let document = r#"{ "accounts": [
            { "name": "acct-dup", "accessToken": "at-first" },
            { "name": "acct-dup", "accessToken": "at-second" }
        ] }"#;
        fs::write(&path, document).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-dup"), true).unwrap(),
            DisabledWrite::Ambiguous
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            document,
            "an ambiguous identity must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// The refusal is scoped to the AMBIGUOUS identity, not to the file: an
    /// unambiguous account in the same document still writes. Without this pair,
    /// "nothing was written" above would be satisfied by a save_disabled that had
    /// simply stopped working.
    #[test]
    fn a_duplicate_elsewhere_does_not_block_an_unambiguous_write() {
        let path = tmp_path("disable-ambiguous-neighbour");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "acct-dup", "accessToken": "at-first" },
                { "name": "acct-dup", "accessToken": "at-second" },
                { "name": "acct-unique", "accessToken": "at-unique" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-unique"), true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][2]["disabled"], json!(true));
        assert!(
            after["accounts"][0].get("disabled").is_none()
                && after["accounts"][1].get("disabled").is_none(),
            "only the unambiguous entry may be touched: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// Two entries that share a NAME but are separated by org are NOT ambiguous —
    /// `same_identity` tells them apart, so the write still lands. The refusal must
    /// bite on genuinely indistinguishable entries only, or the two-org config the
    /// identity work exists to support would stop being writable.
    #[test]
    fn two_orgs_under_one_name_are_not_ambiguous() {
        let path = tmp_path("disable-two-orgs-unambiguous");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-b", "accountUuid": "uuid-person",
                  "orgUuid": "org-b", "orgName": "Org B" }
            ] }"#,
        )
        .unwrap();

        let target = crate::identity::probe(
            "me@example.com",
            Some("uuid-person".to_string()),
            Some("org-a".to_string()),
            Some("Org A".to_string()),
        );
        assert_eq!(
            save_disabled(&path, &target, true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        assert!(after["accounts"][1].get("disabled").is_none());
        fs::remove_file(&path).ok();
    }

    /// The flag write goes through the same atomic 0600 path as every other
    /// write, so it can never leave the token file world-readable.
    #[test]
    fn save_disabled_writes_owner_only_permissions() {
        let path = tmp_path("disable-perm");
        fs::write(&path, DISABLE_SAMPLE).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn throttle_zero_spacing_is_inert() {
        // `Some(0)` spacing normalizes to unset (footgun parity with pacing).
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "throttle": { "minSpacingMs": 0 } }"#)
                .unwrap();
        assert_eq!(config.throttle.effective_min_spacing(), None);
        assert!(!config.throttle.is_active());
    }
}
