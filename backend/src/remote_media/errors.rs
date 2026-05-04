use crate::remote_media::redaction::redact_remote_text;
use reqwest::StatusCode;
use std::{error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteMediaErrorKind {
    RemoteAuthDenied,
    RemoteServerUnavailable,
    RemoteLibraryUnavailable,
    RemoteLibraryTypeUnsupported,
    RemoteCatalogDecodeFailed,
    RemoteCatalogPageStalled,
    RemoteCatalogIncomplete,
    RemoteItemNotFound,
    NoDirectPlayableRemoteSource,
    RemoteStreamOpenFailed,
    RemoteRateLimited,
    ResourceDiscoveryFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMediaError {
    pub kind: RemoteMediaErrorKind,
    pub provider: Option<&'static str>,
    pub status: Option<StatusCode>,
    detail: Option<String>,
}

impl RemoteMediaError {
    pub fn new(kind: RemoteMediaErrorKind) -> Self {
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
        let redacted = redact_remote_text(detail.as_ref());
        if !redacted.trim().is_empty() {
            self.detail = Some(redacted);
        }
        self
    }

    pub fn detail_text(&self) -> Option<&str> { self.detail.as_deref() }

    pub fn from_http_status(status: StatusCode) -> Self {
        let kind = if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            RemoteMediaErrorKind::RemoteAuthDenied
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            RemoteMediaErrorKind::RemoteRateLimited
        } else if status == StatusCode::NOT_FOUND {
            RemoteMediaErrorKind::RemoteItemNotFound
        } else {
            RemoteMediaErrorKind::RemoteStreamOpenFailed
        };
        Self::new(kind).status(status)
    }

    pub fn from_reqwest_error(err: &reqwest::Error) -> Self {
        let kind = if err.is_timeout() || err.is_connect() {
            RemoteMediaErrorKind::RemoteServerUnavailable
        } else if err.is_decode() {
            RemoteMediaErrorKind::RemoteCatalogDecodeFailed
        } else if let Some(status) = err.status() {
            return Self::from_http_status(status).detail(err.to_string());
        } else {
            RemoteMediaErrorKind::RemoteStreamOpenFailed
        };
        Self::new(kind).detail(err.to_string())
    }
}

impl fmt::Display for RemoteMediaError {
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

impl Error for RemoteMediaError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_maps_to_stable_failure_kinds() {
        assert_eq!(
            RemoteMediaError::from_http_status(StatusCode::UNAUTHORIZED).kind,
            RemoteMediaErrorKind::RemoteAuthDenied
        );
        assert_eq!(
            RemoteMediaError::from_http_status(StatusCode::TOO_MANY_REQUESTS).kind,
            RemoteMediaErrorKind::RemoteRateLimited
        );
        assert_eq!(
            RemoteMediaError::from_http_status(StatusCode::NOT_FOUND).kind,
            RemoteMediaErrorKind::RemoteItemNotFound
        );
    }

    #[test]
    fn detail_is_redacted_before_display() {
        let err = RemoteMediaError::new(RemoteMediaErrorKind::RemoteStreamOpenFailed)
            .provider("plex")
            .detail("https://media.example.invalid/stream?X-Plex-Token=secret-token&api_key=secret-key");
        let rendered = err.to_string();

        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("secret-key"));
        assert!(rendered.contains("<redacted>"));
    }
}
