//! Whether a request asked Anthropic's prompt cache for the EXTENDED (1-hour)
//! TTL, read straight off the buffered body bytes already parsed once for
//! [`crate::model::parse_request_model`].
//!
//! Anthropic's `cache_control` breakpoints can appear at several depths — the
//! last `system` block, a tool definition, a message content block — and every
//! shape shares one property that matters here: the client only ever sets
//! `"ttl"` at all to ask for the NON-default window (`"1h"`; the field is
//! omitted entirely for the ordinary 5-minute default).
//!
//! This used to be a bounded byte scan for `"cache_control"` followed by a
//! nearby `"ttl"`. That is blind to JSON structure: it fires on an unrelated
//! sibling key that merely sits close to a real `cache_control` object, and —
//! worse — it fires on a JSON *string value* that happens to quote the shape,
//! which is exactly what an agent session embeds in a request body the moment
//! it reads or diffs this very file's own doc comments and test fixtures.
//!
//! So instead this is a typed [`serde_json`] peek, modelled on
//! [`crate::model::parse_request_model`]: a minimal `#[derive(Deserialize)]`
//! shape that only reads `cache_control` off the three structurally legal
//! positions (a `system` block, a tool definition, a message content block)
//! and checks *that object's own* `ttl` field. A `"ttl":"1h"` anywhere else —
//! including inside a string, a mismatched key, or a sibling object — is not
//! visible to this shape at all, so it can never be mistaken for the field.
//! This still does not allocate a generic `serde_json::Value` tree: every
//! field it does not care about is simply skipped by `serde` during the same
//! single pass.

use serde::Deserialize;

/// The `cache_control` object itself: only its own `ttl` matters here.
#[derive(Deserialize)]
struct CacheControl {
    #[serde(default)]
    ttl: Option<String>,
}

/// `true` iff `cache_control` is present and its own `ttl` is the extended
/// `"1h"` window. Absent, malformed-away, or any other value (e.g. `"5m"`) is
/// `false` — the ordinary default-window behaviour.
fn is_extended(cache_control: &Option<CacheControl>) -> bool {
    matches!(
        cache_control.as_ref().and_then(|c| c.ttl.as_deref()),
        Some("1h")
    )
}

/// A single content block — the shape shared by `system` array entries and a
/// message's `content` array entries. Everything but `cache_control` is
/// skipped.
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    cache_control: Option<CacheControl>,
}

/// `system` and a message's `content` are both legally either a plain string
/// (no `cache_control` possible) or an array of content blocks (where it is).
/// A string value — including one that happens to quote a `cache_control`
/// object's JSON shape — never matches the `Blocks` arm.
#[derive(Deserialize)]
#[serde(untagged)]
enum TextOrBlocks {
    // The string itself is never read — its only job is to consume the
    // `Text` shape so it does not fall through to `Blocks` and expose
    // whatever `cache_control`-shaped substring it might quote.
    Text(#[allow(dead_code)] String),
    Blocks(Vec<ContentBlock>),
}

impl TextOrBlocks {
    fn any_extended(&self) -> bool {
        match self {
            TextOrBlocks::Text(_) => false,
            TextOrBlocks::Blocks(blocks) => blocks.iter().any(|b| is_extended(&b.cache_control)),
        }
    }
}

/// One entry of `messages`. Everything but `content` is skipped.
#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<TextOrBlocks>,
}

/// One entry of `tools`. Everything but `cache_control` is skipped.
#[derive(Deserialize)]
struct ToolDef {
    #[serde(default)]
    cache_control: Option<CacheControl>,
}

/// The three top-level positions a `cache_control` object can legally appear
/// under. Everything else in the request body is skipped.
#[derive(Deserialize)]
struct TtlPeek {
    #[serde(default)]
    system: Option<TextOrBlocks>,
    #[serde(default)]
    messages: Option<Vec<Message>>,
    #[serde(default)]
    tools: Option<Vec<ToolDef>>,
}

