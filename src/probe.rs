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
///
/// This is the CENTRE of a per-account random draw, not a fleet-wide period: the
/// background scheduler ([`crate::schedule`]) draws each account's next probe
/// uniformly from `cadence +/- 30%` and gives each a random initial offset, so N
/// accounts never share an instant and a restart re-scatters them. Raised from
/// 75s to 300s together with that change — a synchronized 75s fleet sweep was
/// both the loudest and the most fingerprintable thing this proxy did, and the
/// bars do not need refreshing four times a minute.
pub const DEFAULT_PROBE_SECONDS: u64 = 300;
/// Delay between sequential per-account probes in one *sweep*. Probing every
/// account concurrently bursts the usage endpoint and trips *its* 429 rate limit
/// — which used to surface as a false "error" on every row. Spacing the calls
/// keeps a full sweep well inside the cadence while staying under the endpoint's
/// burst limit.
///
/// Still load-bearing, and still the aggregate-rate bound the throttle default
/// mirrors ([`crate::config::ThrottleConfig`]). It applies to the two remaining
/// places that touch several accounts in a row: the one-shot sweeps
/// ([`crate::manager::Manager::probe_all`], `tcr accounts --probe`,
/// [`crate::manager::Manager::warm_all`]) and the rare case of two randomly
/// scheduled accounts coming due together. The background scheduler is strictly
/// *less* aggressive than the sweep it replaced — the same N calls are spread
/// across a whole cadence instead of packed into one `N * 350ms` run — so
/// nothing about the bound is weakened by it.
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
/// `extra_usage_usd` is not a bucket at all — see its own doc-comment.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub five_hour: Option<UsageBucket>,
    pub seven_day: Option<UsageBucket>,
    pub seven_day_oi: Option<UsageBucket>,
    /// Real, already-billed pay-as-you-go overage in USD, from the payload's
    /// `extra_usage` field. Unlike the three buckets above this is not a
    /// percentage of a limit — it is a dollar amount tcr cannot derive from
    /// response headers, so today's display falls back to `src/pricing.rs`'s
    /// synthetic token-counted estimate. `None` means "the endpoint did not
    /// report this field" or "its shape was not one we recognise" — never a
    /// fabricated `0.0`; see [`parse_extra_usage_usd`].
    pub extra_usage_usd: Option<f64>,
}

/// Why a probe failed. `status` carries the HTTP code when there was one — the
/// probe loop force-refreshes the token and retries exactly once on a `401`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("usage probe failed{}: {message}", .status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct ProbeError {
    pub status: Option<u16>,
    pub message: String,
    pub retry_after_secs: Option<u64>,
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

/// Pull the real, already-billed pay-as-you-go overage (in USD) out of the
/// payload's `extra_usage` field.
///
/// The endpoint's exact shape for this field is as undocumented as the
/// `limits[]` naming [`find_scoped_weekly_limit`] works around, so the same
/// style applies here: try the plausible key names instead of betting on one,
/// and let anything that does not match read as absent rather than `0.0`. A
/// bare numeric `extra_usage` (no wrapping object) is accepted too, since a
/// scalar dollar figure is at least as plausible as an object carrying one.
fn parse_extra_usage_usd(data: &Value) -> Option<f64> {
    let value = data.get("extra_usage")?;
    if value.is_null() {
        return None;
    }
    if let Some(scalar) = parse_pct(value) {
        return Some(scalar);
    }
    ["amount", "amount_usd", "usd", "total_usd", "cost", "value"]
        .iter()
        .find_map(|k| value.get(*k))
        .and_then(parse_pct)
}

/// Turn a parsed usage-endpoint payload into normalized buckets.
pub fn usage_from_payload(data: &Value) -> Usage {
    let fable = find_scoped_weekly_limit(data, "fable");
    Usage {
        five_hour: normalize_usage_bucket(data.get("five_hour")),
        seven_day: normalize_usage_bucket(data.get("seven_day")),
        seven_day_oi: normalize_usage_bucket(fable.as_ref()),
        extra_usage_usd: parse_extra_usage_usd(data),
    }
}

