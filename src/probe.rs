//! Active, zero-spend quota probe — the real feature of this proxy.
//!
//! Ported from `teamclaude/src/prober.js` + `oauth.js` (`fetchUsage`,
//! `normalizeUsageBucket`, `findScopedWeeklyLimit`). On an interval the manager
//! probes **every** OAuth account's quota via the Anthropic OAuth *usage*
//! endpoint — which reports utilization WITHOUT spending message quota — so idle
//! accounts' bars stay fresh instead of freezing at their last-served value.
//!
//! The endpoint reports utilization as a percentage in `0..=100`, so a raw `1`
//! means 1%, not 100%: [`normalize_usage_bucket`] divides by 100.
//!
//! **Probe health is first-class.** Each probe records ok / error / timeout with
//! a timestamp and message; the manager surfaces it to the TUI so a *failing*
//! probe reads as a visible error rather than a silently-frozen bar (the JS
//! proxy hid probe failures — that is what this design fixes).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;

/// Anthropic OAuth usage endpoint (zero message-quota spend).
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// `anthropic-beta` header value required by the usage endpoint.
pub const OAUTH_USAGE_BETA: &str = "oauth-2025-04-20";
/// Per-probe wall-clock ceiling; a probe that outlives it reads as a timeout.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default probe cadence when `quotaProbeSeconds` is absent from the config.
pub const DEFAULT_PROBE_SECONDS: u64 = 75;
/// Delay between sequential per-account probes in one sweep. Probing every
/// account concurrently bursts the usage endpoint and trips *its* 429 rate limit
/// — which used to surface as a false "error" on every row. Spacing the calls
/// keeps a full sweep well inside the cadence while staying under the endpoint's
/// burst limit.
pub const PROBE_SPACING: Duration = Duration::from_millis(350);

/// One normalized usage window: fractional utilization (`0.0..=1.0+`) and the
/// reset instant in epoch **milliseconds**. Either may be `None` when the
/// endpoint omitted it (matches the JS `{ utilization: null, resetAt }` shape).
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageBucket {
    pub utilization: Option<f64>,
    pub reset_at_ms: Option<i64>,
}

/// The normalized buckets a single probe yields. `seven_day_oi` is the
/// model-scoped (Fable) weekly, pulled from the payload's `limits[]` array.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub five_hour: Option<UsageBucket>,
    pub seven_day: Option<UsageBucket>,
    pub seven_day_oi: Option<UsageBucket>,
}

/// Why a probe failed. `status` carries the HTTP code when there was one — the
/// probe loop force-refreshes the token and retries exactly once on a `401`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("usage probe failed{}: {message}", .status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct ProbeError {
    pub status: Option<u16>,
    pub message: String,
}

/// Parse a percentage-ish JSON scalar (number or numeric string) to `f64`.
fn parse_pct(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Normalize a raw reset scalar to epoch **milliseconds**. Numbers below `1e12`
/// are treated as seconds; strings may be numeric or RFC3339.
fn parse_reset_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().map(to_ms),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(num) = s.parse::<i64>() {
                return Some(to_ms(num));
            }
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .ok()
                .map(|dt| (dt.unix_timestamp_nanos() / 1_000_000) as i64)
        }
        _ => None,
    }
}

fn to_ms(v: i64) -> i64 {
    if v < 1_000_000_000_000 {
        v * 1000
    } else {
        v
    }
}

/// Normalize one usage bucket (`five_hour`, `seven_day`, or a scoped limit) into
/// a [`UsageBucket`]. Returns `None` only when the bucket itself is absent.
pub fn normalize_usage_bucket(bucket: Option<&Value>) -> Option<UsageBucket> {
    let bucket = bucket?;
    if !bucket.is_object() {
        return None;
    }
    let utilization = ["used_percentage", "utilization", "usedPercentage"]
        .iter()
        .find_map(|k| bucket.get(*k))
        .and_then(parse_pct)
        .map(|pct| pct / 100.0);
    let reset_at_ms = ["resets_at", "resetsAt", "reset_at", "resetAt"]
        .iter()
        .find_map(|k| bucket.get(*k))
        .and_then(parse_reset_ms);
    Some(UsageBucket {
        utilization,
        reset_at_ms,
    })
}

