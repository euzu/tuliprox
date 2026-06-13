use crate::model::ReverseProxyDisabledHeaderConfig;
use axum::http::{header, HeaderMap, HeaderValue};

/// Returns true when a header must never be forwarded by the live HLS cache proxy.
pub fn should_remove_hls_origin_header(
    header_name: &str,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
) -> bool {
    let header_lc = header_name.trim().to_ascii_lowercase();
    matches!(header_lc.as_str(), "authorization" | "cookie" | "cookie2" | "host" | "proxy-authorization" | "set-cookie")
        || header_lc.starts_with("x-tuliprox-")
        || disabled_headers.is_some_and(|disabled| disabled.should_remove(header_lc.as_str()))
}

/// Removes disabled and sensitive headers before an origin request leaves Tuliprox.
pub fn scrub_hls_origin_headers(headers: &mut HeaderMap, disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>) {
    let names = headers
        .keys()
        .filter(|name| should_remove_hls_origin_header(name.as_str(), disabled_headers))
        .cloned()
        .collect::<Vec<_>>();
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

pub fn force_identity_without_range(headers: &mut HeaderMap) {
    headers.remove(header::RANGE);
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
}

#[cfg(test)]
mod tests {
    use super::{force_identity_without_range, scrub_hls_origin_headers, should_remove_hls_origin_header};
    use crate::model::ReverseProxyDisabledHeaderConfig;
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

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
    fn identity_helper_removes_existing_range() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));

        force_identity_without_range(&mut headers);

        assert!(!headers.contains_key(header::RANGE));
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
    }
}
