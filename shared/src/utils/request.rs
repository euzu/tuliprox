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
pub const HEADER_PLAYLIST_SCOPE_REVISION: &str = "X-Tuliprox-Playlist-Scope-Revision";

pub fn set_sanitize_sensitive_info(value: bool) { CONSTANTS.sanitize.store(value, Ordering::Relaxed); }
pub fn is_sanitize_sensitive_info_enabled() -> bool { CONSTANTS.sanitize.load(Ordering::Relaxed) }
/// Masks log values centrally while preserving URL components, parameter names, and separators.
/// Disabled sanitization preserves the original input; standalone local file URLs are also preserved.
pub fn sanitize_sensitive_info(value: &str) -> Cow<'_, str> {
    if !is_sanitize_sensitive_info_enabled() {
        return Cow::Borrowed(value);
    }

    if let Some((scheme, _)) = split_log_url(value) {
        let file = scheme.eq_ignore_ascii_case("file");
        let single =
            !value.chars().any(char::is_whitespace) || (file && CONSTANTS.re_url.captures_iter(value).nth(1).is_none());
        if single {
            return if file { Cow::Borrowed(value) } else { Cow::Owned(mask_log_url(value)) };
        }
    }
    if value.starts_with(['/', '?']) && !value.chars().any(char::is_whitespace) {
        return Cow::Owned(mask_url_reference(value));
    }
    let bare_query = !value.contains("://")
        && value.contains('&')
        && value.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty() && !key.contains(['/', ':', '?', '#']) && !key.chars().any(char::is_whitespace)
        });
    if bare_query {
        return Cow::Owned(mask_query_values(value));
    }
    Cow::Owned(mask_log_message(value))
}

fn mask_log_message(value: &str) -> String {
    // A URL inside a password/token field is itself secret data, rather than a
    // diagnostic URL whose components should remain visible.
    let mut credentials = [&CONSTANTS.re_credentials, &CONSTANTS.re_password]
        .into_iter()
        .flat_map(|regex| {
            regex
                .captures_iter(value)
                .map(|field| (field.get(0).expect("credential field").start(), field.get(2).expect("credential value")))
        })
        .collect::<Vec<_>>();
    credentials.sort_unstable_by_key(|(start, _)| *start);
    let mut credential_index = 0;
    let mut result = String::with_capacity(value.len());
    let mut previous_end = 0;
    let mut spans = CONSTANTS.re_url.captures_iter(value).peekable();
    while let Some(captures) = spans.next() {
        let matched = captures.name("absolute").or_else(|| captures.name("relative")).expect("URI capture");
        if matched.start() < previous_end {
            continue;
        }
        let next_start = spans.peek().map_or(value.len(), |next| {
            next.name("absolute").or_else(|| next.name("relative")).expect("URI capture").start()
        });
        let url_end = log_url_end(value, matched, next_start);
        let mut credential = None;
        while let Some(&(start, raw)) = credentials.get(credential_index) {
            if start >= matched.start() {
                break;
            }
            credential_index += 1;
            if start >= previous_end && raw.end() > matched.start() {
                credential = Some(raw);
                break;
            }
        }
        if let Some(raw) = credential {
            // Extend the secret only when authority recovery continues the
            // credential's own value. Its explicit field boundary takes precedence.
            let end = if raw.end() == matched.end() { url_end } else { raw.end() };
            result.push_str(&mask_log_text(&value[previous_end..raw.start()]));
            result.push_str(&mask_log_field_value(&value[raw.start()..end]));
            previous_end = end;
            continue;
        }
        result.push_str(&mask_log_text(&value[previous_end..matched.start()]));
        let wrapper = value[..matched.start()].chars().next_back();
        let closing = match wrapper {
            Some('(') => ")",
            Some('[') => "]",
            Some('{') => "}",
            _ => "",
        };
        let span = &value[matched.start()..url_end];
        let (url, suffix) = if closing.is_empty() {
            (span, "")
        } else {
            span.strip_suffix(closing).map_or((span, ""), |url| (url, closing))
        };
        result.push_str(&mask_log_url(url));
        result.push_str(suffix);
        previous_end = url_end;
    }
    result.push_str(&mask_log_text(&value[previous_end..]));
    result
}

