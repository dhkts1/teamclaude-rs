//! Whether a request asked Anthropic's prompt cache for the EXTENDED (1-hour)
//! TTL, read straight off the buffered body bytes already parsed once for
//! [`crate::model::parse_request_model`].
//!
//! Anthropic's `cache_control` breakpoints can appear at several depths — the
//! last `system` block, a tool definition, a message content block — and every
//! shape shares one property that matters here: the client only ever sets
//! `"ttl"` at all to ask for the NON-default window (`"1h"`; the field is
//! omitted entirely for the ordinary 5-minute default). So rather than modelling
//! every place a `cache_control` object can appear, this is a bounded byte scan:
//! find a `"cache_control"` key, then look for a `"ttl"` key anywhere in the
//! window that follows (key ORDER inside the object is not assumed — `"ttl"`
//! may come before or after `"type"`) and, only once `"ttl"` itself is found,
//! require `:` with optional JSON whitespace (space/tab/CR/LF) on either side
//! and the literal value `"1h"`. No second JSON parse of the body (nothing here
//! allocates a `Value` tree), and a short window so a `ttl`-shaped string
//! anywhere else in a large body (message content, a tool's own output) can
//! never be mistaken for the field.

/// How far past a `"cache_control"` key to look for its `ttl` value before
/// giving up — comfortably past `{"type":"ephemeral","ttl":"1h"}` even
/// pretty-printed with indentation, with margin for key reordering, without
/// being wide enough to reach into unrelated JSON.
const SCAN_WINDOW: usize = 128;

const CACHE_CONTROL_KEY: &[u8] = br#""cache_control""#;
const TTL_KEY: &[u8] = br#""ttl""#;
const EXTENDED_TTL_LITERAL: &[u8] = br#""1h""#;

/// `true` iff `body` contains at least one `cache_control` object whose `ttl`
/// is the extended `"1h"` window. Anything else — absent, `"5m"`, malformed,
/// truncated, not JSON at all — is `false`, which is today's 15-minute pin
/// behaviour. This must never be a way to get a LONGER pin from a malformed
/// body: the only path to `true` is a `"ttl"` key, JSON-whitespace-tolerant
/// `:`, and the exact literal `"1h"` — nothing looser than that.
pub fn requests_extended_ttl(body: &[u8]) -> bool {
    let mut start = 0;
    while let Some(rel) = find(&body[start..], CACHE_CONTROL_KEY) {
        let key_end = start + rel + CACHE_CONTROL_KEY.len();
        let window_end = (key_end + SCAN_WINDOW).min(body.len());
        if ttl_1h_in_window(&body[key_end..window_end]) {
            return true;
        }
        start = key_end;
    }
    false
}

/// Within `window` (already bounded to [`SCAN_WINDOW`] bytes past a
/// `"cache_control"` key), find a `"ttl"` key and check whether ITS value —
/// skipping whitespace, then `:`, then whitespace again, exactly as JSON
/// permits between any token and the next — is the literal `"1h"`.
fn ttl_1h_in_window(window: &[u8]) -> bool {
    let mut start = 0;
    while let Some(rel) = find(&window[start..], TTL_KEY) {
        let after_key = start + rel + TTL_KEY.len();
        let mut pos = skip_json_whitespace(window, after_key);
        if window.get(pos) == Some(&b':') {
            pos = skip_json_whitespace(window, pos + 1);
            if window[pos..].starts_with(EXTENDED_TTL_LITERAL) {
                return true;
            }
        }
        start = after_key;
    }
    false
}

/// Advance `pos` past any run of JSON whitespace (space, tab, CR, LF) —
/// the same four characters the JSON grammar itself treats as insignificant
/// between tokens, so this tolerates any client's pretty-printing without
/// tolerating anything JSON wouldn't.
fn skip_json_whitespace(body: &[u8], mut pos: usize) -> usize {
    while matches!(body.get(pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        pos += 1;
    }
    pos
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

    #[test]
    fn spaced_json_ttl_1h_is_detected() {
        let body = br#"{"cache_control": {"type": "ephemeral", "ttl": "1h"}}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn pretty_printed_ttl_1h_across_newlines_is_detected() {
        let body =
            b"{\n  \"cache_control\": {\n    \"type\": \"ephemeral\",\n    \"ttl\": \"1h\"\n  }\n}";
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn reordered_and_spaced_keys_are_detected() {
        let body = br#"{"cache_control": {"ttl": "1h", "type": "ephemeral"}}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn spaced_5m_ttl_is_not_extended() {
        let body = br#"{"cache_control": {"type": "ephemeral", "ttl": "5m"}}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn a_spaced_ttl_1h_in_message_content_far_from_cache_control_does_not_count() {
        let filler = "x".repeat(SCAN_WINDOW + 10);
        let body = format!(
            r#"{{"cache_control": {{"type": "ephemeral"}}, "note": "{filler}", "ttl": "1h"}}"#
        );
        assert!(!requests_extended_ttl(body.as_bytes()));
    }
}
