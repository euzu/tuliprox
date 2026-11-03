//! Secret redaction for MCP tool results.
//!
//! `get_config` returns the effective configuration, including provider
//! credentials and the api-proxy user list. A MCP client is an LLM that may
//! forward what it reads, so the result is scrubbed here instead of being
//! trusted to the REST layer (which only masks the messaging channels).
//!
//! Limits of this approach: redaction is by key name and by URL shape. A
//! secret stored under a key whose name carries none of the patterns, a
//! secret embedded in free text, a numeric or boolean secret, and credentials
//! carried in a URL path (for example Xtream `/live/user/pass/1.ts`) are NOT
//! detected. Anything that does not parse as JSON is withheld entirely.

use serde_json::{json, Value};
use shared::model::REDACTED_SECRET;

/// Largest REST body the redactor accepts; a bigger one is withheld rather than
/// passed through unredacted.
pub(super) const REDACT_BODY_LIMIT: usize = 16 * 1024 * 1024;

pub(super) const REDACTION_FAILED: &str = "The result could not be parsed for redaction and was withheld.";
pub(super) const REDACTION_TOO_LARGE: &str = "The result is too large to redact and was withheld.";

/// Lowercase key fragments that mark a value as a secret.
const SECRET_KEY_PARTS: [&str; 9] =
    ["password", "passwd", "secret", "token", "api_key", "apikey", "api-key", "authorization", "cookie"];
/// Exact lowercase keys that hold a credential identity.
const CREDENTIAL_KEYS: [&str; 2] = ["username", "user"];
/// URL query parameters whose value is a credential.
const SENSITIVE_QUERY_KEYS: [&str; 7] = ["password", "pass", "username", "user", "token", "api_key", "apikey"];

/// Redacts a JSON REST body. Never returns the raw input: a body that is not
/// valid JSON, or too large, is replaced by a fixed text.
pub(super) fn redact_json_body(body: &[u8]) -> String {
    if body.len() > REDACT_BODY_LIMIT {
        return REDACTION_TOO_LARGE.to_string();
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return REDACTION_FAILED.to_string();
    };
    summarize_api_proxy_users(&mut value);
    redact_value(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| REDACTION_FAILED.to_string())
}

/// The api-proxy user list is dropped; only the target names and the number of
/// accounts per target remain.
fn summarize_api_proxy_users(root: &mut Value) {
    let Some(users) = root.get_mut("api_proxy").and_then(|api_proxy| api_proxy.get_mut("user")) else {
        return;
    };
    let Some(targets) = users.as_array() else {
        return;
    };
    let summary: Vec<Value> = targets
        .iter()
        .map(|target| {
            json!({
                "target": target.get("target").cloned().unwrap_or(Value::Null),
                "credentials": target.get("credentials").and_then(Value::as_array).map_or(0, Vec::len),
            })
        })
        .collect();
    *users = Value::Array(summary);
}

fn is_secret_key(lowercase_key: &str) -> bool { SECRET_KEY_PARTS.iter().any(|part| lowercase_key.contains(part)) }

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                let key = key.to_ascii_lowercase();
                let secret = match entry {
                    Value::String(text) => {
                        !text.is_empty() && (is_secret_key(&key) || CREDENTIAL_KEYS.contains(&key.as_str()))
                    }
                    // Numbers and booleans (e.g. `token_ttl_mins`) are settings, not secrets.
                    Value::Array(_) | Value::Object(_) => is_secret_key(&key),
                    _ => false,
                };
                if secret {
                    *entry = Value::String(REDACTED_SECRET.to_string());
                } else {
                    redact_value(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        Value::String(text) => {
            if let Some(redacted) = redact_url_text(text) {
                *text = redacted;
            }
        }
        _ => {}
    }
}

/// Masks the userinfo password and credential query parameters of a URL-shaped
/// string. Works on the text so that unusual URLs (templates, placeholders)
/// are covered too. `None` when nothing needed redaction.
fn redact_url_text(text: &str) -> Option<String> {
    let (head, tail) = match text.split_once('?') {
        Some((head, tail)) => (head, Some(tail)),
        None => (text, None),
    };
    let mut changed = false;
    let mut result = redact_userinfo(head).map_or_else(
        || head.to_string(),
        |redacted| {
            changed = true;
            redacted
        },
    );
    if let Some(tail) = tail {
        let (query, fragment) = match tail.split_once('#') {
            Some((query, fragment)) => (query, Some(fragment)),
            None => (tail, None),
        };
        result.push('?');
        let pairs: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((key, value))
                    if !value.is_empty() && SENSITIVE_QUERY_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k)) =>
                {
                    changed = true;
                    format!("{key}={REDACTED_SECRET}")
                }
                _ => pair.to_string(),
            })
            .collect();
        result.push_str(&pairs.join("&"));
        if let Some(fragment) = fragment {
            result.push('#');
            result.push_str(fragment);
        }
    }
    changed.then_some(result)
}