fn log_url_end(value: &str, matched: regex::Match<'_>, next_start: usize) -> usize {
    let end = matched.end();
    let Some((scheme, authority)) = split_log_url(matched.as_str()) else { return end };
    if scheme.eq_ignore_ascii_case("file") || authority.contains(['/', '?', '#']) {
        return end;
    }
    let suffix = &value[end..];
    let Some(boundary) = suffix.find(['/', '?', '#']).filter(|offset| end + offset <= next_start) else { return end };
    let tail = &suffix[..boundary];
    if !tail.starts_with(char::is_whitespace)
        || tail.ends_with(char::is_whitespace)
        || tail.contains(['<', '>', '"', '\'', '\r', '\n'])
    {
        return end;
    }
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let has_port = host_port
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()));
    let explicit_end = has_port || authority.ends_with([')', ']', '}']);
    if explicit_end && !tail.contains('@') {
        return end;
    }
    // The final token can continue the authority as userinfo@host, even with
    // '=' in the userinfo. Earlier fields and explicit URL endings retain precedence.
    let (preceding, last) = tail.rsplit_once(char::is_whitespace).unwrap_or(("", tail));
    let userinfo_continues =
        last.rsplit_once('@').is_some_and(|(userinfo, host)| !userinfo.is_empty() && !host.is_empty());
    let diagnostic_tail = if userinfo_continues && !explicit_end { preceding } else { tail };
    if diagnostic_tail.split_whitespace().any(|part| {
        part.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        })
    }) {
        return end;
    }
    // Reuse the reference scanner so quoted queries and embedded URLs stay
    // within the recovered span. A leading slash lets it scan a bare query or fragment.
    let reference = &suffix[boundary..];
    let (scan, prefix_len) = if reference.starts_with('/') {
        (Cow::Borrowed(reference), 0)
    } else {
        (Cow::Owned(format!("/{reference}")), 1)
    };
    CONSTANTS
        .re_url
        .captures(&scan)
        .and_then(|captures| captures.name("relative"))
        .map_or(end, |reference| end + boundary + reference.end() - prefix_len)
}

fn mask_log_text(value: &str) -> String {
    let mut result = value.to_string();
    for regex in [&CONSTANTS.re_query_values, &CONSTANTS.re_credentials, &CONSTANTS.re_password] {
        result = regex
            .replace_all(&result, |captures: &regex::Captures<'_>| {
                format!("{}{}", &captures[1], mask_log_field_value(&captures[2]))
            })
            .into_owned();
    }
    for regex in [&CONSTANTS.re_ipv4, &CONSTANTS.re_ipv6] {
        result = regex.replace_all(&result, "$1***").into_owned();
    }
    result
}

fn mask_log_field_value(raw: &str) -> String {
    let value = raw.trim_end_matches([')', ']', '}', ',', ';']);
    format!("{}{}", masked_log_value(value), &raw[value.len()..])
}

fn masked_nonempty(value: &str) -> &str {
    if value.is_empty() {
        ""
    } else {
        "***"
    }
}

fn masked_log_value(value: &str) -> String {
    if value.len() >= 2 {
        let quote = value.as_bytes()[0];
        if matches!(quote, b'\'' | b'"') && value.as_bytes().last() == Some(&quote) {
            let quote = char::from(quote);
            return format!("{quote}{}{quote}", masked_nonempty(&value[1..value.len() - 1]));
        }
    }
    masked_nonempty(value).to_string()
}

