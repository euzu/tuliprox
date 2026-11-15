//! `host::cache-image`: fetch an image once into the host's image cache.

use crate::sys;

/// Fetches `url` through the host (<= 2 MiB, SVG refused, quota-bound) and
/// returns the locally served URL to put in `logo` / `icon` fields.
pub fn cache(url: &str) -> Result<String, String> { sys::cache_image(url) }

/// Fetches `url` with extra request headers through the host (<= 2 MiB, SVG refused, quota-bound) and
/// returns the locally served URL to put in `logo` / `icon` fields.
pub fn cache_with_headers(url: &str, headers: &[(String, String)]) -> Result<String, String> {
    sys::cache_image_with_headers(url, headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    #[test]
    fn cache_and_cache_with_headers_work_with_mock() {
        mock::reset();
        let res = cache("http://example.com/logo.png").unwrap();
        assert_eq!(res, "/mock-cache/http://example.com/logo.png");
        assert_eq!(mock::cached_images(), vec!["http://example.com/logo.png"]);

        let headers = vec![("Authorization".to_string(), "Bearer secret".to_string())];
        let res_headers = cache_with_headers("http://example.com/icon.png", &headers).unwrap();
        assert_eq!(res_headers, "/mock-cache/http://example.com/icon.png");
        assert_eq!(mock::cached_images_with_headers(), vec![("http://example.com/icon.png".to_string(), headers)]);
    }
}
