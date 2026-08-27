//! `Manager` throttle methods, split verbatim from `mod.rs`.

use super::select::{classify_request, RequestClass};
use super::*;

/// Whether a [`RequestClass`] is exempt from the PER-ORG GCRA when
/// [`Manager::throttle_exempt_noise_enabled`] is on (it never exempts the fleet
/// ceiling — see [`Manager::throttle_send`]). Today this is a single
/// `== RequestClass::Noise` comparison, but it is named rather than inlined at
/// the call site because of a load-bearing fact [`classify_request`]'s own
/// doc-comment states for the SELECTION side and this file depends on too:
/// `classify_request` **fails safe toward `ControlPreferred`** for any unknown
/// path. For throttling specifically, `ControlPreferred` is one of the two
/// classes that stays throttled (along with `Inference`) — so the
/// selection-side fail-safe direction happens to also be the throttle-side
/// fail-safe direction: an unrecognised path degrades to "still throttled",
/// never to "silently exempt". That agreement is a happy accident nobody had
/// written down before this comment.
///
/// The other reason this is named: `classify_request` now drives TWO
/// independent policies — account selection (`src/manager/select.rs`) and
/// throttling (here). Changing its `Noise` arm for a selection reason (e.g.
/// widening or narrowing the `/api/event_logging*` / `/mcp-registry*` prefix
/// match) silently changes which traffic this predicate exempts too. There is
/// no compiler check tying the two together — only this comment.
fn throttle_exempt(class: RequestClass) -> bool {
    class == RequestClass::Noise
}

/// Pure bucket-key derivation, split out of [`Manager::throttle_bucket_key`] so
/// both branches are unit-testable without building a `Manager` (same pattern as
/// [`throttle_slot`]).
///
/// Prefers the ORG identity via [`crate::identity::org_key_of`] (uuid, else name),
/// falling back to the account index when neither is known. Both arms are
/// namespaced so an org literally named `acct:3` cannot collide with account 3's
/// fallback key.
fn bucket_key_for(org_uuid: Option<&str>, org_name: Option<&str>, idx: usize) -> String {
    match crate::identity::org_key_of(org_uuid, org_name) {
        Some(key) => format!("org:{key}"),
        None => format!("acct:{idx}"),
    }
}

impl Manager {
    /// Whether `Noise`-classified traffic (`/api/event_logging*`,
    /// `/mcp-registry*` — see [`classify_request`]) skips the PER-ORG GCRA, read
    /// from the config's unmodelled top-level `throttleExemptNoise`. **Default
    /// `false`** — every account-served request pays a slot. Same read pattern as
    /// [`Manager::session_affinity_enabled`].
    ///
    /// Note this exempts ONE of the two buckets. Exempt traffic still pays the
    /// fleet ceiling, so it is never entirely unpaced.
    ///
    /// Ships OFF deliberately, but the reasoning changed when the throttle was
    /// split in two. The original argument was that freeing telemetry's ~32% of
    /// slots does not reduce upstream pressure, it *reallocates* it onto
    /// `/v1/messages` — true when ONE bucket was all there was, because every slot
    /// telemetry gave up was a slot inference immediately took.
    ///
    /// With a per-org bucket plus a fleet ceiling that is no longer the trade.
    /// Exempt traffic now skips only the PER-ORG bucket and still pays the fleet
    /// ceiling (see [`Manager::throttle_send`]), so total egress stays bounded
    /// while telemetry stops consuming an organization's burst budget — which is
    /// the budget an interactive turn is actually waiting on.
    ///
    /// The code default stays `false` regardless: this repo is public and other
    /// fleets may not have the account count to absorb it.
    pub fn throttle_exempt_noise_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("throttleExemptNoise")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// The GCRA bucket key for account `idx`: its ORGANIZATION when known,
    /// otherwise an account-scoped fallback.
    ///
    /// Keyed on org rather than account because that is what Anthropic limits.
    /// The two coincide on a fleet where every account is its own org (which is the
    /// case today), and diverge the moment two accounts share one — at which point
    /// account-keying would hand that org double its intended rate, the exact bug
    /// class this split exists to fix.
    ///
    /// Both arms are namespaced (`org:` / `acct:`) so a fallback key can never
    /// collide with an organization whose name happens to look like one.
    ///
    /// **A key can change once, mid-life.** A newly-added account starts with no
    /// `org_uuid` and gets one backfilled after identity resolution
    /// (`manager/mod.rs`, `add_or_update_account`), so its first requests bucket
    /// under `acct:<idx>` and later move to `org:<uuid>`. That re-key resets one
    /// bucket's TAT once, permitting a single unpaced burst for that one account.
    /// Accepted deliberately: pinning the key at first use would instead hold a
    /// WRONG key forever in the case where the backfill is what corrects it.
    fn throttle_bucket_key(&self, idx: usize) -> String {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account = accounts.get(idx);
        bucket_key_for(
            account.and_then(|a| a.org_uuid.as_deref()),
            account.and_then(|a| a.org_name.as_deref()),
            idx,
        )
    }

