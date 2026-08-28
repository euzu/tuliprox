use std::fmt;
use url::Url;

/// Immutable concrete request entry used by one manifest recovery burst.
#[derive(Clone, Eq, PartialEq)]
pub struct HlsManifestOriginBinding {
    request_url: Url,
    provider_url_index: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HlsManifestOriginBindingError {
    #[error("manifest origin binding requires an HTTP(S) request URL")]
    UnsupportedScheme,
    #[error("manifest origin binding requires a request host")]
    MissingHost,
}

impl HlsManifestOriginBinding {
    pub fn new(request_url: Url, provider_url_index: Option<usize>) -> Result<Self, HlsManifestOriginBindingError> {
        if !matches!(request_url.scheme(), "http" | "https") {
            return Err(HlsManifestOriginBindingError::UnsupportedScheme);
        }
        if !request_url.has_host() {
            return Err(HlsManifestOriginBindingError::MissingHost);
        }
        Ok(Self { request_url, provider_url_index })
    }

    pub fn request_url(&self) -> &Url { &self.request_url }

    pub const fn provider_url_index(&self) -> Option<usize> { self.provider_url_index }
}

impl fmt::Debug for HlsManifestOriginBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HlsManifestOriginBinding")
            .field("scheme", &self.request_url.scheme())
            .field("has_host", &self.request_url.has_host())
            .field("provider_url_index", &self.provider_url_index)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{HlsManifestOriginBinding, HlsManifestOriginBindingError};
    use url::Url;

    #[test]
    fn concrete_binding_preserves_http_request_and_provider_index() {
        let request_url = Url::parse("https://user:secret@origin.example:8443/live/index.m3u8?token=abc&quality=hd")
            .expect("HTTP request URL");

        let binding = HlsManifestOriginBinding::new(request_url.clone(), Some(3)).expect("binding accepted");

        assert_eq!(binding.request_url(), &request_url);
        assert_eq!(binding.request_url().path(), "/live/index.m3u8");
        assert_eq!(binding.request_url().query(), Some("token=abc&quality=hd"));
        assert_eq!(binding.provider_url_index(), Some(3));
        let debug = format!("{binding:?}");
        assert!(!debug.contains("user"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("token"));
    }

    #[test]
    fn concrete_binding_accepts_http_and_rejects_non_http_schemes() {
        let http = Url::parse("http://127.0.0.1/live/index.m3u8?x=1").expect("HTTP URL");
        assert!(HlsManifestOriginBinding::new(http, None).is_ok());

        for value in ["provider://demo/live/index.m3u8", "ftp://origin.example/live/index.m3u8"] {
            let url = Url::parse(value).expect("test URL");
            assert_eq!(HlsManifestOriginBinding::new(url, None), Err(HlsManifestOriginBindingError::UnsupportedScheme));
        }
    }
}