/// Pull a per-model weekly limit out of the payload's `limits[]` array (where
/// the endpoint now reports model-scoped quota as a `weekly` entry carrying
/// `scope.model.display_name`). Returns a synthesized bucket-shaped value.
///
/// Note the endpoint's naming asymmetry, which is why this translation exists at
/// all: a top-level bucket (`five_hour`, `seven_day`) reports its fraction under
/// one of three candidate keys led by `used_percentage`, while a `limits[]` entry
/// reports the same quantity only as `percent`. [`normalize_usage_bucket`] knows
/// the former set, so the entry is re-keyed to `utilization` here.
///
/// A key whose source value is missing **or `null` is omitted, never emitted as
/// null**. `serde_json::json!` renders an absent `entry.get(..)` as
/// `Value::Null`, so building the object unconditionally produced
/// `{"utilization": null}` — an object that is *present* and therefore passes
/// [`normalize_usage_bucket`]'s absent-check, carrying a hole downstream disguised
/// as a reading. The distinction is load-bearing in [`crate::quota::apply_bucket`]
/// and only stays honest if "we did not read it" is expressed as absence at every
/// hop.
pub fn find_scoped_weekly_limit(data: &Value, needle: &str) -> Option<Value> {
    let needle = needle.to_lowercase();
    let entry = data.get("limits")?.as_array()?.iter().find(|l| {
        l.get("group").and_then(Value::as_str) == Some("weekly")
            && l.get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str)
                .is_some_and(|name| name.to_lowercase().contains(&needle))
    })?;
    let mut bucket = serde_json::Map::new();
    for (src, dst) in [("percent", "utilization"), ("resets_at", "resets_at")] {
        match entry.get(src) {
            Some(v) if !v.is_null() => {
                bucket.insert(dst.to_string(), v.clone());
            }
            _ => {}
        }
    }
    Some(Value::Object(bucket))
}

/// Turn a parsed usage-endpoint payload into normalized buckets.
pub fn usage_from_payload(data: &Value) -> Usage {
    let fable = find_scoped_weekly_limit(data, "fable");
    Usage {
        five_hour: normalize_usage_bucket(data.get("five_hour")),
        seven_day: normalize_usage_bucket(data.get("seven_day")),
        seven_day_oi: normalize_usage_bucket(fable.as_ref()),
    }
}

/// Fetch OAuth subscription usage for `access_token`. Zero message-quota spend.
pub async fn fetch_usage(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Usage, ProbeError> {
    let resp = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_USAGE_BETA)
        .header("Accept", "application/json")
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|e| ProbeError {
            status: None,
            message: e.to_string(),
        })?;

    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        let detail = detail.chars().take(200).collect::<String>();
        return Err(ProbeError {
            status: Some(status.as_u16()),
            message: if detail.is_empty() {
                format!("HTTP {}", status.as_u16())
            } else {
                detail
            },
        });
    }

    let data: Value = resp.json().await.map_err(|e| ProbeError {
        status: Some(status.as_u16()),
        message: e.to_string(),
    })?;
    Ok(usage_from_payload(&data))
}

/// Future returned by [`UsageProber::probe`]. `'static` so it can be awaited
/// after any manager lock is released.
pub type ProbeFuture = Pin<Box<dyn Future<Output = Result<Usage, ProbeError>> + Send>>;

/// Abstraction over "turn an access token into usage", so the probe loop can be
/// exercised in tests without hitting the network.
pub trait UsageProber: Send + Sync {
    fn probe(&self, access_token: String) -> ProbeFuture;
}

/// The production prober: a real HTTPS call to the Anthropic usage endpoint.
pub struct LiveUsageProber {
    client: reqwest::Client,
}

impl LiveUsageProber {
    pub fn new() -> Self {
        Self {
            // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
            // proxy — an ambient proxy env var would route the quota probe through
            // another proxy and silently fail every probe (frozen quota bars).
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build reqwest client"),
        }
    }
}

impl Default for LiveUsageProber {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageProber for LiveUsageProber {
    fn probe(&self, access_token: String) -> ProbeFuture {
        let client = self.client.clone();
        Box::pin(async move { fetch_usage(&client, &access_token).await })
    }
}

/// Health of an account's most recent probe, surfaced to the TUI. A failing
/// probe becomes a visible [`ProbeStatus::Error`]/[`ProbeStatus::Timeout`],
/// never a silently-frozen bar.
/// Serialized kebab-case so it crosses the status endpoint's wire
/// ([`crate::status`]) as the same token [`ProbeStatus::as_str`] already prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeStatus {
    /// Never probed yet (or a non-OAuth account).
    #[default]
    Never,
    /// Last probe succeeded.
    Ok,
    /// Last probe returned an error (HTTP or transport).
    Error,
    /// Last probe exceeded [`PROBE_TIMEOUT`].
    Timeout,
    /// The usage endpoint rate-limited the probe itself (HTTP 429). Benign: the
    /// account's *serving* quota is unaffected and its last-learned bar is kept
    /// (this path never calls `apply_usage`), so it must NOT read as a red error —
    /// a probe 429 painting "error" on every row is exactly the bug this fixes.
    RateLimited,
}

