use crate::redaction;
use std::io;
use thiserror::Error;

/// Render a portal URL safely. Kept as this module's name for its existing callers; the
/// rule itself lives in [`crate::redaction`] alongside the rest.
pub fn safe_stalker_url(value: &str) -> String {
    redaction::safe_url(value)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StalkerErrorUrl(String);

impl From<&str> for StalkerErrorUrl {
    fn from(value: &str) -> Self {
        Self(safe_stalker_url(value))
    }
}

impl From<String> for StalkerErrorUrl {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

/// Failure modes surfaced by the Stalker portal client. The variants cover both transport
/// errors (network, body caps, JSONP decoding) and protocol-level errors (token rejection,
/// portal refusal, missing playable URL). `Status`-flavored variants carry the upstream
/// status code so reverse-proxy code can decide whether to trigger a `create_link` retry.
#[derive(Debug, Error)]
pub enum StalkerError {
    #[error("stalker portal handshake failed: {message}")]
    HandshakeFailed { message: String, url: Option<StalkerErrorUrl> },

    #[error("stalker portal rejected the token (status {status})")]
    TokenRejected { status: u16, url: Option<StalkerErrorUrl> },

    #[error("stalker portal body reported code {code} for {action}: {body_snippet}")]
    PortalBodyError { code: u16, action: String, body_snippet: String },

    #[error("stalker portal response exceeded {action} body cap of {cap_bytes} bytes")]
    ResponseTooLarge { action: String, cap_bytes: u64 },

    #[error("stalker portal refused the cmd: {reason}")]
    PortalRefusedCmd { reason: String },

    #[error("stalker portal returned an unsupported url scheme: {scheme}")]
    UnsupportedScheme { scheme: String },

    #[error("stalker portal body could not be decoded: {message}")]
    BodyDecode { message: String },

    #[error("stalker portal returned HTML body where JSON was expected: {snippet}")]
    HtmlResponse { snippet: String },

    #[error("stalker portal returned an empty body for {action}")]
    EmptyBody { action: String },

    #[error("stalker {portal_type} catalog is incomplete: {reason}")]
    CatalogIncomplete { portal_type: &'static str, reason: String },

    #[error("stalker portal response status {status} for {action}")]
    BadStatus { status: u16, action: String, body_snippet: String },

    #[error("stalker client exhausted all bootstrap recipes for portal {portal}")]
    RecipesExhausted { portal: String },

    #[error("stalker URL factory found no candidate endpoint for {portal}")]
    NoEndpoint { portal: String },

    #[error("stalker device profile is invalid: {message}")]
    InvalidProfile { message: String },

    #[error("stalker portal request failure: {0}")]
    RequestBuild(reqwest::Error),

    #[error("stalker portal I/O error: {0}")]
    Io(#[from] io::Error),
}

impl From<reqwest::Error> for StalkerError {
    fn from(err: reqwest::Error) -> Self {
        // Strip the URL before wrapping: `reqwest::Error`'s `Display` includes the full
        // request URL, which may carry `token=`/`mac=` query parameters that must not
        // leak into logs.
        Self::RequestBuild(err.without_url())
    }
}

impl StalkerError {
    /// Status codes that the Stalker portal returns when a token has gone stale. Reverse-proxy
    /// code uses this to decide whether to retry `create_link` for the same item.
    ///
    /// In addition to HTTP-level 401/403/456/204 the Stalker/Ministra middleware surfaces
    /// account-blocked / token-revoked conditions inside a `200 OK` response body as a
    /// `code` field (e.g. `{"code": 44, "text": "Account is blocked"}`). Those portal-internal
    /// codes 44 and 440..=449 are mapped to a synthetic client error so the proxy retry path
    /// can react to them.
    pub fn is_token_rejected(&self) -> bool {
        match self {
            Self::TokenRejected { status, .. } => matches!(*status, 204 | 401 | 403 | 456),
            Self::BadStatus { status, .. } => matches!(*status, 401 | 403 | 456 | 204),
            Self::PortalBodyError { code, .. } => matches!(*code, 44 | 440..=449),
            _ => false,
        }
    }

    pub fn is_unsupported_catalog_action(&self) -> bool {
        match self {
            Self::BodyDecode { .. } | Self::HtmlResponse { .. } => true,
            Self::EmptyBody { action } => action == "get_all_channels",
            Self::BadStatus { status, action, .. } => {
                action == "get_all_channels" && matches!(*status, 400 | 404 | 405 | 501)
            }
            Self::PortalBodyError { action, .. } => action == "get_all_channels" && !self.is_token_rejected(),
            _ => false,
        }
    }
}

/// What kind of failure this is, independent of which variant carried it.
///
/// Callers were pattern-matching on variants to answer questions the variants were never
/// organised around — "should I retry this?", "is the provider down or is my token
/// stale?". The sibling `tuliprox-media-server` crate answers the same questions with a
/// `MediaServerErrorKind`; this is its counterpart, and it is what
/// [`StalkerError::is_retryable`] is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StalkerErrorKind {
    /// The portal rejected our identity: a stale token, a blocked account.
    /// Re-handshaking is the fix.
    Auth,
    /// The portal or the network failed in a way that may not repeat.
    Transient,
    /// The portal answered, but not in a shape we can use: HTML, bad JSON, a missing
    /// field. Retrying the same call gets the same answer.
    Protocol,
    /// A limit we imposed was hit — a body cap, the configured page limit.
    Capacity,
    /// The input is misconfigured: no reachable endpoint, an invalid device profile.
    Config,
}

impl StalkerError {
    /// Classify the failure. See [`StalkerErrorKind`].
    #[must_use]
    pub fn kind(&self) -> StalkerErrorKind {
        if self.is_token_rejected() {
            return StalkerErrorKind::Auth;
        }
        match self {
            Self::TokenRejected { .. } | Self::HandshakeFailed { .. } | Self::RecipesExhausted { .. } => {
                StalkerErrorKind::Auth
            }
            Self::ResponseTooLarge { .. } | Self::CatalogIncomplete { .. } => StalkerErrorKind::Capacity,
            Self::NoEndpoint { .. } | Self::InvalidProfile { .. } | Self::UnsupportedScheme { .. } => {
                StalkerErrorKind::Config
            }
            Self::BodyDecode { .. }
            | Self::HtmlResponse { .. }
            | Self::EmptyBody { .. }
            | Self::PortalRefusedCmd { .. }
            | Self::PortalBodyError { .. } => StalkerErrorKind::Protocol,
            Self::BadStatus { .. } | Self::RequestBuild(_) | Self::Io(_) => StalkerErrorKind::Transient,
        }
    }

    /// Whether trying the same call again could plausibly succeed.
    ///
    /// [`StalkerErrorKind::Auth`] is retryable only after a re-handshake, which is a
    /// different call; it is reported here as not retryable so a caller does not loop on
    /// a rejected token. Use [`Self::is_token_rejected`] for that path.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self.kind(), StalkerErrorKind::Transient)
    }
}

pub type StalkerResult<T> = Result<T, StalkerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_classifies_and_only_transient_failures_are_retried() {
        let cases: [(StalkerError, StalkerErrorKind); 8] = [
            (StalkerError::TokenRejected { status: 401, url: None }, StalkerErrorKind::Auth),
            (
                StalkerError::PortalBodyError { code: 44, action: "vod".into(), body_snippet: String::new() },
                StalkerErrorKind::Auth,
            ),
            (StalkerError::RecipesExhausted { portal: "p".into() }, StalkerErrorKind::Auth),
            (StalkerError::ResponseTooLarge { action: "vod".into(), cap_bytes: 1 }, StalkerErrorKind::Capacity),
            (StalkerError::NoEndpoint { portal: "p".into() }, StalkerErrorKind::Config),
            (StalkerError::HtmlResponse { snippet: "<html>".into() }, StalkerErrorKind::Protocol),
            (
                StalkerError::BadStatus { status: 502, action: "vod".into(), body_snippet: String::new() },
                StalkerErrorKind::Transient,
            ),
            (StalkerError::Io(std::io::Error::other("reset")), StalkerErrorKind::Transient),
        ];

        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "{error}");
            assert_eq!(
                error.is_retryable(),
                expected == StalkerErrorKind::Transient,
                "only a transient failure is worth repeating verbatim: {error}"
            );
        }
    }

    /// A 401 arriving as `BadStatus` rather than `TokenRejected` must still classify as
    /// auth - the two variants describe the same portal answer.
    #[test]
    fn a_rejected_token_classifies_as_auth_whichever_variant_carries_it() {
        let as_status = StalkerError::BadStatus { status: 403, action: "vod".into(), body_snippet: String::new() };
        assert_eq!(as_status.kind(), StalkerErrorKind::Auth);
        assert!(!as_status.is_retryable(), "looping on a rejected token is never right");
    }

    #[test]
    fn no_content_token_rejection_is_recognized() {
        assert!(StalkerError::TokenRejected { status: 204, url: None }.is_token_rejected());
    }

    #[test]
    fn stalker_error_urls_drop_userinfo_query_and_fragment() {
        let raw = "https://user:pass@portal.example/server/load.php?token=secret&mac=00:11:22:33:44:55#fragment";
        let errors = [
            StalkerError::HandshakeFailed { message: "failed".to_string(), url: Some(raw.into()) },
            StalkerError::TokenRejected { status: 401, url: Some(raw.into()) },
        ];

        for error in errors {
            let debug = format!("{error:?}");
            assert!(debug.contains("https://portal.example/server/load.php"));
            for secret in ["user", "pass", "secret", "00:11:22:33:44:55", "fragment"] {
                assert!(!debug.contains(secret), "error debug output leaked {secret}");
            }
        }
    }
}
