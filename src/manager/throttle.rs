//! `Manager` throttle methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Global outbound-initiation throttle. Inert (returns immediately) unless
    /// `throttle` is configured. When active, applies a GCRA token bucket across the
    /// WHOLE fleet at the single send site so a cold fan-out cannot burst the shared
    /// upstream limiter. Holds NO resource across the sleep — pure initiation delay,
    /// cannot deadlock, never turns a request into a failure.
    pub async fn throttle_send(&self) {
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