impl ProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeStatus::Never => "never",
            ProbeStatus::Ok => "ok",
            ProbeStatus::Error => "error",
            ProbeStatus::Timeout => "timeout",
            ProbeStatus::RateLimited => "rate-limited",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_divides_percentage_by_100() {
        let bucket = serde_json::json!({ "used_percentage": 42, "resets_at": 1_893_456_000 });
        let out = normalize_usage_bucket(Some(&bucket)).unwrap();
        assert_eq!(out.utilization, Some(0.42));
        // Seconds → ms.
        assert_eq!(out.reset_at_ms, Some(1_893_456_000_000));
    }

    #[test]
    fn normalize_accepts_alt_keys_and_ms_reset() {
        let bucket = serde_json::json!({ "utilization": "90", "resetAt": 1_893_456_000_000i64 });
        let out = normalize_usage_bucket(Some(&bucket)).unwrap();
        assert_eq!(out.utilization, Some(0.90));
        assert_eq!(out.reset_at_ms, Some(1_893_456_000_000));
    }

    #[test]
    fn normalize_absent_bucket_is_none() {
        assert!(normalize_usage_bucket(None).is_none());
        assert!(normalize_usage_bucket(Some(&Value::Null)).is_none());
    }

    #[test]
    fn scoped_weekly_limit_matches_fable_by_display_name() {
        let data = serde_json::json!({
            "limits": [
                { "group": "weekly", "scope": { "model": { "display_name": "Claude Sonnet" } }, "percent": 10, "resets_at": 1 },
                { "group": "weekly", "scope": { "model": { "display_name": "Claude Fable 5" } }, "percent": 55, "resets_at": 1_893_456_000 }
            ]
        });
        let usage = usage_from_payload(&data);
        let oi = usage.seven_day_oi.unwrap();
        assert_eq!(oi.utilization, Some(0.55));
    }

    /// A `limits[]` entry with no `percent` must not be re-keyed into a
    /// `{"utilization": null}` object. That object is *present*, so it sails past
    /// `normalize_usage_bucket`'s absent-check and lands on `apply_bucket` looking
    /// like a bucket that was read — which is how a hole became a `0.0`.
    #[test]
    fn scoped_weekly_limit_missing_percent_omits_the_key() {
        let data = serde_json::json!({
            "limits": [
                { "group": "weekly", "scope": { "model": { "display_name": "Claude Fable 5" } }, "resets_at": 1_893_456_000 }
            ]
        });
        let synthesized = find_scoped_weekly_limit(&data, "fable").expect("the entry matched");
        assert!(
            synthesized.get("utilization").is_none(),
            "an unread percent must be omitted, not emitted as null: {synthesized}"
        );
        assert!(
            synthesized.get("resets_at").is_some(),
            "the reset it DID report is still carried"
        );

        // End to end: the hole stays a hole all the way into the tracked window.
        let usage = usage_from_payload(&data);
        assert_eq!(
            usage.seven_day_oi.expect("bucket present").utilization,
            None
        );
        let mut quota = crate::quota::Quota::default();
        quota.apply_usage(&usage);
        assert!(
            quota.seven_day_oi.is_none(),
            "an unreadable percent must reach the wire as absent, never as 0.0"
        );
    }

    /// Today's live payload shape — every weekly entry carrying an integer
    /// `percent` — must be completely unaffected by the omission rule above.
    #[test]
    fn scoped_weekly_limit_with_a_numeric_percent_is_unchanged() {
        let data = serde_json::json!({
            "limits": [
                { "group": "weekly", "scope": { "model": { "display_name": "Claude Fable 5" } }, "percent": 0, "resets_at": 1_893_456_000 }
            ]
        });
        let usage = usage_from_payload(&data);
        let oi = usage.seven_day_oi.expect("bucket present");
        assert_eq!(
            oi.utilization,
            Some(0.0),
            "a reported 0 percent is a reading and stays one"
        );
        assert_eq!(oi.reset_at_ms, Some(1_893_456_000_000));

        let mut quota = crate::quota::Quota::default();
        quota.apply_usage(&usage);
        assert_eq!(
            quota.seven_day_oi.map(|w| w.utilization),
            Some(0.0),
            "a genuine 0% weekly still renders as a measured empty bar"
        );
    }

    #[test]
    fn rfc3339_reset_string_parses() {
        let bucket =
            serde_json::json!({ "used_percentage": 5, "resets_at": "2030-01-01T00:00:00Z" });
        let out = normalize_usage_bucket(Some(&bucket)).unwrap();
        assert!(out.reset_at_ms.is_some());
    }
}
