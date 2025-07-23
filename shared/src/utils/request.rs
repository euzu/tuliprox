use std::borrow::Cow;
use std::sync::atomic::Ordering;
use url::Url;
use crate::utils::{CONSTANTS, DASH_EXT, DASH_EXT_FRAGMENT, DASH_EXT_QUERY, HLS_EXT, HLS_EXT_FRAGMENT, HLS_EXT_QUERY};


pub fn set_sanitize_sensitive_info(value: bool) {
    CONSTANTS.sanitize.store(value, Ordering::SeqCst);
}
pub fn sanitize_sensitive_info(query: &str) -> Cow<str> {
    if !CONSTANTS.sanitize.load(Ordering::SeqCst) {
        return Cow::Borrowed(query);
    }

    let mut result = query.to_owned();

    for (re, replacement) in &[
        (&CONSTANTS.re_credentials, "$1***"),
        (&CONSTANTS.re_ipv4, "$1***"),
        (&CONSTANTS.re_ipv6, "$1***"),
        (&CONSTANTS.re_stream_url, "$1***/$2/***"),
        (&CONSTANTS.re_url, "$1***/$2"),
    ] {
        result = re.replace_all(&result, *replacement).into_owned();
    }
    Cow::Owned(result)
}

#[inline]
fn ensure_extension(ext: &str) -> Option<&str> {
    if ext.len() > 4 {
        return None;
    }
    Some(ext)
}

pub fn extract_extension_from_url(url: &str) -> Option<&str> {
    if let Some(protocol_pos) = url.find("://") {
        if let Some(last_slash_pos) = url[protocol_pos + 3..].rfind('/') {
            let path = &url[protocol_pos + 3 + last_slash_pos + 1..];
            if let Some(last_dot_pos) = path.rfind('.') {
                return ensure_extension(&path[last_dot_pos..]);
            }
        }
    } else if let Some(last_dot_pos) = url.rfind('.') {
        if last_dot_pos > url.rfind('/').unwrap_or(0) {
            return ensure_extension(&url[last_dot_pos..]);
        }
    }
    None
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
        None => (url, ""), // No query or fragment
    };

    // Find the last '/' in the base URL, which marks the end of the domain and the beginning of the file path
    if let Some(last_slash_pos) = base_url.rfind('/') {
        if last_slash_pos < 9 { // protocol slash, return url as is
            return url.to_string();
        }
        let (path_part, file_name_with_extension) = base_url.split_at(last_slash_pos + 1);
        // Find the last dot in the file name to replace the extension
        if let Some(dot_pos) = file_name_with_extension.rfind('.') {
            return format!(
                "{}{}.{}{}",
                path_part,
                &file_name_with_extension[..dot_pos], // Keep the name part before the dot
                ext, // Add the new extension
                suffix // Add the query or fragment if any
            );
        }
    }

    // If no extension is found, add the new extension to the base URL
    format!("{}{}.{}{}", base_url, "", ext, suffix)
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
