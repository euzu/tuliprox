use crate::{
    concat_string,
    error::TuliproxError,
    info_err, info_err_res,
    utils::{CONSTANTS, DASH_EXT, DASH_EXT_FRAGMENT, DASH_EXT_QUERY, HLS_EXT, HLS_EXT_FRAGMENT, HLS_EXT_QUERY},
};
use std::{borrow::Cow, sync::atomic::Ordering};
use url::Url;

pub const PROVIDER_SCHEME_PREFIX: &str = "provider://";

pub const CONTENT_TYPE_JSON: &str = "application/json";
pub const CONTENT_TYPE_CBOR: &str = "application/cbor";
pub const ACCEPT_PREFER_CBOR: &str = "application/cbor, application/json;q=0.9";
pub const HEADER_IF_MATCH: &str = "If-Match";
pub const HEADER_CONFIG_MAIN_REVISION: &str = "X-Tuliprox-Main-Revision";
pub const HEADER_CONFIG_SOURCES_REVISION: &str = "X-Tuliprox-Sources-Revision";
pub const HEADER_CONFIG_API_PROXY_REVISION: &str = "X-Tuliprox-ApiProxy-Revision";

pub fn set_sanitize_sensitive_info(value: bool) { CONSTANTS.sanitize.store(value, Ordering::Relaxed); }
pub fn sanitize_sensitive_info(query: &str) -> Cow<'_, str> {
    if !CONSTANTS.sanitize.load(Ordering::Relaxed) {
        return Cow::Borrowed(query);
    }

    let mut result = query.to_owned();

    for (re, replacement) in &[
        (&CONSTANTS.re_credentials, "$1***"),
        (&CONSTANTS.re_ipv4, "$1***"),
        (&CONSTANTS.re_ipv6, "$1***"),
        (&CONSTANTS.re_stream_url, "$1***/$2/***"),
        (&CONSTANTS.re_url, "$1***/$2"),
        (&CONSTANTS.re_password, "$1***"),
    ] {
        result = re.replace_all(&result, *replacement).into_owned();
    }
    Cow::Owned(result)
}

/// Extracts the file extension from a URL path (query/fragment stripped).
/// Returns the extension **prefixed with a dot** (e.g., ".m3u8").
pub fn extract_extension_from_url(input: &str) -> Option<String> {
    // 1. Strip query + fragment
    let input = input.split('?').next().unwrap_or(input).split('#').next().unwrap_or(input);

    // 2. Remove scheme (http://, file://, etc.)
    let path = input.split("://").last().unwrap_or(input);

    // 3. Take last path segment
    let filename = path.rsplit('/').next().filter(|s| !s.is_empty())?;

    // 4. Extract extension
    let ext = filename.rsplit('.').next().filter(|e| *e != filename)?; // ensures dot exists

    Some(concat_string!(".", ext))
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

/// Internal helper to parse the provider URL into (host, path_and_query)
pub fn parse_provider_scheme_url_parts(stream_url: &str) -> Result<(&str, &str), TuliproxError> {
    let rest = stream_url
        .strip_prefix(PROVIDER_SCHEME_PREFIX)
        .ok_or_else(|| info_err!("Not a provider URL: '{}'", sanitize_sensitive_info(stream_url)))?;

    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    if host.is_empty() {
        return info_err_res!("Provider host is empty in URL: '{}'", sanitize_sensitive_info(stream_url));
    }

    Ok((host, path))
}
