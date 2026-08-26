use crate::header_policy::{HeaderProtocol, HopByHopHeader};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use tuliprox_core::{model::ReverseProxyDisabledHeaderConfig, utils::content_coding::force_accept_encoding_identity};

/// Returns true when a header must never be forwarded by the live HLS cache proxy.
///
/// Thin wrapper around `HopByHopHeader::is_sensitive(HeaderProtocol::Hls, …)` so the
/// hard-coded hop-by-hop list and Tuliprox-internal prefix live in one place shared
/// with the MPEG-TS reverse-proxy path. Adding a new "always strip" header now
/// requires editing only `proxy/header_policy.rs`.
pub fn should_remove_hls_origin_header(
    header_name: &str,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
) -> bool {
    HopByHopHeader::is_sensitive(header_name, HeaderProtocol::Hls, disabled_headers)
}

/// Removes disabled and sensitive headers before an origin request leaves Tuliprox.
pub fn scrub_hls_origin_headers(headers: &mut HeaderMap, disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>) {
    let mut names = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    names.extend(
        headers.keys().filter(|name| should_remove_hls_origin_header(name.as_str(), disabled_headers)).cloned(),
    );
    for name in names {
        headers.remove(name);
    }
}

pub fn sanitized_hls_origin_headers(
    source_headers: &HeaderMap,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
) -> HeaderMap {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, disabled_headers);
    headers
}

/// Extracts trusted provider session cookies from origin response headers.
pub fn extract_hls_provider_session_header_map(headers: &HeaderMap) -> HeaderMap {
    let cookies = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next().map(str::trim))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    let mut session_headers = HeaderMap::new();
    if let Some(cookie_header) =
        (!cookies.is_empty()).then(|| cookies.join("; ")).and_then(|value| HeaderValue::from_str(&value).ok())
    {
        session_headers.insert(header::COOKIE, cookie_header);
    }
    session_headers
}

/// Extracts provider session cookies for the legacy `ActiveUserManager` session store.
pub fn extract_hls_provider_session_headers(headers: &HeaderMap) -> HashMap<String, String> {
    extract_hls_provider_session_header_map(headers)
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect()
}

/// Appends trusted provider session headers after client-header scrubbing.
pub fn append_hls_provider_session_headers(headers: &mut HeaderMap, provider_session_headers: &HeaderMap) {
    if let Some(cookie) = provider_session_headers.get(header::COOKIE).cloned() {
        headers.insert(header::COOKIE, cookie);
    }
}

pub fn hls_origin_headers_with_provider_session(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
) -> HeaderMap {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    append_hls_provider_session_headers(&mut headers, provider_session_headers);
    headers
}

pub fn force_identity_without_range(headers: &mut HeaderMap) {
    headers.remove(header::RANGE);
    force_accept_encoding_identity(headers);
}

#[cfg(test)]
mod tests {
    use super::{
        append_hls_provider_session_headers, extract_hls_provider_session_header_map,
        extract_hls_provider_session_headers, force_identity_without_range, scrub_hls_origin_headers,
        should_remove_hls_origin_header,
    };
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
    use tuliprox_core::model::ReverseProxyDisabledHeaderConfig;

    #[test]
    fn should_remove_sensitive_and_disabled_headers() {
        let disabled = ReverseProxyDisabledHeaderConfig {
            referer_header: true,
            x_header: true,
            cloudflare_header: true,
            custom_header: vec!["X-Origin-Secret".to_string()],
        };

        assert!(should_remove_hls_origin_header("Authorization", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Cookie", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Connection", Some(&disabled)));
        assert!(should_remove_hls_origin_header("TE", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Trailer", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Transfer-Encoding", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Upgrade", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Proxy-Authorization", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Host", Some(&disabled)));
        assert!(should_remove_hls_origin_header("X-Tuliprox-Main-Revision", Some(&disabled)));
        assert!(should_remove_hls_origin_header("Referer", Some(&disabled)));
        assert!(should_remove_hls_origin_header("X-Blocked", Some(&disabled)));
        assert!(should_remove_hls_origin_header("CF-Ray", Some(&disabled)));
        assert!(should_remove_hls_origin_header("x-origin-secret", Some(&disabled)));
        assert!(!should_remove_hls_origin_header("Accept-Language", Some(&disabled)));
    }

    #[test]
    fn scrub_removes_sensitive_and_disabled_headers_from_header_map() {
        let disabled = ReverseProxyDisabledHeaderConfig {
            referer_header: false,
            x_header: true,
            cloudflare_header: false,
            custom_header: vec!["X-Origin-Secret".to_string()],
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        headers.insert(header::HOST, HeaderValue::from_static("origin.example.com"));
        headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("blocked"));
        headers.insert(HeaderName::from_static("x-origin-secret"), HeaderValue::from_static("secret"));
        headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));

        scrub_hls_origin_headers(&mut headers, Some(&disabled));

        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key(header::HOST));
        assert!(!headers.contains_key("x-blocked"));
        assert!(!headers.contains_key("x-origin-secret"));
        assert_eq!(headers.get(header::ACCEPT_LANGUAGE).expect("language"), "de");
    }

    #[test]
    fn scrub_removes_headers_named_by_connection() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, x-origin-hop"));
        headers.insert(HeaderName::from_static("x-origin-hop"), HeaderValue::from_static("secret"));
        headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));

        scrub_hls_origin_headers(&mut headers, None);

        assert!(!headers.contains_key(header::CONNECTION));
        assert!(!headers.contains_key("x-origin-hop"));
        assert!(headers.contains_key(header::ACCEPT_LANGUAGE));
    }

    #[test]
    fn identity_helper_removes_existing_range() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));

        force_identity_without_range(&mut headers);

        assert!(!headers.contains_key(header::RANGE));
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
    }

    #[test]
    fn extract_provider_session_headers_converts_set_cookie_to_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append(header::SET_COOKIE, HeaderValue::from_static("sid=abc; Path=/; HttpOnly"));
        headers.append(header::SET_COOKIE, HeaderValue::from_static("pref=1; SameSite=Lax"));

        let header_map = extract_hls_provider_session_header_map(&headers);
        assert_eq!(header_map.get(header::COOKIE).expect("cookie"), "sid=abc; pref=1");

        let legacy_headers = extract_hls_provider_session_headers(&headers);
        assert_eq!(legacy_headers.get("cookie").map(String::as_str), Some("sid=abc; pref=1"));
    }

    #[test]
    fn append_provider_session_headers_restores_trusted_cookie_after_scrub() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("client=secret"));
        scrub_hls_origin_headers(&mut headers, None);

        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(header::COOKIE, HeaderValue::from_static("sid=abc"));
        append_hls_provider_session_headers(&mut headers, &provider_headers);

        assert_eq!(headers.get(header::COOKIE).expect("cookie"), "sid=abc");
    }
}
