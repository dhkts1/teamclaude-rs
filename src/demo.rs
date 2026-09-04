//! `tcr demo` — render the live TUI against fake, in-memory accounts.
//!
//! This exists purely to capture a clean, sanitized dashboard screenshot for the
//! public README: the real TUI shows `@token.security` account names that must
//! never ship. The demo seeds a [`Manager`] with ~6 believable accounts in varied
//! states (healthy / near-limit / throttled / dead-cred) and hands it to the real
//! [`crate::tui::run`] — NO server, NO prober, NO warmer, NO network, NO real
//! tokens. Quitting (`q` / `Ctrl-C`) and terminal restore reuse the TUI's own
//! run loop unchanged; nothing here is ever exercised by the serving path.

use std::io;
use std::sync::Arc;

use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex as AsyncMutex;

use crate::manager::{AccountRuntime, AccountStatus, Manager};
use crate::probe::ProbeStatus;
use crate::quota::{Quota, QuotaWindow};
use crate::stats::{RequestLogEntry, SessionKind};

/// A 5-hour quota window at `util`, resetting a few hours out so it reads live
/// (a future reset means [`QuotaWindow::effective`] returns the seeded value).
fn five_hour(util: f64) -> QuotaWindow {
    QuotaWindow {
        utilization: util,
        reset: Some(OffsetDateTime::now_utc() + Duration::hours(3)),
    }
}

/// A weekly quota window at `util`, resetting a few days out.
fn seven_day(util: f64) -> QuotaWindow {
    QuotaWindow {
        utilization: util,
        reset: Some(OffsetDateTime::now_utc() + Duration::days(4)),
    }
}

/// A believable base runtime row; callers clone it and override the varying
/// fields. Every account carries obviously-fake credentials.
fn base(name: &str, priority: i64) -> AccountRuntime {
    let now = crate::now_ms();
    AccountRuntime {
        name: name.to_string(),
        account_type: "oauth".to_string(),
        account_uuid: None,
        org_uuid: None,
        org_name: Some("Demo Org".to_string()),
        priority,
        disabled: false,
        groups: Vec::new(),
        switch_threshold: None,
        access_token: "demo-access-token".to_string(),
        refresh_token: Some("demo-refresh-token".to_string()),
        expires_at_ms: Some(now + 3_600_000),
        status: AccountStatus::Active,
        quota: Quota::default(),
        // The demo's rows are presented as already-probed (`probe_status: Ok`), so
        // their quota reads as READ too — otherwise every demo row would render as
        // a permanently un-warmable account.
        quota_known: true,
        consecutive_probe_failures: 0,
        consecutive_warms_without_evidence: 0,
        warm_evidence_retry_after_ms: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        requests: 0,
        last_used_ms: None,
        last_selected_seq: 0,
        in_flight: 0,
        last_served_ms: 0,
        rate_limited_until_ms: None,
        overall_rejected_until_ms: None,
        five_hour_rejected_until_ms: None,
        seven_day_rejected_until_ms: None,
        fable_rejected_until_ms: None,
        refresh_retry_after_ms: None,
        error_retry_after_ms: None,
        error_backoff_ms: 0,
        probe_status: ProbeStatus::Ok,
        last_probe_ms: Some(now - 12_000),
        probe_error: None,
        probe_retry_after_ms: None,
        stream_error_times_ms: std::collections::VecDeque::new(),
        last_stream_error: None,
        refresh_lock: Arc::new(AsyncMutex::new(())),
        http: crate::manager::build_serving_client(false),
        serves_since_client_build: 0,
    }
}

/// An age `secs` seconds in the past, in epoch-ms — for `last_used` columns.
fn ago_ms(secs: i64) -> i64 {
    crate::now_ms() - secs * 1000
}

