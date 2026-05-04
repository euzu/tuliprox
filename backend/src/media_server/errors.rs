use crate::media_server::redaction::redact_media_server_text;
use reqwest::StatusCode;
use std::{error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaServerErrorKind {
    MediaServerAuthDenied,
    MediaServerUnavailable,
    MediaServerLibraryUnavailable,
    MediaServerLibraryTypeUnsupported,
    MediaServerCatalogDecodeFailed,
    MediaServerCatalogPageStalled,
    MediaServerCatalogIncomplete,
    MediaServerItemNotFound,
    NoDirectPlayableMediaServerSource,
    MediaServerStreamOpenFailed,
    MediaServerRateLimited,
    MediaServerDiscoveryFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerError {
    pub kind: MediaServerErrorKind,
    pub provider: Option<&'static str>,
    pub status: Option<StatusCode>,
    detail: Option<String>,
}

impl MediaServerError {
    pub fn new(kind: MediaServerErrorKind) -> Self {
        Self {
            kind,
            provider: None,
            status: None,
            detail: None,
        }
    }

    pub fn provider(mut self, provider: &'static str) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn status(mut self, status: StatusCode) -> Self {
        self.status = Some(status);
        self
    }

    pub fn detail(mut self, detail: impl AsRef<str>) -> Self {
        let redacted = redact_media_server_text(detail.as_ref());
        if !redacted.trim().is_empty() {
            self.detail = Some(redacted);
        }
        self
    }

    pub fn detail_text(&self) -> Option<&str> { self.detail.as_deref() }

    pub fn from_http_status(status: StatusCode) -> Self {
        Self::from_http_status_with_fallback(
            status,
            MediaServerErrorKind::MediaServerItemNotFound,
            MediaServerErrorKind::MediaServerStreamOpenFailed,
        )
    }

    pub fn from_http_status_with_fallback(
        status: StatusCode,
        not_found_kind: MediaServerErrorKind,
        fallback_kind: MediaServerErrorKind,
    ) -> Self {
        let kind = if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            MediaServerErrorKind::MediaServerAuthDenied
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            MediaServerErrorKind::MediaServerRateLimited
        } else if status == StatusCode::NOT_FOUND {
            not_found_kind
        } else {
            fallback_kind
        };
        Self::new(kind).status(status)
    }

    pub fn from_reqwest_error(err: &reqwest::Error) -> Self {
        Self::from_reqwest_error_with_fallback(
            err,
            MediaServerErrorKind::MediaServerItemNotFound,
            MediaServerErrorKind::MediaServerStreamOpenFailed,
        )
    }

    pub fn from_reqwest_error_with_fallback(
        err: &reqwest::Error,
        not_found_kind: MediaServerErrorKind,
        fallback_kind: MediaServerErrorKind,
    ) -> Self {
        let kind = if err.is_timeout() || err.is_connect() {
            MediaServerErrorKind::MediaServerUnavailable
        } else if err.is_decode() {
            fallback_kind
        } else if let Some(status) = err.status() {
            return Self::from_http_status_with_fallback(status, not_found_kind, fallback_kind).detail(err.to_string());
        } else {
            fallback_kind
        };
        Self::new(kind).detail(err.to_string())
    }
}

impl fmt::Display for MediaServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(provider) = self.provider {
            write!(f, " provider={provider}")?;
        }
        if let Some(status) = self.status {
            write!(f, " status={status}")?;
        }
        if let Some(detail) = self.detail.as_deref() {
            write!(f, " detail={detail}")?;
        }
        Ok(())
    }
}

impl Error for MediaServerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_maps_to_stable_failure_kinds() {
        assert_eq!(
            MediaServerError::from_http_status(StatusCode::UNAUTHORIZED).kind,
            MediaServerErrorKind::MediaServerAuthDenied
        );
        assert_eq!(
            MediaServerError::from_http_status(StatusCode::TOO_MANY_REQUESTS).kind,
            MediaServerErrorKind::MediaServerRateLimited
        );
        assert_eq!(
            MediaServerError::from_http_status(StatusCode::NOT_FOUND).kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
    }

    #[test]
    fn http_status_mapping_accepts_operation_specific_fallbacks() {
        assert_eq!(
            MediaServerError::from_http_status_with_fallback(
                StatusCode::NOT_FOUND,
                MediaServerErrorKind::MediaServerLibraryUnavailable,
                MediaServerErrorKind::MediaServerCatalogDecodeFailed,
            )
            .kind,
            MediaServerErrorKind::MediaServerLibraryUnavailable
        );
        assert_eq!(
            MediaServerError::from_http_status_with_fallback(
                StatusCode::INTERNAL_SERVER_ERROR,
                MediaServerErrorKind::MediaServerLibraryUnavailable,
                MediaServerErrorKind::MediaServerCatalogDecodeFailed,
            )
            .kind,
            MediaServerErrorKind::MediaServerCatalogDecodeFailed
        );
        assert_eq!(
            MediaServerError::from_http_status_with_fallback(
                StatusCode::FORBIDDEN,
                MediaServerErrorKind::MediaServerLibraryUnavailable,
                MediaServerErrorKind::MediaServerCatalogDecodeFailed,
            )
            .kind,
            MediaServerErrorKind::MediaServerAuthDenied
        );
    }

    #[test]
    fn detail_is_redacted_before_display() {
        let err = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .provider("plex")
            .detail("https://media.example.invalid/stream?X-Plex-Token=secret-token&api_key=secret-key");
        let rendered = err.to_string();

        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("secret-key"));
        assert!(rendered.contains("<redacted>"));
    }
}