/// `true` iff `body` contains at least one structurally legal `cache_control`
/// object — on a `system` block, a tool definition, or a message content
/// block — whose own `ttl` is the extended `"1h"` window. Anything else —
/// absent, `"5m"`, malformed, truncated, not JSON at all, or a `cache_control`
/// shape that only appears inside a string — is `false`, which is today's
/// 15-minute pin behaviour. This must never be a way to get a LONGER pin from
/// a malformed or merely lookalike body: the only path to `true` is a real
/// `cache_control` object, in one of the three legal positions, whose own
/// `ttl` is exactly `"1h"`.
pub fn requests_extended_ttl(body: &[u8]) -> bool {
    let Ok(peek) = serde_json::from_slice::<TtlPeek>(body) else {
        return false;
    };

    if peek.system.is_some_and(|s| s.any_extended()) {
        return true;
    }
    if let Some(messages) = peek.messages {
        if messages
            .iter()
            .any(|m| m.content.as_ref().is_some_and(TextOrBlocks::any_extended))
        {
            return true;
        }
    }
    if let Some(tools) = peek.tools {
        if tools.iter().any(|t| is_extended(&t.cache_control)) {
            return true;
        }
    }
    false
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
    fn detects_extended_ttl_on_a_tool_definition() {
        let body = br#"{"tools":[{"name":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn detects_extended_ttl_on_a_message_content_block() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
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
        let body = br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
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
    fn spaced_json_ttl_1h_is_detected() {
        let body = br#"{"system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn pretty_printed_ttl_1h_across_newlines_is_detected() {
        let body = b"{\n  \"system\": [{\n    \"type\": \"text\",\n    \"text\": \"x\",\n    \"cache_control\": {\n      \"type\": \"ephemeral\",\n      \"ttl\": \"1h\"\n    }\n  }]\n}";
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn reordered_and_spaced_keys_are_detected() {
        let body = br#"{"system": [{"cache_control": {"ttl": "1h", "type": "ephemeral"}, "type": "text", "text": "x"}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn spaced_5m_ttl_is_not_extended() {
        let body = br#"{"system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]}"#;
        assert!(!requests_extended_ttl(body));
    }

    /// Reproduced failure #1 from the bridge: a `cache_control` object with no
    /// `ttl`, sitting next to an unrelated sibling object that happens to have
    /// a `"ttl":"1h"` key of its own. Neither is a legal `cache_control`/`ttl`
    /// pairing, so this must be `false`.
    #[test]
    fn unrelated_sibling_ttl_next_to_cache_control_does_not_count() {
        let body = br#"{"cache_control":{"type":"ephemeral"},"config":{"ttl":"1h"}}"#;
        assert!(!requests_extended_ttl(body));
    }

    /// Reproduced failure #2 from the bridge: a real `cache_control` (with no
    /// `ttl`) inside a `system` block, plus an unrelated top-level `"tool"`
    /// (singular — not the legal `"tools"` array) whose own object has a
    /// `"ttl":"1h"`. Must be `false`.
    #[test]
    fn unrelated_singular_tool_ttl_does_not_count() {
        let body = br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}],"tool":{"ttl":"1h"}}"#;
        assert!(!requests_extended_ttl(body));
    }

    /// The self-trigger class: a message whose `content` is a plain STRING
    /// that happens to quote the literal `cache_control`/`ttl` JSON shape —
    /// e.g. an agent session pasting this very module's own test fixtures
    /// into a request. A string can never be a `cache_control` object, so
    /// this must be `false` regardless of what text it contains.
    #[test]
    fn ttl_1h_quoted_inside_a_message_string_does_not_count() {
        let body = br#"{"messages":[{"role":"user","content":"see {\"cache_control\":{\"type\":\"ephemeral\",\"ttl\":\"1h\"}} for reference"}]}"#;
        assert!(!requests_extended_ttl(body));
    }
}
