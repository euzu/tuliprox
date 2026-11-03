use crate::{
    defaults::{DASH_EXT, DASH_EXT_FRAGMENT, DASH_EXT_QUERY, HLS_EXT, HLS_EXT_FRAGMENT, HLS_EXT_QUERY},
    error::TuliproxError,
    utils::CONSTANTS,
};
use std::{borrow::Cow, sync::atomic::Ordering};
use url::Url;

pub const PROVIDER_SCHEME_PREFIX: &str = "provider://";
pub const BATCH_SCHEME_PREFIX: &str = "batch://";
pub const FILE_SCHEME_PREFIX: &str = "file://";

pub const CONTENT_TYPE_JSON: &str = "application/json";
pub const CONTENT_TYPE_CBOR: &str = "application/cbor";
pub const ACCEPT_PREFER_CBOR: &str = "application/cbor, application/json;q=0.9";
pub const HEADER_IF_MATCH: &str = "If-Match";
pub const HEADER_CONFIG_MAIN_REVISION: &str = "X-Tuliprox-Main-Revision";
pub const HEADER_CONFIG_SOURCES_REVISION: &str = "X-Tuliprox-Sources-Revision";
pub const HEADER_CONFIG_API_PROXY_REVISION: &str = "X-Tuliprox-ApiProxy-Revision";

pub fn set_sanitize_sensitive_info(value: bool) { CONSTANTS.sanitize.store(value, Ordering::Relaxed); }
pub fn is_sanitize_sensitive_info_enabled() -> bool { CONSTANTS.sanitize.load(Ordering::Relaxed) }
pub fn sanitize_sensitive_info(query: &str) -> Cow<'_, str> {
    // Local file URLs carry no host or credentials; masking would only mangle the path.
    if !is_sanitize_sensitive_info_enabled() || query.starts_with(FILE_SCHEME_PREFIX) {
        return Cow::Borrowed(query);
    }

    let mut result = query.to_owned();

    for (re, replacement) in &[
        (&CONSTANTS.re_credentials, "$1***"),
        (&CONSTANTS.re_ipv4, "$1***"),
        (&CONSTANTS.re_ipv6, "$1***"),
        (&CONSTANTS.re_stream_url, "$1***/$2/***/"),
        (&CONSTANTS.re_url, "$1***/$2"),
        (&CONSTANTS.re_password, "$1***"),
    ] {
        result = re.replace_all(&result, *replacement).into_owned();
    }
    Cow::Owned(result)
}

/// Extracts the file extension from a URL path (query and fragment stripped).
/// Returns the extension **prefixed with a dot** (e.g., ".m3u8").
pub fn extract_extension_from_url(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    let end = bytes.iter().position(|b| matches!(*b, b'?' | b'#')).unwrap_or(bytes.len());

    let segment_start = bytes[..end].iter().rposition(|b| *b == b'/').map_or(0, |idx| idx + 1);
    if segment_start >= end {
        return None;
    }

    let segment = &bytes[segment_start..end];
    let dot = segment.iter().rposition(|b| *b == b'.')?;
    let extension_start = segment_start + dot + 1;
    let extension = &input[extension_start..end];

    if extension.is_empty() || extension.len() > 4 || extension.eq_ignore_ascii_case("php") {
        return None;
    }

    Some(&input[extension_start - 1..end])
}

/// Upper bound for a container extension. Real containers are `mkv`, `mp4`, `m3u8`, `webm`; anything
/// longer is not an extension and must not reach a client that appends it to a stream URL.
const MAX_CONTAINER_EXTENSION_LEN: usize = 8;

