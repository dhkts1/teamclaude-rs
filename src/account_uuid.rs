//! Rewrite the request body's `account_uuid` to match the pooled account whose
//! token we inject. Claude Code puts the logged-in account's UUID inside
//! `metadata.user_id` (a *stringified* JSON string) of a `/v1/messages` body;
//! under rotation that UUID would disagree with the injected token.
//!
//! This is a byte-exact JSON state machine — no regex, no serde re-serialization
//! (which would rewrite the whole body to change 36 bytes and could drop
//! unmodelled fields). It tracks JSON structure (a container stack, the current
//! key, in-string/escape state) to locate the depth-2 `metadata.user_id` string
//! VALUE, and only *inside that value* does it stream-match the escaped
//! `account_uuid\":\"` prefix and overwrite the following 36-byte value with the
//! new UUID. The overwrite is **same-length**, so `output.len() == input.len()`
//! always and content-length / flow-control are untouched.
//!
//! The container-stack guard is load-bearing: a stray `account_uuid` elsewhere
//! in the body (user message content, tool results) is never touched, because
//! the matcher only runs while we are inside the `metadata.user_id` value. On any
//! format surprise the body passes through unchanged — the machine fails SAFE and
//! never produces malformed JSON.
//!
//! Ported 1:1 from the JS `AccountUuidPatcher`
//! (`teamclaude/src/account-uuid-rewrite.js`); tcr buffers the whole body, so
//! this is a single pass over the full slice rather than a chunk-streaming apparatus.

use std::borrow::Cow;

/// Byte sequence of `account_uuid":"` as it appears INSIDE the (escaped)
/// `user_id` string: `account_uuid` `\` `"` `:` `\` `"`.
const PREFIX: &[u8] = br#"account_uuid\":\""#;

/// A JSON container frame: an object (`{`) or array (`[`). `name` is the key the
/// parent object opened this container under; `key` is the current member key
/// within this object; `awaiting_key` is true after `{` or `,` (the next string
/// is a key, not a value).
struct Frame {
    is_obj: bool,
    name: Option<String>,
    key: Option<String>,
    awaiting_key: bool,
}

/// Streaming byte-exact locator state (mirrors the JS `AccountUuidPatcher`, but
/// read-only: it never mutates the body — it only records the byte offset where
/// the depth-2 `metadata.user_id` `account_uuid` value begins).
struct Patcher {
    frames: Vec<Frame>,
    in_str: bool,
    esc: bool,
    reading_key: bool,
    key_buf: Vec<u8>,
    target: bool,
    match_pos: usize,
    value_start: Option<usize>,
    done: bool,
}