/// Build the six fake account rows described in the demo spec.
fn demo_accounts() -> Vec<AccountRuntime> {
    // alice — green, healthy, actively serving.
    let mut alice = base("alice", 0);
    alice.quota.five_hour = Some(five_hour(0.45));
    alice.quota.seven_day = Some(seven_day(0.20));
    alice.requests = 1240;
    alice.input_tokens = 4_200_000;
    alice.output_tokens = 310_000;
    alice.last_used_ms = Some(ago_ms(3));

    // bob — green, low use.
    let mut bob = base("bob", 0);
    bob.quota.five_hour = Some(five_hour(0.12));
    bob.quota.seven_day = Some(seven_day(0.08));
    bob.requests = 340;
    bob.input_tokens = 1_100_000;
    bob.output_tokens = 88_000;
    bob.last_used_ms = Some(ago_ms(120));

    // carol — near-limit (yellow "near" on the weekly bar). Her own lower
    // threshold makes 0.88 read as near without touching the fleet default.
    let mut carol = base("carol", 0);
    carol.switch_threshold = Some(0.85);
    carol.quota.five_hour = Some(five_hour(0.88));
    carol.quota.seven_day = Some(seven_day(0.76));
    carol.requests = 2130;
    carol.input_tokens = 7_800_000;
    carol.output_tokens = 540_000;
    carol.last_used_ms = Some(ago_ms(3600));

    // team-prod — green, and the CURRENT serving account (▶ marker).
    let mut team_prod = base("team-prod", 0);
    team_prod.quota.five_hour = Some(five_hour(0.30));
    team_prod.quota.seven_day = Some(seven_day(0.15));
    team_prod.requests = 890;
    team_prod.input_tokens = 2_600_000;
    team_prod.output_tokens = 190_000;
    team_prod.last_used_ms = Some(ago_ms(1));

    // dave — 5h window full (red "full"), parked out on a 429 throttle hold.
    let mut dave = base("dave", 10);
    dave.status = AccountStatus::Throttled;
    dave.rate_limited_until_ms = Some(crate::now_ms() + 15 * 60_000);
    dave.quota.five_hour = Some(five_hour(1.00));
    dave.quota.seven_day = Some(seven_day(0.40));
    dave.requests = 60;
    dave.input_tokens = 300_000;
    dave.output_tokens = 22_000;
    dave.last_used_ms = Some(ago_ms(720));

    // backup — dead credential: red "error" status, failing probe, no quota.
    let mut backup = base("backup", 10);
    backup.status = AccountStatus::Error;
    backup.probe_status = ProbeStatus::Error;
    backup.probe_error = Some("refresh token rejected (HTTP 401)".to_string());
    backup.last_probe_ms = Some(ago_ms(45));

    vec![alice, bob, carol, team_prod, dave, backup]
}

/// A recent-request log entry `secs` seconds ago.
fn log_entry(secs: i64, method: &str, path: &str, status: u16, account: &str) -> RequestLogEntry {
    RequestLogEntry {
        time: OffsetDateTime::now_utc() - Duration::seconds(secs),
        method: method.to_string(),
        path: path.to_string(),
        status,
        account: account.to_string(),
    }
}

/// `tcr demo` — seed a fake fleet and launch the real TUI. Purely for a sanitized
/// README screenshot; returns once the TUI's own quit handling restores the terminal.
pub async fn run_demo() -> io::Result<()> {
    let manager = Manager::from_runtimes(demo_accounts());

    // team-prod (index 3) is the account currently serving — paints the ▶ marker.
    manager.set_current(3);

    // Two live stable sessions so the sessions pane isn't empty in the screenshot.
    manager.seed_session(0xA1F3, 3, 24, ago_ms(1), SessionKind::Stable);
    manager.seed_session(0xB2C7, 0, 12, ago_ms(3), SessionKind::Stable);

    // A handful of recent requests, pushed oldest-first (the ring buffer is
    // reversed to most-recent-first when the snapshot is built).
    for entry in [
        log_entry(300, "GET", "/v1/models", 200, "bob"),
        log_entry(120, "POST", "/v1/messages", 200, "carol"),
        log_entry(30, "POST", "/v1/messages", 429, "dave"),
        log_entry(3, "POST", "/v1/messages", 200, "alice"),
        log_entry(1, "POST", "/v1/messages", 200, "team-prod"),
    ] {
        manager.push_log(entry);
    }

    crate::tui::run(manager).await
}