/// Canonical container extension: lowercase, without the leading dot.
///
/// Providers report the value in different shapes (`mp4`, `.MP4`, ` mp4 `) and some send a nullish
/// placeholder (`null`, `~`). Clients build their playback URL as `<stream_id>.<extension>`, so a
/// placeholder or a malformed token is worse than no value at all. Returns `None` for those.
///
/// The return value borrows from `raw` when it is already canonical, so resolving an extension for a
/// large VOD catalogue does not allocate a temporary `String` per item.
pub fn normalize_container_extension(raw: &str) -> Option<Cow<'_, str>> {
    let trimmed = raw.trim();
    let without_dot = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if without_dot.is_empty()
        || without_dot.len() > MAX_CONTAINER_EXTENSION_LEN
        || super::is_nullish(without_dot)
        || !without_dot.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    if without_dot.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Some(Cow::Owned(without_dot.to_ascii_lowercase()))
    } else {
        Some(Cow::Borrowed(without_dot))
    }
}

/// Resolve the container extension published for one item.
///
/// Order: the value the provider reported, then the extension carried by the item URL, then the
/// input's configured default, and finally [`DEFAULT_CONTAINER_EXTENSION`]. The last two steps are
/// assumptions, so they never override a value the provider or the URL already states. The result is
/// always canonical and never empty: clients build `<stream_id>.<container_extension>` and drop items
/// without one, while a nullish placeholder would make them request `.null`.
pub fn resolve_container_extension<'a>(stored: &'a str, url: &'a str, configured: Option<&'a str>) -> Cow<'a, str> {
    if let Some(extension) = normalize_container_extension(stored) {
        return extension;
    }
    if let Some(extension) = extract_extension_from_url(url).and_then(normalize_container_extension) {
        return extension;
    }
    configured
        .and_then(normalize_container_extension)
        .unwrap_or(Cow::Borrowed(crate::defaults::DEFAULT_CONTAINER_EXTENSION))
}

pub fn is_hls_url(url: &str) -> bool {
    let lc_url = url.to_lowercase();
    lc_url.ends_with(HLS_EXT) || lc_url.contains(HLS_EXT_QUERY) || lc_url.contains(HLS_EXT_FRAGMENT)
}

pub fn is_dash_url(url: &str) -> bool {
    let lc_url = url.to_lowercase();
    lc_url.ends_with(DASH_EXT) || lc_url.contains(DASH_EXT_QUERY) || lc_url.contains(DASH_EXT_FRAGMENT)
}

pub fn replace_url_extension(url: &str, new_ext: &str) -> String {
    let ext = new_ext.strip_prefix('.').unwrap_or(new_ext); // Remove leading dot if exists

    // Split URL into the base part (domain and path) and the suffix (query/fragment)
    let (base_url, suffix) = match url.find(['?', '#'].as_ref()) {
        Some(pos) => (&url[..pos], &url[pos..]), // Base URL and suffix
        None => (url, ""),                       // No query or fragment
    };

    // Find the last '/' in the base URL, which marks the end of the domain and the beginning of the file path
    if let Some(last_slash_pos) = base_url.rfind('/') {
        if last_slash_pos < 9 {
            // protocol slash, return url as is
            return url.to_string();
        }
        let (path_part, file_name_with_extension) = base_url.split_at(last_slash_pos + 1);
        // Find the last dot in the file name to replace the extension
        if let Some(dot_pos) = file_name_with_extension.rfind('.') {
            return format!(
                "{path_part}{}.{ext}{suffix}",
                &file_name_with_extension[..dot_pos], // Keep the name part before the dot
            );
        }
    }

    // If no extension is found, add the new extension to the base URL
    format!("{base_url}.{ext}{suffix}")
}

pub fn get_credentials_from_url(url: &Url) -> (Option<String>, Option<String>) {
    let mut username = None;
    let mut password = None;
    for (key, value) in url.query_pairs() {
        if key.eq("username") {
            username = Some(value.to_string());
        } else if key.eq("password") {
            password = Some(value.to_string());
        }
    }
    (username, password)
}

pub fn get_credentials_from_url_str(url_with_credentials: &str) -> (Option<String>, Option<String>) {
    if let Ok(url) = Url::parse(url_with_credentials) {
        get_credentials_from_url(&url)
    } else {
        (None, None)
    }
}

