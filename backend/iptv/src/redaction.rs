//! What must never reach a log line, a stored error, or a debug dump.
//!
//! Provider URLs carry credentials in three places at once — userinfo (`user:pass@host`),
//! query parameters (`token=`, `mac=`, `password=`), and Stalker's base64 `cmd` — and this
//! crate had three unrelated answers to that: `safe_stalker_url` for error URLs, an inline
//! key list inside the debug-dump writer, and `sanitize_sensitive_info` from `shared` on
//! the Xtream side. The sibling `tuliprox-media-server` crate already has one module for
//! the same job; this is its counterpart, so the key list is defined once and every path
//! that renders a provider string goes through it.

use shared::utils::sanitize_sensitive_info;

const REDACTED: &str = "[redacted]";

/// Keys whose values are secret wherever they appear — as a query parameter, as a JSON
/// object key, or as a cookie name. Matched case-insensitively, as a substring for JSON
/// keys (portals prefix and suffix them freely) and whole-word for query parameters.
pub const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "credential",
    "secret",
    "auth",
    "email",
    "phone",
    "account",
    "login",
    "username",
    "mac",
    "cmd",
    "url",
];

/// Query parameters that carry a credential. A narrower list than [`SENSITIVE_KEYS`]:
/// dropping the whole query string is the norm for URLs, so this is for the cases where
/// the surrounding text has to survive.
pub const SENSITIVE_QUERY_KEYS: &[&str] =
    &["token", "password", "passwd", "mac", "login", "username", "auth", "authorization", "api_key", "apikey", "cmd"];

/// True when a JSON object key or cookie name should have its value replaced.
///
/// Substring rather than equality: portals emit `stb_token`, `account_id`, `user_email`
/// and a dozen other decorations of the same field.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|sensitive| lowered.contains(sensitive))
}

/// Render a provider URL safely: userinfo, query and fragment removed.
///
/// Anything that is not an `http`/`https` URL is refused outright rather than echoed —
/// a value that failed to parse is exactly the one most likely to be a raw credential.
#[must_use]
pub fn safe_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return "[redacted invalid URL]".to_string();
    };
    if !matches!(url.scheme(), "http" | "https") || url.set_username("").is_err() || url.set_password(None).is_err() {
        return "[redacted invalid URL]".to_string();
    }
    url.set_query(None);
    url.set_fragment(None);
    url.into()
}

/// Redact credentials from free text that has to stay readable — a log message quoting a
/// URL, an error snippet. Keeps the structure, replaces the values.
#[must_use]
pub fn redact_text(value: &str) -> String { redact_query_like_tokens(&sanitize_sensitive_info(value)) }

/// Replace the values of `key=` / `key:` pairs, leaving everything else intact.
fn redact_query_like_tokens(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    'outer: while !rest.is_empty() {
        for key in SENSITIVE_QUERY_KEYS {
            let Some(separator) = matched_key_separator(rest, key) else { continue };
            result.push_str(&rest[..key.len()]);
            result.push(separator);
            result.push_str(REDACTED);
            rest = &rest[key.len() + separator.len_utf8()..];
            rest = skip_value(rest);
            continue 'outer;
        }
        let Some(ch) = rest.chars().next() else { break };
        result.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    result
}

/// The separator following `key` at the start of `text`, when `text` starts with that key
/// used as a `key=value` pair rather than as part of a longer word.
fn matched_key_separator(text: &str, key: &str) -> Option<char> {
    let candidate = text.get(..key.len())?;
    if !candidate.eq_ignore_ascii_case(key) {
        return None;
    }
    let separator = text.get(key.len()..)?.chars().next()?;
    matches!(separator, '=' | ':').then_some(separator)
}

/// Consume the value that follows a redacted key, honouring quotes.
fn skip_value(mut rest: &str) -> &str {
    let quote = rest.chars().next().filter(|ch| matches!(ch, '"' | '\''));
    if let Some(quote) = quote {
        rest = &rest[quote.len_utf8()..];
        while let Some(ch) = rest.chars().next() {
            rest = &rest[ch.len_utf8()..];
            if ch == quote {
                break;
            }
        }
        return rest;
    }
    while let Some(ch) = rest.chars().next() {
        if matches!(ch, '&' | ' ' | '\n' | '\r' | '\t' | '"' | '\'') {
            break;
        }
        rest = &rest[ch.len_utf8()..];
    }
    rest
}

/// Replace every sensitive value in a JSON document, in place.
pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, field) in fields {
                if is_sensitive_key(key) {
                    *field = serde_json::Value::String(REDACTED.to_string());
                } else {
                    redact_json(field);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_json),
        _ => {}
    }
}

/// Sanitize a string for safe use in file/directory path components.
///
/// If `allow_dots` is true, '.' characters are preserved (e.g. for hostnames);
/// otherwise they are replaced with '_'.
#[must_use]
pub fn sanitize_path_component(value: &str, allow_dots: bool) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            '.' if allow_dots => '.',
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{is_sensitive_key, redact_json, redact_text, safe_url, sanitize_path_component};

    #[test]
    fn sanitize_path_component_preserves_allowed_chars() {
        assert_eq!(sanitize_path_component("abc-XYZ_123.host/extra", true), "abc-XYZ_123.host_extra");
        assert_eq!(sanitize_path_component("abc-XYZ_123.host/extra", false), "abc-XYZ_123_host_extra");
    }

    #[test]
    fn safe_url_drops_userinfo_query_and_fragment() {
        let safe = safe_url("https://user:pass@portal.example/server/load.php?token=secret&mac=00:11:22#frag");
        assert_eq!(safe, "https://portal.example/server/load.php");
    }

    #[test]
    fn a_value_that_is_not_a_url_is_refused_rather_than_echoed() {
        assert_eq!(safe_url("not a url at all"), "[redacted invalid URL]");
        assert_eq!(safe_url("file:///etc/passwd"), "[redacted invalid URL]");
    }

    #[test]
    fn sensitive_keys_match_the_decorations_portals_actually_emit() {
        for key in ["token", "stb_token", "Account_Number", "user_email", "MAC", "cmd"] {
            assert!(is_sensitive_key(key), "{key} should be treated as sensitive");
        }
        for key in ["title", "category_id", "number", "genre"] {
            assert!(!is_sensitive_key(key), "{key} should not be redacted");
        }
    }

    #[test]
    fn redact_text_keeps_the_structure_and_drops_the_values() {
        let redacted = redact_text("GET /load.php?token=abc123&title=News&mac=00:1A:79:DE:AD:BE failed");
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("00:1A:79:DE:AD:BE"));
        assert!(redacted.contains("title=News"));
        assert!(redacted.contains("failed"));
    }

    #[test]
    fn redact_text_does_not_corrupt_non_ascii() {
        let redacted = redact_text("https://portal.example/épisode?token=sëcret&title=café");
        assert!(!redacted.contains("sëcret"));
        assert!(redacted.contains("épisode"));
        assert!(redacted.contains("title=café"));
    }

    #[test]
    fn redact_json_walks_nested_documents() {
        let mut value = serde_json::json!({
            "js": {"token": "secret", "channels": [{"cmd": "ffmpeg http://host/x", "title": "News"}]}
        });
        redact_json(&mut value);
        let rendered = value.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("ffmpeg"));
        assert!(rendered.contains("News"));
    }
}