fn mask_query_values(query: &str) -> String {
    query
        .split('&')
        .map(|parameter| {
            parameter
                .split_once('=')
                .map_or_else(|| parameter.to_string(), |(name, value)| format!("{name}={}", masked_log_value(value)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn split_log_url(value: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = value.split_once("://")?;
    let mut bytes = scheme.bytes();
    (bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')))
    .then_some((scheme, rest))
}

fn mask_log_url(value: &str) -> String {
    let Some((scheme, rest)) = split_log_url(value) else { return mask_url_reference(value) };
    if scheme.eq_ignore_ascii_case("file") {
        return mask_log_text(value);
    }
    let boundary = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, reference) = rest.split_at(boundary);
    format!("{scheme}://{}{}", mask_url_authority(authority), mask_url_reference(reference))
}

fn mask_url_authority(authority: &str) -> String {
    let (userinfo, host_port) = authority.rsplit_once('@').map_or((None, authority), |(user, host)| (Some(user), host));
    let mut result = String::new();
    if let Some(userinfo) = userinfo {
        if let Some((user, password)) = userinfo.split_once(':') {
            result.push_str(masked_nonempty(user));
            result.push(':');
            result.push_str(masked_nonempty(password));
        } else {
            result.push_str(masked_nonempty(userinfo));
        }
        result.push('@');
    }
    if let Some(bracketed) = host_port.strip_prefix('[') {
        if let Some((host, suffix)) = bracketed.split_once(']') {
            result.push('[');
            result.push_str(masked_nonempty(host));
            result.push(']');
            if let Some(port) = suffix.strip_prefix(':') {
                result.push(':');
                result.push_str(mask_port(port));
            } else {
                result.push_str(masked_nonempty(suffix));
            }
            return result;
        }
    }
    if let Some((host, port)) = host_port.rsplit_once(':') {
        result.push_str(masked_nonempty(host));
        result.push(':');
        result.push_str(mask_port(port));
    } else {
        result.push_str(masked_nonempty(host_port));
    }
    result
}

fn mask_port(port: &str) -> &str {
    if port.bytes().all(|byte| byte.is_ascii_digit()) {
        port
    } else {
        masked_nonempty(port)
    }
}

fn mask_url_reference(reference: &str) -> String {
    let (before_fragment, fragment) =
        reference.split_once('#').map_or((reference, None), |(path, fragment)| (path, Some(fragment)));
    let (path, query) =
        before_fragment.split_once('?').map_or((before_fragment, None), |(path, query)| (path, Some(query)));
    // Preserve ordinary paths and the established Xtream credential masking.
    // Origin HLS segment paths carry their credential in the hex directory.
    let stream_path = CONSTANTS.re_stream_path.replace(path, "/${context}/***/");
    let mut result = CONSTANTS.re_hls_segment_path.replace(&stream_path, "$1***$2").into_owned();
    if let Some(query) = query {
        result.push('?');
        result.push_str(&mask_query_values(query));
    }
    if let Some(fragment) = fragment {
        result.push('#');
        result.push_str(&masked_log_value(fragment));
    }
    result
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
    use std::sync::Mutex;

    static SANITIZATION_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct SanitizationSettingGuard(bool);

    impl SanitizationSettingGuard {
        fn set(enabled: bool) -> Self {
            let previous = super::is_sanitize_sensitive_info_enabled();
            set_sanitize_sensitive_info(enabled);
            Self(previous)
        }
    }

    impl Drop for SanitizationSettingGuard {
        fn drop(&mut self) { set_sanitize_sensitive_info(self.0); }
    }

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
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        for scheme in ["http", "https", "provider", "batch"] {
            let input = format!("{scheme}://example/live/myuser/mypass/15373.ts");
            let expected = format!("{scheme}://***/live/***/15373.ts");
            assert_eq!(sanitize_sensitive_info(&input), expected);
        }
    }

    #[test]
    fn sanitize_sensitive_info_preserves_head_path_policy() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        let cases = [
            (
                "http://example/timeshift/myuser/mypass/3/2026-06-17:14-00/449.ts",
                "http://***/timeshift/***/3/2026-06-17:14-00/449.ts",
            ),
            ("http://example/streaming/myuser/mypass/123.ts", "http://***/streaming/***/123.ts"),
            ("http://example/xtream/myuser/mypass/123.ts", "http://***/xtream/***/123.ts"),
            ("http://example/timeshift.php/myuser/mypass/123.ts", "http://***/timeshift.php/***/123.ts"),
            ("https://provider.example/movie/myuser/mypass/456.ts", "https://***/movie/***/456.ts"),
            ("https://provider.example/series/myuser/mypass/789.ts", "https://***/series/***/789.ts"),
            (
                "http://example/player_api.php?username=foo&password=bar",
                "http://***/player_api.php?username=***&password=***",
            ),
            ("https://example/hls/index.m3u8", "https://***/hls/index.m3u8"),
            ("https://example/a//file.unknown/../archive.xml.gz", "https://***/a//file.unknown/../archive.xml.gz"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_sensitive_info(input), expected, "input: {input}");
        }
    }

    #[test]
    fn sanitize_sensitive_info_masks_only_hex_in_matching_hls_segment_paths() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        let cases = [
            ("/hls/ab12cd34/1025126_978.ts", "/hls/***/1025126_978.ts"),
            ("/hls/ABCDEF0123/42_0.TS?sig=secret&flag", "/hls/***/42_0.TS?sig=***&flag"),
            ("https://origin.example/hls/ab12cd34/1025126_978.ts", "https://***/hls/***/1025126_978.ts"),
            ("provider://profile/hls/ab12cd34/42_1.ts", "provider://***/hls/***/42_1.ts"),
            ("GET /hls/ab12cd34/42_1.ts?token=secret status=200", "GET /hls/***/42_1.ts?token=*** status=200"),
            ("/hls/not-hex/42_1.ts", "/hls/not-hex/42_1.ts"),
            ("/hls/ab12cd34/index.m3u8", "/hls/ab12cd34/index.m3u8"),
            ("/hls/ab12cd34/42.ts", "/hls/ab12cd34/42.ts"),
            ("/hls/ab12cd34/channel_1.ts", "/hls/ab12cd34/channel_1.ts"),
            ("/hls/ab12cd34/42_1.ts/extra", "/hls/ab12cd34/42_1.ts/extra"),
            ("/other/ab12cd34/42_1.ts", "/other/ab12cd34/42_1.ts"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_sensitive_info(input), expected, "input: {input}");
            assert_eq!(sanitize_sensitive_info(expected), expected, "masking must be idempotent");
        }
    }

    #[test]
    fn sanitize_sensitive_info_keeps_file_urls_untouched() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        let input = "file:///home/user/projects/m3u-test/server/m3u-simple/data/epg.xml.gz";
        assert_eq!(sanitize_sensitive_info(input), input);
        assert_eq!(sanitize_sensitive_info("FILE:///tmp/local.xml"), "FILE:///tmp/local.xml");
    }

    #[test]
    fn sanitize_sensitive_info_masks_url_values_without_removing_components() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        let cases = [
            (
                "https://user:password@origin.example:8443/auth-path/0.ts?sig=secret&opaque=unknown#fragment",
                "https://***:***@***:8443/auth-path/0.ts?sig=***&opaque=***#***",
            ),
            ("provider://profile/auth-path/index.m3u8?opaque=secret", "provider://***/auth-path/index.m3u8?opaque=***"),
            ("batch://profile/auth-path/catalog?opaque=secret", "batch://***/auth-path/catalog?opaque=***"),
            (
                "https://origin.example/auth-path/opaque-target?unknown=secret",
                "https://***/auth-path/opaque-target?unknown=***",
            ),
            (
                "https://origin.example?opaque=one&opaque=two&encoded=%26token%3Dsecret",
                "https://***?opaque=***&opaque=***&encoded=***",
            ),
            ("https://[2001:db8::1]:8443/auth-path?opaque=secret", "https://[***]:8443/auth-path?opaque=***"),
            ("https://bad host/auth-path/0.ts?opaque=secret", "https://***/auth-path/0.ts?opaque=***"),
            ("/auth-path/0.ts?opaque=secret", "/auth-path/0.ts?opaque=***"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_sensitive_info(input), expected, "input: {input}");
            assert_eq!(sanitize_sensitive_info(expected), expected, "masking must be idempotent");
        }
    }

    #[test]
    fn sanitize_sensitive_info_preserves_query_names_order_empty_values_and_flags() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        let cases = [
            (
                "https://u:p@host:8443/a//b/../0.ts?flag&empty=&q=one&q=two&&%73ig=a%26b#frag",
                "https://***:***@***:8443/a//b/../0.ts?flag&empty=&q=***&q=***&&%73ig=***#***",
            ),
            ("https://host/a?", "https://***/a?"),
            ("https://host/#", "https://***/#"),
            (r#"https://host/path?q=""&q='x y'&flag"#, r#"https://***/path?q=""&q='***'&flag"#),
            ("/a//index.m3u8?flag&empty=&name=x&", "/a//index.m3u8?flag&empty=&name=***&"),
            ("opaque=secret&safe=value&flag&empty=", "opaque=***&safe=***&flag&empty="),
            ("https://:pass@host:8443/0.ts", "https://:***@***:8443/0.ts"),
            (
                "https://host/a?192.0.2.1=value&encoded%5Bkey%5D=secret",
                "https://***/a?192.0.2.1=***&encoded%5Bkey%5D=***",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_sensitive_info(input), expected, "input: {input}");
            assert_eq!(input.matches('&').count(), expected.matches('&').count());
            assert_eq!(input.matches('?').count(), expected.matches('?').count());
            assert_eq!(input.matches('#').count(), expected.matches('#').count());
            assert_eq!(sanitize_sensitive_info(expected), expected, "masking must be idempotent");
        }
    }

    #[test]
    fn sanitize_sensitive_info_preserves_diagnostics_around_multiple_urls() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(true);
        let cases = [
            ("error sending request for url (https://origin.example/auth-path/opaque-target?unknown=secret)",
             "error sending request for url (https://***/auth-path/opaque-target?unknown=***)"),
            ("request_url=provider://profile/auth/index.m3u8?token=first&opaque=secret final_url=https://cdn.example/opaque?sig=secret status=407",
             "request_url=provider://***/auth/index.m3u8?token=***&opaque=*** final_url=https://***/opaque?sig=*** status=407"),
            ("URL 'https://cdn.example/auth?opaque=secret' failed", "URL 'https://***/auth?opaque=***' failed"),
            ("url=<https://host/auth?opaque=secret>", "url=<https://***/auth?opaque=***>"),
            ("épisode GET /épisode?token=sëcret&title=café après", "épisode GET /épisode?token=***&title=*** après"),
            ("url=https://host/a?192.0.2.1=value status=407", "url=https://***/a?192.0.2.1=*** status=407"),
            ("username=user&password=secret&token=secret&api_key=secret", "username=***&password=***&token=***&api_key=***"),
            (r#"token="së\"cret" after"#, r#"token="***" after"#),
            ("password: 'two words' remaining", "password: '***' remaining"),
            ("password=secret://host/path#fragment", "password=***"),
            ("password: https://host/path?token=x&other=y#fragment remaining", "password: *** remaining"),
            ("password: 'https://host/path?token=x#fragment' remaining", "password: '***' remaining"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_sensitive_info(input), expected, "input: {input}");
            assert_eq!(sanitize_sensitive_info(expected), expected, "masking must be idempotent");
        }
    }

    #[test]
    fn sanitize_sensitive_info_masks_each_url_with_a_port_in_diagnostics() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let input = "http://a:8080 to http://b:8080";
        let expected = "http://***:8080 to http://***:8080";
        {
            let _guard = SanitizationSettingGuard::set(true);
            assert_eq!(sanitize_sensitive_info(input), expected);
            assert_eq!(sanitize_sensitive_info(expected), expected);
        }
        let _guard = SanitizationSettingGuard::set(false);
        assert_eq!(sanitize_sensitive_info(input), input);
    }

    #[test]
    fn sanitize_sensitive_info_masks_malformed_authorities_in_diagnostics() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cases = [
            (
                "http://bad user:example@internal.test/path to http://b:8080",
                "http://***:***@***/path to http://***:8080",
            ),
            (
                "request (http://bad host/path?sig=example) to http://b:8080/end",
                "request (http://***/path?sig=***) to http://***:8080/end",
            ),
            (
                "http://a:8080 to http://bad host/path?u=http://inner.test/a status=200",
                "http://***:8080 to http://***/path?u=*** status=200",
            ),
        ];
        {
            let _guard = SanitizationSettingGuard::set(true);
            for (input, expected) in cases {
                assert_eq!(sanitize_sensitive_info(input), expected);
                assert_eq!(sanitize_sensitive_info(expected), expected);
            }
        }
        let _guard = SanitizationSettingGuard::set(false);
        for (input, _) in cases {
            assert_eq!(sanitize_sensitive_info(input), input);
        }
    }

    #[test]
    fn sanitize_sensitive_info_masks_equals_in_malformed_url_userinfo() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cases = [
            (
                "http://bad user=demo:sample@internal.test/path to http://b:8080",
                "http://***:***@***/path to http://***:8080",
            ),
            ("http://bad user=demo:sample@internal.test/path", "http://***:***@***/path"),
            (
                "request (http://bad user=demo:sample@internal.test/path) to http://b:8080",
                "request (http://***:***@***/path) to http://***:8080",
            ),
            (
                "password: http://bad user=demo:sample@internal.test/path status=502 to http://b:8080",
                "password: *** status=502 to http://***:8080",
            ),
            ("http://bad user=demo@internal.test/path", "http://***@***/path"),
            (
                "http://a:8080 reason=cdn:down@edge/connect to http://b:8080",
                "http://***:8080 reason=cdn:down@edge/connect to http://***:8080",
            ),
            (
                "http://a status=502 reason=cdn:down@edge/connect to http://b:8080",
                "http://*** status=502 reason=cdn:down@edge/connect to http://***:8080",
            ),
        ];
        {
            let _guard = SanitizationSettingGuard::set(true);
            let actual = cases.map(|(input, _)| sanitize_sensitive_info(input).into_owned());
            assert_eq!(actual, cases.map(|(_, expected)| expected.to_owned()));
            for (_, expected) in cases {
                assert_eq!(sanitize_sensitive_info(expected), expected);
            }
        }
        let _guard = SanitizationSettingGuard::set(false);
        for (input, _) in cases {
            assert_eq!(sanitize_sensitive_info(input), input);
        }
    }

    #[test]
    fn sanitize_sensitive_info_preserves_fields_after_a_url_authority() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cases = [
            (
                "http://a:8080 status=502 reason=cdn/connect to http://b:8080",
                "http://***:8080 status=502 reason=cdn/connect to http://***:8080",
            ),
            ("http://a reason=cdn/connect", "http://*** reason=cdn/connect"),
            ("http://a:8080 cdn/connect", "http://***:8080 cdn/connect"),
        ];
        {
            let _guard = SanitizationSettingGuard::set(true);
            for (input, expected) in cases {
                assert_eq!(sanitize_sensitive_info(input), expected);
                assert_eq!(sanitize_sensitive_info(expected), expected);
            }
        }
        let _guard = SanitizationSettingGuard::set(false);
        for (input, _) in cases {
            assert_eq!(sanitize_sensitive_info(input), input);
        }
    }

    #[test]
    fn sanitize_sensitive_info_preserves_fields_after_a_credential_url() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cases = [
            (
                "password: http://a:8080 status=502 reason=cdn/connect to http://b:8080",
                "password: *** status=502 reason=cdn/connect to http://***:8080",
            ),
            (
                "password: http://bad user:ex=ample@internal.test/path status=502 to http://b:8080",
                "password: *** status=502 to http://***:8080",
            ),
            (
                "password: http://bad user:example@internal.test/path, status=502 to http://b:8080",
                "password: ***, status=502 to http://***:8080",
            ),
        ];
        {
            let _guard = SanitizationSettingGuard::set(true);
            for (input, expected) in cases {
                assert_eq!(sanitize_sensitive_info(input), expected);
                assert_eq!(sanitize_sensitive_info(expected), expected);
            }
        }
        let _guard = SanitizationSettingGuard::set(false);
        for (input, _) in cases {
            assert_eq!(sanitize_sensitive_info(input), input);
        }
    }

    #[test]
    fn sanitize_sensitive_info_masks_embedded_absolute_url_as_a_relative_query_value() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let input = "GET /get?sig=example&u=http://h/x status=200";
        let expected = "GET /get?sig=***&u=*** status=200";
        {
            let _guard = SanitizationSettingGuard::set(true);
            assert_eq!(sanitize_sensitive_info(input), expected);
            assert_eq!(sanitize_sensitive_info(expected), expected);
        }
        let _guard = SanitizationSettingGuard::set(false);
        assert_eq!(sanitize_sensitive_info(input), input);
    }

    #[test]
    fn sanitize_sensitive_info_disabled_preserves_complete_original_values() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = SanitizationSettingGuard::set(false);
        for input in [
            "https://user:password@origin.example:8443/auth-path/0.ts?sig=secret&opaque=unknown#fragment",
            "provider://profile/auth-path/index.m3u8?opaque=secret",
            "https://bad host/auth-path/0.ts?opaque=secret",
            "/auth-path/0.ts?opaque=secret",
            "/hls/ab12cd34/1025126_978.ts?token=secret&opaque=value",
            "/hls/17/session/index.m3u8?token=secret",
            "error sending request for url (https://origin.example/auth-path/opaque-target?unknown=secret)",
            "username=user&password=secret&token=secret&api_key=secret",
        ] {
            assert_eq!(sanitize_sensitive_info(input), input);
        }
    }
}
