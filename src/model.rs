//! Per-model request classification for routing (Fable-aware account selection).
//!
//! tcr tracks a single model-scoped quota bucket — the weekly `7d_oi` window,
//! which on current plans is the **Fable** weekly cap. To gate it we need two
//! dependency-free primitives, ported from the JS `src/model.js`:
//!   - [`is_fable_model`] — does a model id name a Fable model?
//!   - [`parse_request_model`] — what model (if any) does a request body ask for?
//!
//! Everything else in the JS routing layer (the `routes[]`/glob table, per-account
//! `models`, the Sonnet bucket) is deliberately out of scope: tcr has exactly one
//! model-scoped bucket, so a single Fable predicate is the whole surface.

use serde::Deserialize;

/// Case-insensitive substring test for "fable" — the Rust port of the JS
/// `/fable/i.test(model)`. Any model id containing "fable" (in any case) is a
/// Fable model, so `claude-fable-5` and `CLAUDE-FABLE` both match while
/// `claude-opus-4-6` and the empty string do not.
pub fn is_fable_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("fable")
}

/// The top-level `model` key of a request body, if present. Only the ROOT `model`
/// is read — a `model` nested inside message content is never the request's target
/// model. Mirrors the `usage_from_json` parse pattern in `proxy.rs`: a lenient
/// `serde_json` peek that yields `None` on non-JSON or a missing/absent key.
pub fn parse_request_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<ModelPeek>(body)
        .ok()
        .and_then(|p| p.model)
}

/// Minimal shape that reads only the top-level `model` field, ignoring everything
/// else in the body.
#[derive(Deserialize)]
struct ModelPeek {
    #[serde(default)]
    model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_fable_model_is_case_insensitive() {
        assert!(is_fable_model("claude-fable-5"));
        assert!(is_fable_model("CLAUDE-FABLE"));
        assert!(!is_fable_model("claude-opus-4-6"));
        assert!(!is_fable_model(""));
    }

    #[test]
    fn parse_request_model_reads_top_level_model() {
        let body = br#"{"model":"claude-fable-5","messages":[]}"#;
        assert_eq!(
            parse_request_model(body),
            Some("claude-fable-5".to_string())
        );
    }

    #[test]
    fn parse_request_model_ignores_nested_model() {
        // A `model` buried inside message content is NOT the request's target.
        let body = br#"{"messages":[{"role":"user","content":{"model":"claude-fable-5"}}]}"#;
        assert_eq!(parse_request_model(body), None);
    }

    #[test]
    fn parse_request_model_none_when_absent() {
        let body = br#"{"messages":[]}"#;
        assert_eq!(parse_request_model(body), None);
    }

    #[test]
    fn parse_request_model_none_on_non_json() {
        assert_eq!(parse_request_model(b"not json at all"), None);
    }
}