/// Fetch OAuth subscription usage for `access_token`. Zero message-quota spend.
pub async fn fetch_usage(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Usage, ProbeError> {
    fetch_usage_at(client, access_token, USAGE_URL).await
}

async fn fetch_usage_at(
    client: &reqwest::Client,
    access_token: &str,
    usage_url: &str,
) -> Result<Usage, ProbeError> {
    let resp = client
        .get(usage_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_USAGE_BETA)
        .header("Accept", "application/json")
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|e| ProbeError {
            status: None,
            message: e.to_string(),
            retry_after_secs: None,
        })?;

    let status = resp.status();
    if !status.is_success() {
        let retry_after_secs = crate::proxy::parse_retry_after(resp.headers())
            .and_then(|seconds| u64::try_from(seconds).ok());
        let detail = resp.text().await.unwrap_or_default();
        let detail = detail.chars().take(200).collect::<String>();
        return Err(ProbeError {
            status: Some(status.as_u16()),
            message: if detail.is_empty() {
                format!("HTTP {}", status.as_u16())
            } else {
                detail
            },
            retry_after_secs,
        });
    }

    let data: Value = resp.json().await.map_err(|e| ProbeError {
        status: Some(status.as_u16()),
        message: e.to_string(),
        retry_after_secs: None,
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

/// What one account's plan reads as, straight off the profile endpoint: the
/// three raw strings, verbatim, with no interpretation applied — the label is
/// derived from them exactly once, by [`tcr_status_wire::plan_label`].
///
/// All-`None` is the honest answer for a fetch that failed or returned nothing
/// usable, and it is what makes a failure cost nothing: the plan backfill
/// records no fields, marks no probe failed, and simply asks again on the next
/// probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub organization_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub seat_tier: Option<String>,
}

impl Plan {
    /// Whether this reading carries anything at all worth recording. A fetch
    /// that came back empty must not trigger a config write or overwrite a
    /// runtime field with a `None` we did not learn.
    pub fn is_empty(&self) -> bool {
        self.organization_type.is_none()
            && self.rate_limit_tier.is_none()
            && self.seat_tier.is_none()
    }
}

/// Future returned by [`PlanProber::plan`]. `'static` for the same reason
/// [`ProbeFuture`] is: it is awaited after every manager lock is released.
pub type PlanFuture = Pin<Box<dyn Future<Output = Plan> + Send>>;

/// Abstraction over "turn an access token into that account's plan", so the
/// backfill in the probe loop can be exercised without hitting the network —
/// the same seam, and for the same reason, as [`UsageProber`] beside it.
///
/// Infallible by design: it returns an empty [`Plan`] rather than a `Result`.
/// A plan is a nice-to-have label, and the probe loop's real job is quota. If
/// this call fails there is nothing for a caller to decide — record nothing,
/// leave the account's probe health alone (a failed PROFILE fetch must never
/// paint a red probe on a perfectly healthy account), and try again next time.
pub trait PlanProber: Send + Sync {
    fn plan(&self, access_token: String) -> PlanFuture;
}

/// The production plan prober: [`crate::oauth::fetch_profile`], the same HTTP
/// body login already uses. Deliberately not a second client with a second set
/// of proxy/timeout settings to keep in step — one call to the profile
/// endpoint, one implementation of it.
pub struct LivePlanProber;

impl PlanProber for LivePlanProber {
    fn plan(&self, access_token: String) -> PlanFuture {
        Box::pin(async move {
            let profile = crate::oauth::fetch_profile(&access_token).await;
            Plan {
                organization_type: profile.organization_type,
                rate_limit_tier: profile.rate_limit_tier,
                seat_tier: profile.seat_tier,
            }
        })
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
    /// A SUSTAINED run of consecutive 5xx reads from the usage endpoint — as
    /// opposed to one or a few, which still read as the benign `RateLimited`
    /// above. One or two 5xx is a hiccup the endpoint clears on its own; a run
    /// that keeps going is first-hand evidence the endpoint itself is down, and
    /// probe health being first-class means that must surface as its own
    /// visible state rather than keep hiding behind `RateLimited`'s "benign,
    /// self-clearing" label forever. See `Manager::probe_account`'s
    /// `SUSTAINED_5XX_THRESHOLD`.
    UpstreamDown,
}

impl ProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeStatus::Never => "never",
            ProbeStatus::Ok => "ok",
            ProbeStatus::Error => "error",
            ProbeStatus::Timeout => "timeout",
            ProbeStatus::RateLimited => "rate-limited",
            ProbeStatus::UpstreamDown => "upstream-down",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_usage_reads_numeric_retry_after_from_the_wire() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 321\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = fetch_usage_at(&client, "test-token", &format!("http://{address}/usage"))
            .await
            .unwrap_err();

        assert_eq!(error.status, Some(429));
        assert_eq!(error.retry_after_secs, Some(321));
    }

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

    /// 1a: a payload carrying real billed overage under the object shape
    /// (`{"amount": ...}`) must reach `Usage.extra_usage_usd`, not get dropped
    /// on the floor the way it is today.
    #[test]
    fn extra_usage_object_shape_is_parsed() {
        let data = serde_json::json!({
            "five_hour": { "used_percentage": 10 },
            "extra_usage": { "amount": "12.34" }
        });
        let usage = usage_from_payload(&data);
        assert_eq!(usage.extra_usage_usd, Some(12.34));
    }

    /// A bare numeric `extra_usage` (no wrapping object) is at least as
    /// plausible as an object carrying the figure, so it must parse too.
    #[test]
    fn extra_usage_bare_numeric_shape_is_parsed() {
        let data = serde_json::json!({ "extra_usage": 5.5 });
        let usage = usage_from_payload(&data);
        assert_eq!(usage.extra_usage_usd, Some(5.5));
    }

    /// A payload that never mentions `extra_usage` at all must read as absent,
    /// never as a fabricated `0.0` — the same rule `find_scoped_weekly_limit`'s
    /// own tests pin for the weekly bucket.
    #[test]
    fn extra_usage_absent_field_is_none() {
        let data = serde_json::json!({ "five_hour": { "used_percentage": 10 } });
        let usage = usage_from_payload(&data);
        assert_eq!(usage.extra_usage_usd, None);
    }

    /// An `extra_usage` present under a shape this parser does not recognise
    /// (no numeric candidate key, not a bare number) must ALSO read as absent —
    /// an unknown shape is "we could not read this", not "there is no overage".
    #[test]
    fn extra_usage_unrecognised_shape_is_none_not_zero() {
        let data = serde_json::json!({ "extra_usage": { "currency": "USD", "note": "n/a" } });
        let usage = usage_from_payload(&data);
        assert_eq!(
            usage.extra_usage_usd, None,
            "an unrecognised shape must not silently become a real-looking 0.0"
        );
    }

    /// End to end: a reported overage survives into the tracked `Quota`, and an
    /// absent one leaves a prior reading in place rather than erasing it.
    #[test]
    fn extra_usage_reaches_quota_and_survives_an_unread_probe() {
        let with_overage = usage_from_payload(&serde_json::json!({
            "extra_usage": { "amount": 7.0 }
        }));
        let mut quota = crate::quota::Quota::default();
        quota.apply_usage(&with_overage);
        assert_eq!(quota.extra_usage_usd, Some(7.0));

        let without_overage = usage_from_payload(&serde_json::json!({ "five_hour": {} }));
        quota.apply_usage(&without_overage);
        assert_eq!(
            quota.extra_usage_usd,
            Some(7.0),
            "an unread probe must not erase a previously learned overage"
        );
    }
}