impl Patcher {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            in_str: false,
            esc: false,
            reading_key: false,
            key_buf: Vec::new(),
            target: false,
            match_pos: 0,
            value_start: None,
            done: false,
        }
    }

    /// Scan the whole buffer READ-ONLY, returning the byte offset of the first
    /// depth-2 `metadata.user_id` `account_uuid` value (the byte right after the
    /// escaped `account_uuid\":\"` prefix), or `None` if there is no match. The
    /// caller validates the value's extent before allocating/overwriting.
    fn locate(&mut self, body: &[u8]) -> Option<usize> {
        for (i, &b) in body.iter().enumerate() {
            self.step(b, i);
            if self.done {
                break;
            }
        }
        self.value_start
    }

    fn step(&mut self, b: u8, i: usize) {
        if self.target {
            self.target_byte(b, i);
            return;
        }

        if self.in_str {
            if self.esc {
                self.esc = false;
                if self.reading_key {
                    self.key_buf.push(b);
                }
                return;
            }
            if b == 0x5c {
                // backslash
                self.esc = true;
                return;
            }
            if b == 0x22 {
                // end of string
                self.in_str = false;
                if self.reading_key {
                    let key = String::from_utf8_lossy(&self.key_buf).into_owned();
                    if let Some(top) = self.frames.last_mut() {
                        top.key = Some(key);
                    }
                    self.key_buf.clear();
                    self.reading_key = false;
                }
                return;
            }
            if self.reading_key {
                self.key_buf.push(b);
            }
            return;
        }

        match b {
            0x7b => {
                // {
                let name = self.frames.last().and_then(|f| f.key.clone());
                self.frames.push(Frame {
                    is_obj: true,
                    name,
                    key: None,
                    awaiting_key: true,
                });
            }
            0x5b => {
                // [
                let name = self.frames.last().and_then(|f| f.key.clone());
                self.frames.push(Frame {
                    is_obj: false,
                    name,
                    key: None,
                    awaiting_key: false,
                });
            }
            0x7d | 0x5d => {
                // } ]
                self.frames.pop();
            }
            0x3a => {
                // : (key → value)
                if let Some(top) = self.frames.last_mut() {
                    top.awaiting_key = false;
                }
            }
            0x2c => {
                // , (next object member is a key)
                if let Some(top) = self.frames.last_mut() {
                    if top.is_obj {
                        top.awaiting_key = true;
                    }
                }
            }
            0x22 => {
                // string start: a key, or a value (possibly the target)
                let is_key = self
                    .frames
                    .last()
                    .map(|t| t.is_obj && t.awaiting_key)
                    .unwrap_or(false);
                if is_key {
                    self.reading_key = true;
                    self.key_buf.clear();
                    self.in_str = true;
                    self.esc = false;
                } else {
                    self.in_str = true;
                    self.esc = false;
                    self.reading_key = false;
                    let is_target = self.frames.len() == 2
                        && self
                            .frames
                            .last()
                            .map(|t| {
                                t.is_obj
                                    && t.name.as_deref() == Some("metadata")
                                    && t.key.as_deref() == Some("user_id")
                            })
                            .unwrap_or(false);
                    if is_target {
                        self.target = true;
                        self.match_pos = 0;
                    }
                }
            }
            _ => {}
        }
    }

    /// Inside the `metadata.user_id` string value: stream-match the escaped
    /// `account_uuid` prefix and record the value's start offset; an unescaped
    /// closing quote exits the target. Read-only — no bytes are mutated.
    fn target_byte(&mut self, b: u8, i: usize) {
        if self.esc {
            self.esc = false;
            self.match_byte(b, i);
            return;
        }
        if b == 0x5c {
            self.esc = true;
            self.match_byte(b, i);
            return;
        }
        if b == 0x22 {
            // end of the user_id value
            self.target = false;
            self.match_pos = 0;
            return;
        }
        self.match_byte(b, i);
    }

    fn match_byte(&mut self, b: u8, i: usize) {
        if b == PREFIX[self.match_pos] {
            self.match_pos += 1;
            if self.match_pos == PREFIX.len() {
                // The value begins at the byte right after the completed prefix.
                self.value_start = Some(i + 1);
                self.match_pos = 0;
                self.done = true; // only one account_uuid per body
            }
        } else {
            // PREFIX has no internal repeat of its first byte.
            self.match_pos = if b == PREFIX[0] { 1 } else { 0 };
        }
    }
}