fn redact_userinfo(head: &str) -> Option<String> {
    let authority_start = head.find("://")? + 3;
    let authority_end = head[authority_start..].find('/').map_or(head.len(), |idx| authority_start + idx);
    let authority = &head[authority_start..authority_end];
    let at = authority.rfind('@')?;
    let colon = authority[..at].find(':')?;
    let password_start = authority_start + colon + 1;
    let password_end = authority_start + at;
    if password_start == password_end {
        return None;
    }
    Some(format!("{}{}{}", &head[..password_start], REDACTED_SECRET, &head[password_end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(value: &Value) -> Value { serde_json::from_str(&redact_json_body(value.to_string().as_bytes())).unwrap() }

    #[test]
    fn nested_secret_keys_are_masked_whatever_their_case_or_type() {
        let redacted = redact(&json!({
            "config": {
                "web_ui": { "auth": { "secret": "s3", "token_ttl_mins": 30, "password_hash": "h" } },
                "messaging": { "telegram": { "bot_token": "t" }, "rest": { "Authorization": "Bearer x" } },
                "proxy": { "url": "http://p", "Username": "u", "PASSWORD": "pw" },
                "other": { "api_key": ["a", "b"], "apikey": { "k": "v" }, "empty_password": "" }
            }
        }));
        let config = &redacted["config"];
        assert_eq!(config["web_ui"]["auth"]["secret"], REDACTED_SECRET);
        assert_eq!(config["web_ui"]["auth"]["password_hash"], REDACTED_SECRET);
        assert_eq!(config["web_ui"]["auth"]["token_ttl_mins"], 30);
        assert_eq!(config["messaging"]["telegram"]["bot_token"], REDACTED_SECRET);
        assert_eq!(config["messaging"]["rest"]["Authorization"], REDACTED_SECRET);
        assert_eq!(config["proxy"]["url"], "http://p");
        assert_eq!(config["proxy"]["Username"], REDACTED_SECRET);
        assert_eq!(config["proxy"]["PASSWORD"], REDACTED_SECRET);
        assert_eq!(config["other"]["api_key"], REDACTED_SECRET);
        assert_eq!(config["other"]["apikey"], REDACTED_SECRET);
        assert_eq!(config["other"]["empty_password"], "");
    }

    #[test]
    fn input_credentials_are_masked_inside_sources() {
        let redacted = redact(&json!({
            "sources": { "inputs": [{ "name": "in", "url": "http://h/", "username": "alice", "password": "pw" }] }
        }));
        let input = &redacted["sources"]["inputs"][0];
        assert_eq!(input["name"], "in");
        assert_eq!(input["username"], REDACTED_SECRET);
        assert_eq!(input["password"], REDACTED_SECRET);
    }

    #[test]
    fn query_credentials_in_any_string_are_masked() {
        let redacted = redact(&json!({
            "sources": { "inputs": [
                { "url": "http://h/get.php?username=x&password=y&type=m3u" },
                { "url": "http://h/p?User=x&PASS=y&Token=t&api_key=k#frag" },
                { "url": "http://h/{template}?username={username}" }
            ] }
        }));
        let urls: Vec<&str> =
            redacted["sources"]["inputs"].as_array().unwrap().iter().map(|i| i["url"].as_str().unwrap()).collect();
        assert_eq!(urls[0], format!("http://h/get.php?username={REDACTED_SECRET}&password={REDACTED_SECRET}&type=m3u"));
        assert_eq!(
            urls[1],
            format!("http://h/p?User={REDACTED_SECRET}&PASS={REDACTED_SECRET}&Token={REDACTED_SECRET}&api_key={REDACTED_SECRET}#frag")
        );
        assert!(!urls[2].contains("{username}"), "{}", urls[2]);
    }

    #[test]
    fn url_userinfo_password_is_masked() {
        let redacted =
            redact(&json!({ "a": "http://u:p@host:8080/path?x=1", "b": "http://u@host/", "c": "http://host/a@b:c" }));
        assert_eq!(redacted["a"], format!("http://u:{REDACTED_SECRET}@host:8080/path?x=1"));
        assert_eq!(redacted["b"], "http://u@host/");
        assert_eq!(redacted["c"], "http://host/a@b:c");
    }

    #[test]
    fn plain_text_is_left_alone() {
        let redacted = redact(&json!({ "note": "what? yes=1 & no", "list": ["a", 1, true, null] }));
        assert_eq!(redacted["note"], "what? yes=1 & no");
        assert_eq!(redacted["list"], json!(["a", 1, true, null]));
    }

    #[test]
    fn api_proxy_users_are_reduced_to_target_and_count() {
        let redacted = redact(&json!({
            "api_proxy": {
                "server": [{ "name": "default", "username": "su", "password": "sp" }],
                "user": [
                    { "target": "t1", "credentials": [{ "username": "a", "password": "b", "token": "c" }, { "username": "d", "password": "e" }] },
                    { "target": "t2", "credentials": [] }
                ]
            }
        }));
        assert_eq!(
            redacted["api_proxy"]["user"],
            json!([{ "target": "t1", "credentials": 2 }, { "target": "t2", "credentials": 0 }])
        );
        assert_eq!(redacted["api_proxy"]["server"][0]["password"], REDACTED_SECRET);
        let text = redacted.to_string();
        for leaked in ["\"a\"", "\"b\"", "\"c\"", "\"e\""] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
    }

    #[test]
    fn unparsable_or_oversized_bodies_are_withheld() {
        assert_eq!(redact_json_body(b"password=hunter2"), REDACTION_FAILED);
        assert_eq!(redact_json_body(b""), REDACTION_FAILED);
        assert_eq!(redact_json_body(&vec![b' '; REDACT_BODY_LIMIT + 1]), REDACTION_TOO_LARGE);
    }
}
