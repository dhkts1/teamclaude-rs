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
    0.90
}
/// Default pacing when the `pacing` key is absent: cap each account at 3 requests
/// concurrently in flight, no min-spacing. This makes a concurrent burst (e.g. a
/// subagent fan-out) SPREAD across the pool instead of saturating one
/// affinity-pinned account. It is safe to default on because pacing is soft — the
/// select() fallback still serves the least-loaded account when all are at the
/// cap, so it can only ever spread load, never drop a request. Set
/// `"pacing": {}` in the config to disable (all knobs `None`).
fn default_pacing() -> PacingConfig {
    PacingConfig {
        max_in_flight_per_account: Some(3),
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
    /// Per-account request pacing. Absent in JSON → [`default_pacing`] →
    /// `maxInFlightPerAccount: 3` so a concurrent burst spreads across the pool
    /// (default-ON, soft). Set `"pacing": {}` to disable (all knobs `None`).
    #[serde(default = "default_pacing")]
    pub pacing: PacingConfig,
    /// Global outbound throttle. Absent → [`default_throttle`] (ON:
    /// `minSpacingMs: 350, burst: 4`). Set `"throttle": {}` to disable (all knobs
    /// `None`), or override the knobs to tune the live rate (read at boot).
    #[serde(default = "default_throttle")]
    pub throttle: ThrottleConfig,
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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
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
    let json = serde_json::to_string_pretty(config)?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));

    // Ensure the parent dir exists (a freshly-provisioned box may lack
    // `~/.config`), so a token-refresh save never fails with ENOENT and drops the
    // rotated refresh token. Fails loudly on a real perms error (finding #2).
    fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "teamclaude.json".to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(config.switch_threshold, 0.90);
        // Absent `pacing` key → good defaults: cap 3 in-flight, no min-spacing.
        assert_eq!(config.pacing.max_in_flight_per_account, Some(3));
        assert_eq!(config.pacing.min_spacing_ms, None);
        assert!(config.pacing.is_active());
    }

    #[test]
    fn empty_pacing_object_disables_pacing() {
        // `"pacing": {}` is the explicit opt-out: both knobs None → inert.
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
    fn throttle_zero_spacing_is_inert() {
        // `Some(0)` spacing normalizes to unset (footgun parity with pacing).
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "throttle": { "minSpacingMs": 0 } }"#)
                .unwrap();
        assert_eq!(config.throttle.effective_min_spacing(), None);
        assert!(!config.throttle.is_active());
    }
}