/// Patch the depth-2 `metadata.user_id.account_uuid` value in `body` to
/// `new_uuid`, returning a same-length buffer.
///
/// Fails SAFE: returns [`Cow::Borrowed`] (the original, byte-for-byte untouched,
/// allocating nothing) on any surprise — `new_uuid` not exactly 36 bytes,
/// `new_uuid` carrying a JSON metachar (`"`/`\`) that would corrupt the escaped
/// string, no target match, an existing value that is not exactly 36 metachar-free
/// bytes terminated by the escaped closing quote (`\"`), or a value already equal
/// to `new_uuid`. Returns [`Cow::Owned`] ONLY when a validated 36-byte value is
/// overwritten with a different, metachar-free UUID. `output.len() == input.len()`
/// in every case, and a copy is allocated only when a real change lands.
pub fn patch_account_uuid<'a>(body: &'a [u8], new_uuid: &str) -> Cow<'a, [u8]> {
    let new_uuid = new_uuid.as_bytes();
    if new_uuid.len() != 36 {
        return Cow::Borrowed(body);
    }
    // #7: a real account UUID is hex + dashes. If the configured value carries a
    // `"` (0x22) or `\` (0x5c) it would be written raw into the escaped
    // metadata.user_id string and corrupt the JSON — refuse and pass through.
    if new_uuid.iter().any(|&b| b == 0x22 || b == 0x5c) {
        return Cow::Borrowed(body);
    }

    // Locate the target value read-only (no allocation, no mutation yet).
    let Some(start) = Patcher::new().locate(body) else {
        return Cow::Borrowed(body);
    };

    // #6: validate the existing value is EXACTLY 36 bytes, metachar-free, and
    // terminated by the escaped closing quote (`\` then `"`). A shorter (or
    // otherwise malformed) value would fail one of these checks — pass through
    // rather than blindly overwriting 36 bytes and clobbering following JSON.
    let end = start + 36;
    if end + 2 > body.len() {
        return Cow::Borrowed(body);
    }
    let value = &body[start..end];
    if value.iter().any(|&b| b == 0x22 || b == 0x5c) {
        // A metachar inside the 36-byte window means the real value ended early
        // (its closing `\"`) or is not a clean UUID — do not overwrite.
        return Cow::Borrowed(body);
    }
    if body[end] != 0x5c || body[end + 1] != 0x22 {
        return Cow::Borrowed(body);
    }
    if value == new_uuid {
        // Already the desired UUID — nothing to change, allocate nothing.
        return Cow::Borrowed(body);
    }

    // Only now that a valid, differing 36-byte value is confirmed do we copy.
    let mut out = body.to_vec();
    out[start..end].copy_from_slice(new_uuid);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "11111111-1111-1111-1111-111111111111";
    const NEW: &str = "22222222-2222-2222-2222-222222222222";

    /// Build a realistic `/v1/messages` body where `metadata.user_id` is a
    /// stringified-JSON string carrying `account_uuid` — serde does the escaping
    /// so the on-wire form is the escaped `account_uuid\":\"` the matcher keys on.
    fn body_with_user_id(inner: &str) -> Vec<u8> {
        let value = serde_json::json!({
            "model": "claude-x",
            "metadata": { "user_id": inner },
            "messages": [],
        });
        serde_json::to_vec(&value).expect("serialize body")
    }

    #[test]
    fn patches_uuid_inside_metadata_user_id() {
        let inner = format!(r#"{{"account_uuid":"{OLD}","subscriptionType":"pro"}}"#);
        let body = body_with_user_id(&inner);
        let out = patch_account_uuid(&body, NEW);

        assert!(matches!(out, Cow::Owned(_)), "a real patch must own");
        assert_ne!(out.as_ref(), body.as_slice(), "output differs from input");
        let out_str = String::from_utf8(out.into_owned()).unwrap();
        assert!(out_str.contains(NEW), "new UUID present");
        assert!(!out_str.contains(OLD), "old UUID overwritten");
    }

    #[test]
    fn leaves_stray_account_uuid_untouched() {
        // A stray account_uuid in top-level message content (depth 3, NOT the
        // depth-2 metadata.user_id) plus a real target. serde escapes both into
        // the `account_uuid\":\"` form, so ONLY the container-stack guard keeps
        // the stray from being clobbered.
        const STRAY: &str = "99999999-9999-9999-9999-999999999999";
        let inner = format!(r#"{{"account_uuid":"{OLD}"}}"#);
        let stray_content = format!(r#"log line: {{"account_uuid":"{STRAY}"}}"#);
        let value = serde_json::json!({
            "metadata": { "user_id": inner },
            "messages": [ { "role": "user", "content": stray_content } ],
        });
        let body = serde_json::to_vec(&value).unwrap();

        let out = patch_account_uuid(&body, NEW);
        let out_str = String::from_utf8(out.into_owned()).unwrap();
        assert!(out_str.contains(NEW), "target patched");
        assert!(!out_str.contains(OLD), "target old UUID gone");
        assert!(
            out_str.contains(STRAY),
            "stray account_uuid in message content untouched"
        );
    }

    #[test]
    fn length_is_always_preserved() {
        let inner = format!(r#"{{"account_uuid":"{OLD}"}}"#);
        let patched_body = body_with_user_id(&inner);
        let passthrough_body = body_with_user_id("no uuid here");

        assert_eq!(
            patch_account_uuid(&patched_body, NEW).len(),
            patched_body.len(),
            "patched output is same length"
        );
        assert_eq!(
            patch_account_uuid(&passthrough_body, NEW).len(),
            passthrough_body.len(),
            "passthrough output is same length"
        );
        assert_eq!(
            patch_account_uuid(&patched_body, "short").len(),
            patched_body.len(),
            "wrong-length UUID passthrough is same length"
        );
    }

    #[test]
    fn none_or_wrong_length_uuid_is_passthrough() {
        let inner = format!(r#"{{"account_uuid":"{OLD}"}}"#);
        let body = body_with_user_id(&inner);

        for uuid in ["", "too-short", "way-too-long-to-be-a-uuid-000000000000000"] {
            let out = patch_account_uuid(&body, uuid);
            assert!(
                matches!(out, Cow::Borrowed(_)),
                "len {} must borrow",
                uuid.len()
            );
            assert_eq!(out.as_ref(), body.as_slice(), "bytes identical");
        }
    }

    #[test]
    fn body_without_metadata_is_unchanged() {
        let value = serde_json::json!({
            "model": "claude-x",
            "messages": [ { "role": "user", "content": "hello" } ],
        });
        let body = serde_json::to_vec(&value).unwrap();
        let out = patch_account_uuid(&body, NEW);
        assert!(matches!(out, Cow::Borrowed(_)), "no metadata → borrow");
        assert_eq!(out.as_ref(), body.as_slice());
    }

    #[test]
    fn only_first_match_patched() {
        // Two account_uuid occurrences INSIDE the target string: only the first
        // 36-byte value is overwritten (the `done` flag).
        const OLD2: &str = "33333333-3333-3333-3333-333333333333";
        let inner = format!(r#"{{"account_uuid":"{OLD}","account_uuid":"{OLD2}"}}"#);
        let body = body_with_user_id(&inner);

        let out = patch_account_uuid(&body, NEW);
        let out_str = String::from_utf8(out.into_owned()).unwrap();
        assert!(out_str.contains(NEW), "first occurrence patched");
        assert!(!out_str.contains(OLD), "first old UUID gone");
        assert!(out_str.contains(OLD2), "second occurrence untouched");
    }

    #[test]
    fn escaped_prefix_exact() {
        // Escaped form (`account_uuid\":\"`) inside the target string patches.
        let matching =
            br#"{"metadata":{"user_id":"{\"account_uuid\":\"11111111-1111-1111-1111-111111111111\"}"}}"#;
        let out = patch_account_uuid(matching, NEW);
        assert!(matches!(out, Cow::Owned(_)), "escaped prefix matches");
        assert!(String::from_utf8(out.into_owned()).unwrap().contains(NEW));

        // Bare form: an UNescaped `"` after `account_uuid` closes the target
        // string before the escaped prefix can complete, so nothing is patched.
        let bare =
            br#"{"metadata":{"user_id":"account_uuid":"11111111-1111-1111-1111-111111111111"}}"#;
        let out = patch_account_uuid(bare, NEW);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "bare (unescaped) form does not match"
        );
        assert_eq!(out.as_ref(), bare.as_slice());
    }

    // #6 — a value SHORTER than 36 bytes must NOT be overwritten: the old code
    // blindly wrote 36 bytes, clobbering the value's closing `\"` and following
    // JSON → malformed body → Anthropic 400. Fail safe instead.
    #[test]
    fn sub_36_value_is_passthrough_not_clobbered() {
        let inner = r#"{"account_uuid":"abc","subscriptionType":"pro"}"#;
        let body = body_with_user_id(inner);
        let out = patch_account_uuid(&body, NEW);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "short value must pass through, not be clobbered"
        );
        assert_eq!(
            out.as_ref(),
            body.as_slice(),
            "body byte-for-byte unchanged"
        );
    }

    // #6 — a value LONGER than 36 bytes is likewise not a clean UUID: the escaped
    // closing quote does not fall at offset 36, so pass through unchanged.
    #[test]
    fn over_36_value_is_passthrough() {
        let long = "1".repeat(40);
        let inner = format!(r#"{{"account_uuid":"{long}"}}"#);
        let body = body_with_user_id(&inner);
        let out = patch_account_uuid(&body, NEW);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            ">36 value must pass through"
        );
        assert_eq!(out.as_ref(), body.as_slice());
    }

    // #6 (regression) — a normal, exactly-36-byte value IS still patched.
    #[test]
    fn exactly_36_value_still_patched() {
        let inner = format!(r#"{{"account_uuid":"{OLD}"}}"#);
        let body = body_with_user_id(&inner);
        let out = patch_account_uuid(&body, NEW);
        assert!(matches!(out, Cow::Owned(_)), "36-byte value must patch");
        assert_eq!(out.len(), body.len(), "same length");
        let out_str = String::from_utf8(out.into_owned()).unwrap();
        assert!(out_str.contains(NEW), "new UUID present");
        assert!(!out_str.contains(OLD), "old UUID overwritten");
    }

    // #7 — a 36-byte `new_uuid` carrying a JSON metachar (`"` or `\`) would be
    // written raw into the escaped string and corrupt it. Reject → pass through.
    #[test]
    fn metachar_new_uuid_is_rejected() {
        let inner = format!(r#"{{"account_uuid":"{OLD}"}}"#);
        let body = body_with_user_id(&inner);

        // 36 bytes, but one is a double-quote.
        let with_quote = format!("{}\"", "2".repeat(35));
        assert_eq!(with_quote.len(), 36);
        // 36 bytes, but one is a backslash.
        let with_backslash = format!("{}\\", "2".repeat(35));
        assert_eq!(with_backslash.len(), 36);

        for bad in [with_quote, with_backslash] {
            let out = patch_account_uuid(&body, &bad);
            assert!(
                matches!(out, Cow::Borrowed(_)),
                "metachar UUID must be rejected"
            );
            assert_eq!(out.as_ref(), body.as_slice(), "body unchanged");
        }
    }

    // #14 — a non-matching body (no metadata.user_id account_uuid) returns
    // Cow::Borrowed, allocating no changed buffer.
    #[test]
    fn non_matching_body_borrows_without_copy() {
        let body = body_with_user_id("no account uuid here at all");
        let out = patch_account_uuid(&body, NEW);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "non-matching body must borrow, not allocate a changed copy"
        );
        assert_eq!(out.as_ref(), body.as_slice(), "bytes identical to input");
    }
}
