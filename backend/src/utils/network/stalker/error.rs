use std::io;
use thiserror::Error;
use url::Url;

/// Failure modes surfaced by the Stalker portal client. The variants cover both transport
/// errors (network, body caps, JSONP decoding) and protocol-level errors (token rejection,
/// portal refusal, missing playable URL). `Status`-flavored variants carry the upstream
/// status code so reverse-proxy code can decide whether to trigger a `create_link` retry.
#[derive(Debug, Error)]
pub enum StalkerError {
    #[error("stalker portal handshake failed: {message}")]
    HandshakeFailed { message: String, url: Option<Url> },

    #[error("stalker portal rejected the token (status {status})")]
    TokenRejected { status: u16, url: Option<Url> },

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

pub type StalkerResult<T> = Result<T, StalkerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_content_token_rejection_is_recognized() {
        assert!(StalkerError::TokenRejected {
            status: 204,
            url: None,
        }
        .is_token_rejected());
    }
}
