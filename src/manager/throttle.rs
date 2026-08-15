//! `Manager` throttle methods, split verbatim from `mod.rs`.

use super::select::{classify_request, RequestClass};
use super::*;

impl Manager {
    /// Whether `Noise`-classified traffic (`/api/event_logging*`,
    /// `/mcp-registry*` — see [`classify_request`]) skips the fleet-wide GCRA
    /// entirely, read from the config's unmodelled top-level
    /// `throttleExemptNoise`. **Default `false`** — current behaviour, every
    /// account-served request pays a slot. Same read pattern as
    /// [`Manager::session_affinity_enabled`].
    ///
    /// Ships OFF deliberately: freeing telemetry's ~32% of slots does not
    /// reduce upstream pressure, it *reallocates* it onto `/v1/messages` — the
    /// only endpoint currently taking 429s. See
    /// `data/plans/swarm-retrospectives.md` (main checkout) for the measured
    /// basis and kill criteria before flipping the default.
    pub fn throttle_exempt_noise_enabled(&self) -> bool {
        self.config
            .lock()
            .expect("config lock poisoned")
            .extra
            .get("throttleExemptNoise")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Global outbound-initiation throttle. Inert (returns immediately) unless
    /// `throttle` is configured. When active, applies a GCRA token bucket across the
    /// WHOLE fleet at the single send site so a cold fan-out cannot burst the shared
    /// upstream limiter. Holds NO resource across the sleep — pure initiation delay,
    /// cannot deadlock, never turns a request into a failure.
    ///
    /// `path` is the caller's already query-stripped request path (see
    /// [`classify_request`]'s contract — matching on a raw target with
    /// `?query=...` mis-classifies `/v1/messages?beta=true` as
    /// `ControlPreferred`). When [`Self::throttle_exempt_noise_enabled`] is on
    /// and `path` classifies as [`RequestClass::Noise`], this returns
    /// immediately without consuming a GCRA slot — telemetry stops queueing
    /// behind inference, but only once explicitly opted in.
    pub async fn throttle_send(&self, path: &str) {
        if self.throttle_exempt_noise_enabled() && classify_request(path) == RequestClass::Noise {
            return;
        }
        let Some(spacing_ms) = self.throttle.effective_min_spacing() else {
            return;
        };
        let burst = self.throttle.effective_burst();
        let now = crate::now_ms();
        let allow_at = {
            let mut tat = self.throttle_tat_ms.lock().await;
            let (new_tat, allow_at) = throttle_slot(*tat, now, spacing_ms as i64, burst);
            *tat = new_tat;
            allow_at
        }; // guard dropped here — never held across the sleep
        let wait = allow_at - now;
        if wait > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        }
    }
}
