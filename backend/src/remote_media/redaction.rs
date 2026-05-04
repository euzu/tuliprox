use reqwest::header::{HeaderMap, HeaderName};
use shared::utils::sanitize_sensitive_info;

const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "token",
    "access_token",
    "x-plex-token",
    "x_emby_token",
    "x-emby-token",
    "api_key",
    "apikey",
    "password",
    "passwd",
    "authorization",
    "auth",
];

const SENSITIVE_HEADER_NAMES: &[&str] = &[
    "authorization",
    "x-emby-token",
    "x-mediabrowser-token",
    "x-plex-token",
    "cookie",
    "set-cookie",
];

pub fn is_sensitive_remote_header(name: &HeaderName) -> bool {
    let value = name.as_str().to_ascii_lowercase();
    SENSITIVE_HEADER_NAMES.iter().any(|sensitive| value == *sensitive)
}

pub fn redact_remote_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let rendered = if is_sensitive_remote_header(name) {
                "<redacted>".to_string()
            } else {
                value
                    .to_str()
                    .map(redact_remote_text)
                    .unwrap_or_else(|_| "<non-utf8>".to_string())
            };
            (name.as_str().to_string(), rendered)
        })
        .collect()
}

pub fn redact_remote_text(value: &str) -> String {
    let sanitized = sanitize_sensitive_info(value);
    redact_query_like_tokens(&sanitized)
}

fn redact_query_like_tokens(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let matched = SENSITIVE_QUERY_KEYS.iter().find_map(|key| {
            let remaining = &value[i..];
            if remaining.len() < key.len() + 1 {
                return None;
            }
            let candidate = &remaining[..key.len()];
            let separator = remaining.as_bytes().get(key.len()).copied();
            if candidate.eq_ignore_ascii_case(key) && matches!(separator, Some(b'=') | Some(b':')) {
                Some((*key, separator.unwrap() as char))
            } else {
                None
            }
        });

        if let Some((key, separator)) = matched {
            result.push_str(key);
            result.push(separator);
            result.push_str("<redacted>");
            i += key.len() + 1;
            while i < bytes.len() && !matches!(bytes[i], b'&' | b' ' | b'\n' | b'\r' | b'\t' | b'\"' | b'\'') {
                i += 1;
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, AUTHORIZATION};

    #[test]
    fn redacts_remote_query_tokens_case_insensitively() {
        let redacted = redact_remote_text(
            "https://media.example.invalid/video?X-Plex-Token=secret-token&api_key=secret-key&safe=value",
        );

        assert!(!redacted.contains("secret-token"));
        assert!(!redacted.contains("secret-key"));
        assert!(redacted.contains("X-Plex-Token=<redacted>") || redacted.contains("x-plex-token=<redacted>"));
        assert!(redacted.contains("api_key=<redacted>"));
        assert!(redacted.contains("safe=value"));
    }

    #[test]
    fn redacts_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert("content-type", HeaderValue::from_static("video/mp4"));

        let rendered = redact_remote_headers(&headers);

        assert!(rendered.iter().any(|(name, value)| name == "authorization" && value == "<redacted>"));
        assert!(rendered.iter().any(|(name, value)| name == "content-type" && value == "video/mp4"));
    }
}
