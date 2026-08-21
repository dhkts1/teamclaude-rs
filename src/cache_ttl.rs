//! Whether a request asked Anthropic's prompt cache for the EXTENDED (1-hour)
//! TTL, read straight off the buffered body bytes already parsed once for
//! [`crate::model::parse_request_model`].
//!
//! Anthropic's `cache_control` breakpoints can appear at several depths — the
//! last `system` block, a tool definition, a message content block — and every
//! shape shares one property that matters here: the client only ever sets
//! `"ttl"` at all to ask for the NON-default window (`"1h"`; the field is
//! omitted entirely for the ordinary 5-minute default). So rather than modelling
//! every place a `cache_control` object can appear, this is a bounded byte scan
//! for the literal `"ttl":"1h"` immediately following a `"cache_control"` key —
//! no second JSON parse of the body (nothing here allocates a `Value` tree),
//! and a short window so a `ttl`-shaped string anywhere else in a large body
//! (message content, a tool's own output) can never be mistaken for the field.

/// How far past a `"cache_control"` key to look for its `ttl` value before
/// giving up — comfortably past `{"type":"ephemeral","ttl":"1h"}` (~40 bytes,
/// pretty-printed or not) with margin for whitespace and key reordering,
/// without being wide enough to reach into unrelated JSON.
const SCAN_WINDOW: usize = 128;

const CACHE_CONTROL_KEY: &[u8] = br#""cache_control""#;
const EXTENDED_TTL_VALUE: &[u8] = br#""ttl":"1h""#;

/// `true` iff `body` contains at least one `cache_control` object whose `ttl`
/// is the extended `"1h"` window. Anything else — absent, `"5m"`, malformed,
/// truncated, not JSON at all — is `false`, which is today's 15-minute pin
/// behaviour. This must never be a way to get a LONGER pin from a malformed
/// body: the only path to `true` is the exact literal Anthropic's API expects.
pub fn requests_extended_ttl(body: &[u8]) -> bool {
    let mut start = 0;
    while let Some(rel) = find(&body[start..], CACHE_CONTROL_KEY) {
        let key_end = start + rel + CACHE_CONTROL_KEY.len();
        let window_end = (key_end + SCAN_WINDOW).min(body.len());
        if find(&body[key_end..window_end], EXTENDED_TTL_VALUE).is_some() {
            return true;
        }
        start = key_end;
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_extended_ttl_on_a_system_block() {
        let body = br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn default_cache_control_with_no_ttl_is_not_extended() {
        let body =
            br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn explicit_5m_ttl_is_not_extended() {
        let body = br#"{"cache_control":{"type":"ephemeral","ttl":"5m"}}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn absent_cache_control_is_not_extended() {
        let body = br#"{"model":"claude-x","messages":[]}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn malformed_json_is_not_extended() {
        assert!(!requests_extended_ttl(b"not json { at all"));
        assert!(!requests_extended_ttl(b""));
    }

    #[test]
    fn a_ttl_1h_string_far_from_any_cache_control_key_does_not_count() {
        // The literal appears in unrelated content, well past the scan window
        // from any `cache_control` key — must not false-positive.
        let filler = "x".repeat(SCAN_WINDOW + 10);
        let body =
            format!(r#"{{"cache_control":{{"type":"ephemeral"}},"note":"{filler}","ttl":"1h"}}"#);
        assert!(!requests_extended_ttl(body.as_bytes()));
    }

    #[test]
    fn extended_ttl_on_a_tool_definition_is_detected() {
        let body = br#"{"tools":[{"name":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }
}
