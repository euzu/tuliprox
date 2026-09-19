//! The rejection returned by every authentication extractor.
//!
//! `AuthBasic`, `AuthBearer` and [`Fingerprint`](super::Fingerprint) each used
//! to declare `type Rejection = (StatusCode, &'static str)`, defined twice -
//! once in `tuliprox-auth`, once beside `Fingerprint`. That tuple carried no
//! structure, so every arm picked its own status by hand and they all picked
//! wrong: a *missing* `Authorization` header answered `403 Forbidden`, which
//! says "you are authenticated and still may not do this" when the truth is
//! "you did not authenticate". A client cannot tell those apart, and the
//! response carried no `WWW-Authenticate` challenge either.
//!
//! One enum, one `IntoResponse`, correct statuses, and the challenge header
//! comes along for free.

use axum::{
    http::{header::WWW_AUTHENTICATE, StatusCode},
    response::{IntoResponse, Response},
};

/// The HTTP authentication scheme an extractor was looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    Basic,
    Bearer,
}

impl AuthScheme {
    /// The `WWW-Authenticate` challenge for this scheme.
    #[inline]
    pub const fn challenge(self) -> &'static str {
        match self {
            Self::Basic => "Basic",
            Self::Bearer => "Bearer",
        }
    }
}

/// Why an authentication extractor refused the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRejection {
    /// No `Authorization` header at all.
    MissingHeader(AuthScheme),
    /// The header is present but not valid single-byte text, or carries no
    /// scheme/credentials split.
    MalformedHeader(AuthScheme),
    /// The header uses a different scheme than the extractor requires.
    WrongScheme(AuthScheme),
    /// Basic credentials decoded but carried no `:` separator.
    MissingBasicPassword,
    /// The peer address is unavailable, so the request cannot be attributed to
    /// a client. Not an authentication failure - a malformed connection.
    MissingPeerAddress,
}

impl AuthRejection {
    /// The scheme to advertise in the `WWW-Authenticate` challenge, if this
    /// rejection warrants one.
    #[inline]
    pub const fn scheme(self) -> Option<AuthScheme> {
        match self {
            Self::MissingHeader(scheme) | Self::MalformedHeader(scheme) | Self::WrongScheme(scheme) => Some(scheme),
            Self::MissingBasicPassword => Some(AuthScheme::Basic),
            Self::MissingPeerAddress => None,
        }
    }

    #[inline]
    pub const fn status(self) -> StatusCode {
        match self {
            // Every one of these means "you have not authenticated", which is
            // 401 - not the 403 these used to return.
            Self::MissingHeader(_) | Self::MalformedHeader(_) | Self::WrongScheme(_) | Self::MissingBasicPassword => {
                StatusCode::UNAUTHORIZED
            }
            Self::MissingPeerAddress => StatusCode::BAD_REQUEST,
        }
    }

    #[inline]
    pub const fn message(self) -> &'static str {
        match self {
            Self::MissingHeader(_) => "Authorization header is missing",
            Self::MalformedHeader(_) => "Authorization header contains invalid characters",
            Self::WrongScheme(AuthScheme::Basic) => "`Authorization` header must be a basic auth",
            Self::WrongScheme(AuthScheme::Bearer) => "`Authorization` header must be a bearer token",
            Self::MissingBasicPassword => "Authorization header contains no password",
            Self::MissingPeerAddress => "IP-Addr is missing",
        }
    }
}

impl std::fmt::Display for AuthRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.message()) }
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let mut response = (self.status(), self.message()).into_response();
        if let Some(scheme) = self.scheme() {
            if let Ok(value) = axum::http::HeaderValue::from_str(scheme.challenge()) {
                response.headers_mut().insert(WWW_AUTHENTICATE, value);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_header_is_unauthorized_not_forbidden() {
        // The whole point of the type: a missing header is 401, and it used to
        // be 403.
        let rejection = AuthRejection::MissingHeader(AuthScheme::Bearer);
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
        let response = rejection.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers().get(WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()), Some("Bearer"));
    }

    #[test]
    fn basic_rejections_challenge_with_basic() {
        for rejection in [
            AuthRejection::MissingHeader(AuthScheme::Basic),
            AuthRejection::MalformedHeader(AuthScheme::Basic),
            AuthRejection::WrongScheme(AuthScheme::Basic),
            AuthRejection::MissingBasicPassword,
        ] {
            assert_eq!(rejection.scheme(), Some(AuthScheme::Basic), "{rejection:?}");
            assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED, "{rejection:?}");
        }
    }

    #[test]
    fn missing_peer_address_is_a_bad_request_without_a_challenge() {
        let rejection = AuthRejection::MissingPeerAddress;
        assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
        assert_eq!(rejection.scheme(), None);
        let response = rejection.into_response();
        assert!(response.headers().get(WWW_AUTHENTICATE).is_none());
    }
}
