use crate::{MediaServerError, MediaServerErrorKind};
use reqwest::StatusCode;

/// The phase of a media-server interaction a request belongs to. It decides
/// which `MediaServerErrorKind`s a failed request surfaces.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MediaServerOperation {
    Discovery,
    Catalog,
    Playback,
}

impl MediaServerOperation {
    pub(crate) const fn not_found_kind(self) -> MediaServerErrorKind {
        match self {
            Self::Discovery => MediaServerErrorKind::MediaServerUnavailable,
            Self::Catalog => MediaServerErrorKind::MediaServerLibraryUnavailable,
            Self::Playback => MediaServerErrorKind::MediaServerItemNotFound,
        }
    }

    pub(crate) const fn fallback_kind(self) -> MediaServerErrorKind {
        match self {
            Self::Discovery => MediaServerErrorKind::MediaServerDiscoveryFailed,
            Self::Catalog => MediaServerErrorKind::MediaServerCatalogDecodeFailed,
            Self::Playback => MediaServerErrorKind::MediaServerStreamOpenFailed,
        }
    }

    pub(crate) fn status_error(self, status: StatusCode, provider: &'static str) -> MediaServerError {
        MediaServerError::from_http_status_with_fallback(status, self.not_found_kind(), self.fallback_kind())
            .provider(provider)
    }

    pub(crate) fn reqwest_error(self, err: &reqwest::Error, provider: &'static str) -> MediaServerError {
        MediaServerError::from_reqwest_error_with_fallback(err, self.not_found_kind(), self.fallback_kind())
            .provider(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_map_to_stable_error_kinds() {
        assert_eq!(
            MediaServerOperation::Discovery.status_error(StatusCode::NOT_FOUND, "test").kind,
            MediaServerErrorKind::MediaServerUnavailable
        );
        assert_eq!(
            MediaServerOperation::Discovery.status_error(StatusCode::BAD_GATEWAY, "test").kind,
            MediaServerErrorKind::MediaServerDiscoveryFailed
        );
        assert_eq!(
            MediaServerOperation::Catalog.status_error(StatusCode::NOT_FOUND, "test").kind,
            MediaServerErrorKind::MediaServerLibraryUnavailable
        );
        assert_eq!(
            MediaServerOperation::Catalog.status_error(StatusCode::BAD_GATEWAY, "test").kind,
            MediaServerErrorKind::MediaServerCatalogDecodeFailed
        );
        assert_eq!(
            MediaServerOperation::Playback.status_error(StatusCode::NOT_FOUND, "test").kind,
            MediaServerErrorKind::MediaServerItemNotFound
        );
        assert_eq!(
            MediaServerOperation::Playback.status_error(StatusCode::BAD_GATEWAY, "test").kind,
            MediaServerErrorKind::MediaServerStreamOpenFailed
        );
    }

    #[test]
    fn status_error_carries_provider_status_and_auth_kinds() {
        let error = MediaServerOperation::Catalog.status_error(StatusCode::UNAUTHORIZED, "emby");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerAuthDenied);
        assert_eq!(error.provider, Some("emby"));
        assert_eq!(error.status, Some(StatusCode::UNAUTHORIZED));
        assert_eq!(
            MediaServerOperation::Playback.status_error(StatusCode::TOO_MANY_REQUESTS, "emby").kind,
            MediaServerErrorKind::MediaServerRateLimited
        );
    }
}