pub fn get_base_url_from_str(url: &str) -> Option<String> {
    if let Ok(url) = Url::parse(url) {
        Some(url.origin().ascii_serialization())
    } else {
        None
    }
}

pub fn concat_path(first: &str, second: &str) -> String {
    let first = first.trim_end_matches('/');
    let second = second.trim_start_matches('/');
    match (first.is_empty(), second.is_empty()) {
        (true, true) => String::new(),
        (true, false) => second.to_string(),
        (false, true) => first.to_string(),
        (false, false) => format!("{first}/{second}"),
    }
}

pub fn concat_path_leading_slash(first: &str, second: &str) -> String {
    let path = concat_path(first, second);
    if path.is_empty() {
        return path;
    }
    let path = path.trim_start_matches('/');
    format!("/{path}")
}

/// Internal helper to parse the provider URL into (host, `path_and_query`)
pub fn parse_provider_scheme_url_parts(stream_url: &str) -> Result<(&str, &str), TuliproxError> {
    let rest = stream_url.strip_prefix(PROVIDER_SCHEME_PREFIX).ok_or_else(|| {
        TuliproxError::Config(format!("Not a provider URL: '{}'", sanitize_sensitive_info(stream_url)))
    })?;

    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    if host.is_empty() {
        return Err(TuliproxError::Config(format!(
            "Provider host is empty in URL: '{}'",
            sanitize_sensitive_info(stream_url)
        )));
    }

    Ok((host, path))
}

#[cfg(test)]
mod tests {
    use super::{
        extract_extension_from_url, normalize_container_extension, resolve_container_extension,
        sanitize_sensitive_info, set_sanitize_sensitive_info,
    };
    use crate::defaults::DEFAULT_CONTAINER_EXTENSION;

    #[test]
    fn normalize_container_extension_accepts_canonical_forms() {
        assert_eq!(normalize_container_extension("mp4").as_deref(), Some("mp4"));
        assert_eq!(normalize_container_extension(".MKV").as_deref(), Some("mkv"));
        assert_eq!(normalize_container_extension("  mkv ").as_deref(), Some("mkv"));
        assert_eq!(normalize_container_extension("m3u8").as_deref(), Some("m3u8"));
    }

    #[test]
    fn normalize_container_extension_rejects_placeholders_and_noise() {
        for rejected in ["", "   ", ".", "~", "null", "NULL", "tar.gz", "mp 4", "mkv?token=x", "abcdefghi"] {
            assert!(normalize_container_extension(rejected).is_none(), "{rejected} must be rejected");
        }
    }

    #[test]
    fn resolve_container_extension_prefers_the_stored_value() {
        assert_eq!(
            resolve_container_extension("avi", "http://provider.example/movie/1.mkv", Some("mkv")).as_ref(),
            "avi"
        );
        // Surrounding whitespace and case are not part of the token.
        assert_eq!(resolve_container_extension(" AVI ", "", None).as_ref(), "avi");
    }

    #[test]
    fn resolve_container_extension_ignores_a_stored_placeholder() {
        let url = "http://provider.example/movie/1.mkv";
        assert_eq!(resolve_container_extension("null", url, Some("avi")).as_ref(), "mkv");
        assert_eq!(resolve_container_extension("~", "", Some("avi")).as_ref(), "avi");
    }

    #[test]
    fn resolve_container_extension_falls_back_to_the_url_then_the_configured_default() {
        assert_eq!(resolve_container_extension("", "http://provider.example/movie/1.mkv", Some("avi")).as_ref(), "mkv");
        assert_eq!(resolve_container_extension("", "http://provider.example/movie/1", Some("avi")).as_ref(), "avi");
    }

