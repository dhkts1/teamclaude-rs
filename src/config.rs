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
/// An unreadable or malformed file falls back to writing the in-memory config:
/// a just-rotated refresh token is single-use, so dropping it strands that
/// account on `invalid_grant` forever, which is strictly worse than overwriting
/// a file we cannot parse anyway. Both fallbacks log a warning naming the cause.
///
/// The merge runs on the file's raw JSON document, NOT on a `Config` round-trip:
/// deserializing would materialize every serde default back into the file, so a
/// key the user just DELETED would reappear as its default (`"pacing": {}`) and a
/// key they never wrote would appear for the first time. Editing the parsed
/// document leaves the file byte-identical apart from the credential fields.
pub fn save_tokens(path: &Path, memory: &Config) -> Result<(), ConfigError> {
    let merged = match read_document(path) {
        Ok(mut doc) => {
            merge_tokens(&mut doc, memory);
            doc
        }
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
    write_atomic(path, &serde_json::to_string_pretty(&merged)?)
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

/// Overwrite the credential fields of every account present in BOTH `memory` and
/// the on-disk `doc`, matched by identity and never by position. Nothing else in
/// the document is touched.
///
/// Iterating the ON-DISK list and pulling tokens in gives both removal semantics
/// for free: an account the user deleted from the file is never resurrected (it
/// has no entry to write into), and an account on disk the server never loaded
/// is left alone (no memory match).
fn merge_tokens(doc: &mut Map<String, Value>, memory: &Config) {
    let Some(accounts) = doc.get_mut("accounts").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in accounts.iter_mut() {
        let Some(object) = entry.as_object_mut() else {
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
        let Some(fresh) = memory
            .accounts
            .iter()
            .find(|a| crate::identity::same_identity(a, &probe))
        else {
            continue;
        };
        let credentials = Credentials {
            access_token: &fresh.access_token,
            refresh_token: fresh.refresh_token.as_deref(),
            expires_at: fresh.expires_at,
        };
        let Ok(Value::Object(fields)) = serde_json::to_value(&credentials) else {
            continue;
        };
        object.extend(fields);
    }
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