    /// Outbound-initiation throttle: two independent GCRA buckets at the single
    /// send site. Inert (returns immediately) when neither is configured. Holds NO
    /// resource across the sleep — pure initiation delay, cannot deadlock, never
    /// turns a request into a failure.
    ///
    /// 1. **Per-organization** ([`Manager::account_throttle`]), keyed by
    ///    [`Self::throttle_bucket_key`]. The real limiter, because Anthropic's
    ///    limits are per-organization. Capacity scales WITH the account pool.
    /// 2. **Fleet-wide ceiling** ([`Manager::fleet_throttle`]). Insurance against a
    ///    shared-identity limit nobody has measured; set far looser so it does not
    ///    bind in normal use.
    ///
    /// A slot is reserved in each active bucket and the request sleeps until the
    /// LATER of the two. That composition is deliberately conservative: reserving
    /// in both and waiting `max()` can advance a bucket's TAT slightly further than
    /// the request actually used, which errs toward MORE pacing, never less.
    ///
    /// Lock discipline: the two guards are taken sequentially and each is dropped
    /// before the next is acquired — never nested, so there is no ordering hazard
    /// and no deadlock to reason about.
    ///
    /// `path` is the caller's already query-stripped request path (see
    /// [`classify_request`]'s contract — matching on a raw target with
    /// `?query=...` mis-classifies `/v1/messages?beta=true` as `ControlPreferred`).
    /// When [`Self::throttle_exempt_noise_enabled`] is on and `path` classifies as
    /// [`RequestClass::Noise`], it skips the PER-ORG bucket only and still pays the
    /// fleet ceiling: telemetry stops eating an organization's burst budget, but
    /// total egress stays bounded.
    ///
    /// `idx` is the account already selected for this send — bound well before the
    /// call site in `proxy.rs` and used there for the token, client and UUID patch.
    /// Because the call sits inside the retry loop, a 429/529 rotation re-enters
    /// here and charges the NEW account's own bucket, which is the intended
    /// behaviour.
    pub async fn throttle_send(&self, path: &str, idx: usize) {
        let noise_exempt =
            self.throttle_exempt_noise_enabled() && throttle_exempt(classify_request(path));

        let now = crate::now_ms();
        let mut allow_at = now;

        if !noise_exempt {
            if let Some(spacing_ms) = self.account_throttle.effective_min_spacing() {
                let burst = self.account_throttle.effective_burst();
                let key = self.throttle_bucket_key(idx);
                let mut tats = self.org_tat_ms.lock().await;
                let tat = tats.entry(key).or_insert(0);
                let (new_tat, org_allow_at) = throttle_slot(*tat, now, spacing_ms as i64, burst);
                *tat = new_tat;
                allow_at = allow_at.max(org_allow_at);
            } // guard dropped here — never held across the sleep
        }

        if let Some(spacing_ms) = self.fleet_throttle.effective_min_spacing() {
            let burst = self.fleet_throttle.effective_burst();
            let mut tat = self.fleet_tat_ms.lock().await;
            let (new_tat, fleet_allow_at) = throttle_slot(*tat, now, spacing_ms as i64, burst);
            *tat = new_tat;
            allow_at = allow_at.max(fleet_allow_at);
        } // guard dropped here — never held across the sleep

        let wait = allow_at - now;
        if wait > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bucket_key_for;

    #[test]
    fn bucket_key_prefers_org_uuid() {
        assert_eq!(
            bucket_key_for(Some("org-abc"), Some("Acme Inc"), 7),
            "org:org-abc",
            "the uuid is the stable identity; the display name can change"
        );
    }

    #[test]
    fn bucket_key_falls_back_to_org_name_then_account() {
        assert_eq!(
            bucket_key_for(None, Some("Acme Inc"), 7),
            "org:Acme Inc",
            "a known org with no uuid still buckets per-org"
        );
        assert_eq!(
            bucket_key_for(None, None, 7),
            "acct:7",
            "an account with no org at all must still get a bucket, never share one"
        );
    }

    /// Two accounts in ONE org share a bucket — the whole reason the key is the
    /// org rather than the account index. Keying on the index would hand that org
    /// double its intended rate.
    #[test]
    fn two_accounts_in_one_org_share_a_bucket() {
        assert_eq!(
            bucket_key_for(Some("org-abc"), None, 3),
            bucket_key_for(Some("org-abc"), None, 9),
        );
    }

    /// Distinct orgs must never collide, and the `acct:` fallback must never
    /// collide with an org whose name happens to look like one — which is why both
    /// arms are namespaced.
    #[test]
    fn distinct_identities_never_collide() {
        assert_ne!(
            bucket_key_for(Some("org-abc"), None, 0),
            bucket_key_for(Some("org-xyz"), None, 0),
        );
        assert_ne!(
            bucket_key_for(None, Some("acct:3"), 9),
            bucket_key_for(None, None, 3),
            "an org NAMED `acct:3` must not land in account 3's fallback bucket"
        );
    }
}