    #[test]
    fn resolve_container_extension_ends_at_the_builtin_default() {
        assert_eq!(resolve_container_extension("", "", None).as_ref(), DEFAULT_CONTAINER_EXTENSION);
        // A configured placeholder is not a value either, so the built-in default answers.
        assert_eq!(resolve_container_extension("", "", Some("null")).as_ref(), DEFAULT_CONTAINER_EXTENSION);
    }

    #[test]
    fn resolve_container_extension_normalizes_the_configured_default() {
        assert_eq!(resolve_container_extension("", "", Some(".MKV")).as_ref(), "mkv");
    }

    #[test]
    fn extract_extension_from_url_ignores_query_and_fragment() {
        let cases = [
            ("http://provider.example/live/video.m3u8?token=abc", Some(".m3u8")),
            ("http://provider.example/live/video.m3u8?token=abc#frag", Some(".m3u8")),
            ("http://provider.example/live/video.m3u8?next=http://cdn.example/segment.ts", Some(".m3u8")),
            ("http://provider.example/live/video.ts?token=abc", Some(".ts")),
            ("http://provider.example/live/video.pHp?token=abc", None),
        ];

        for (input, expected) in cases {
            assert_eq!(extract_extension_from_url(input), expected, "input: {input}");
        }
    }

    #[test]
    fn sanitize_sensitive_info_masks_xtream_path_credentials_for_all_supported_schemes() {
        let previous = super::is_sanitize_sensitive_info_enabled();
        set_sanitize_sensitive_info(true);

        for scheme in ["http", "https", "provider", "batch"] {
            let input = format!("{scheme}://example/live/myuser/mypass/15373.ts");
            let expected = format!("{scheme}://***/live/***/15373.ts");
            assert_eq!(sanitize_sensitive_info(&input), expected);
        }

        set_sanitize_sensitive_info(previous);
    }

    #[test]
    fn sanitize_sensitive_info_masks_path_credentials_for_all_xtream_contexts() {
        // Regression: only `live|video|movie|series|m3u-stream|resource` were originally
        // listed in `re_stream_url`, so URLs like `/timeshift/{user}/{pass}/...` leaked
        // the username and password into log lines. The regex was extended to also
        // cover `timeshift`, `streaming`, `xtream`, and `timeshift.php`.
        let previous = super::is_sanitize_sensitive_info_enabled();
        set_sanitize_sensitive_info(true);

        let cases: &[(&str, &str)] = &[
            (
                "http://example/timeshift/myuser/mypass/3/2026-06-17:14-00/449.ts",
                "http://***/timeshift/***/3/2026-06-17:14-00/449.ts",
            ),
            ("http://example/streaming/myuser/mypass/123.ts", "http://***/streaming/***/123.ts"),
            ("http://example/xtream/myuser/mypass/123.ts", "http://***/xtream/***/123.ts"),
            ("http://example/timeshift.php/myuser/mypass/123.ts", "http://***/timeshift.php/***/123.ts"),
            ("https://provider.example/movie/myuser/mypass/456.ts", "https://***/movie/***/456.ts"),
            ("https://provider.example/series/myuser/mypass/789.ts", "https://***/series/***/789.ts"),
        ];

        for (input, expected) in cases {
            assert_eq!(sanitize_sensitive_info(input).as_ref(), *expected, "input: {input}");
        }

        // Query-string credentials are masked by `re_credentials`, not by the
        // stream-URL rewriter — the host is still masked by `re_url`.
        assert_eq!(
            sanitize_sensitive_info("http://example/player_api.php?username=foo&password=bar"),
            "http://***/player_api.php?username=***&password=***"
        );

        set_sanitize_sensitive_info(previous);
    }

    #[test]
    fn sanitize_sensitive_info_keeps_file_urls_untouched() {
        let previous = super::is_sanitize_sensitive_info_enabled();
        set_sanitize_sensitive_info(true);

        let input = "file:///home/user/projects/m3u-test/server/m3u-simple/data/epg.xml.gz";
        assert_eq!(sanitize_sensitive_info(input), input);

        set_sanitize_sensitive_info(previous);
    }
}
