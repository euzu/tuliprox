//! Outbound fetches of the marketplace: a catalog and a package. Both follow redirects by hand on a
//! no-redirect client, re-check the scheme on every hop, count the body while
//! it streams (never allocating from `Content-Length`) and run under a
//! whole-fetch deadline. Package hops are also classified, so a catalog cannot
//! point the host at its own loopback or link-local addresses.

use http::StatusCode;
use reqwest::{
    header::{HeaderMap, HeaderValue, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION},
    Client, Url,
};
use shared::model::ENV_PLUGIN_MARKETPLACE_ALLOW_HTTP;
use std::{fmt, time::Duration};
use tuliprox_core::utils::request::{classify_resource_destination, ResourceDestination};

/// Largest catalog body accepted.
pub const CATALOG_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Largest package body the install route accepts (the upload cap).
pub const PACKAGE_MAX_BYTES: usize = 32 * 1024 * 1024;
const CATALOG_DEADLINE: Duration = Duration::from_secs(30);
const PACKAGE_DEADLINE: Duration = Duration::from_secs(120);
const MAX_REDIRECTS: usize = 5;

/// What the developer knob allows. Built by the caller and passed down; the
/// fetch code never reads the environment itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FetchPolicy {
    /// `http://` URLs and `Blocked` destinations are permitted.
    pub allow_http: bool,
}

impl FetchPolicy {
    /// Read `TULIPROX_PLUGIN_MARKETPLACE_ALLOW_HTTP` (`1` turns it on). Logs a
    /// warning on every call while it is on.
    #[must_use]
    pub fn from_env() -> Self {
        let allow_http = std::env::var(ENV_PLUGIN_MARKETPLACE_ALLOW_HTTP).is_ok_and(|value| value == "1");
        if allow_http {
            log::warn!(
                "{ENV_PLUGIN_MARKETPLACE_ALLOW_HTTP}=1: marketplace fetches may use http:// and local destinations"
            );
        }
        Self { allow_http }
    }
}

/// A marketplace fetch, sync or store failure. [`fmt::Display`] is sanitized:
/// it never carries a URL, a header or a response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketplaceError {
    /// The URL or a redirect hop breaks the scheme or credential rules.
    Refused(&'static str),
    /// The destination is local to the host and the knob is off.
    ForbiddenDestination(String),
    TooManyRedirects,
    TooLarge(usize),
    Timeout,
    /// The server answered with this status.
    Status(u16),
    /// Connecting, TLS or reading the body failed.
    Network(&'static str),
    /// The catalog body is not acceptable (parse failure).
    Catalog(String),
    /// The cache could not be written.
    Storage(String),
}

impl MarketplaceError {
    /// The HTTP status the API answers with.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::ForbiddenDestination(_) => StatusCode::FORBIDDEN,
            Self::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

impl fmt::Display for MarketplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(reason) => write!(f, "refused: {reason}"),
            Self::ForbiddenDestination(host) => write!(f, "refused: `{host}` is a local destination"),
            Self::TooManyRedirects => write!(f, "too many redirects (limit {MAX_REDIRECTS})"),
            Self::TooLarge(limit) => write!(f, "the download is larger than {limit} bytes"),
            Self::Timeout => write!(f, "the download timed out"),
            Self::Status(code) => write!(f, "the server answered HTTP {code}"),
            Self::Network(what) => write!(f, "{what}"),
            Self::Catalog(problem) => write!(f, "invalid catalog: {problem}"),
            Self::Storage(problem) => write!(f, "cannot store the catalog: {problem}"),
        }
    }
}

impl std::error::Error for MarketplaceError {}

/// The result of a catalog fetch.
#[derive(Debug)]
pub(crate) enum CatalogFetch {
    Fresh { bytes: Vec<u8>, etag: Option<String>, last_modified: Option<String> },
    NotModified,
}

/// Fetch a catalog, conditionally when the validators of the cached copy are
/// given. The URL is admin-entered, so its destination is not classified.
pub(crate) async fn fetch_catalog(
    client: &Client,
    policy: FetchPolicy,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<CatalogFetch, MarketplaceError> {
    let mut conditional = HeaderMap::new();
    if let Some(value) = etag.and_then(|value| HeaderValue::from_str(value).ok()) {
        conditional.insert(IF_NONE_MATCH, value);
    }
    if let Some(value) = last_modified.and_then(|value| HeaderValue::from_str(value).ok()) {
        conditional.insert(IF_MODIFIED_SINCE, value);
    }
    let request =
        Request { client, policy, classify: false, conditional, cap: CATALOG_MAX_BYTES, deadline: CATALOG_DEADLINE };
    request.get(url).await
}

/// Download a package. Every hop must be `https` (`http` under the knob) and,
/// unless the knob is on, a destination that is not `Blocked`.
///
/// # Errors
/// [`MarketplaceError`]: refused scheme or destination, too many redirects,
/// over `max_bytes`, a timeout, an HTTP error status or a network failure.
pub async fn fetch_package(
    client: &Client,
    policy: FetchPolicy,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, MarketplaceError> {
    let request = Request {
        client,
        policy,
        classify: true,
        conditional: HeaderMap::new(),
        cap: max_bytes,
        deadline: PACKAGE_DEADLINE,
    };
    match request.get(url).await? {
        CatalogFetch::Fresh { bytes, .. } => Ok(bytes),
        CatalogFetch::NotModified => Err(MarketplaceError::Status(StatusCode::NOT_MODIFIED.as_u16())),
    }
}

struct Request<'a> {
    client: &'a Client,
    policy: FetchPolicy,
    classify: bool,
    conditional: HeaderMap,
    cap: usize,
    deadline: Duration,
}

impl Request<'_> {
    async fn get(&self, url: &str) -> Result<CatalogFetch, MarketplaceError> {
        tokio::time::timeout(self.deadline, self.follow(url)).await.map_err(|_| MarketplaceError::Timeout)?
    }

    async fn check_hop(&self, url: &Url) -> Result<(), MarketplaceError> {
        match url.scheme() {
            "https" => {}
            "http" if self.policy.allow_http => {}
            _ => return Err(MarketplaceError::Refused("only https URLs are allowed")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(MarketplaceError::Refused("URLs with credentials are not allowed"));
        }
        let Some(host) = url.host_str() else { return Err(MarketplaceError::Refused("the URL has no host")) };
        if self.classify
            && !self.policy.allow_http
            && classify_resource_destination(host).await == ResourceDestination::Blocked
        {
            return Err(MarketplaceError::ForbiddenDestination(host.to_string()));
        }
        Ok(())
    }

    async fn follow(&self, url: &str) -> Result<CatalogFetch, MarketplaceError> {
        let mut current = Url::parse(url.trim()).map_err(|_| MarketplaceError::Refused("the URL is not valid"))?;
        let conditional = !self.conditional.is_empty();
        for hop in 0..=MAX_REDIRECTS {
            self.check_hop(&current).await?;
            let response = self
                .client
                .get(current.clone())
                .headers(self.conditional.clone())
                .send()
                .await
                .map_err(|err| network_error(&err))?;
            let status = response.status();
            if status == StatusCode::NOT_MODIFIED && conditional {
                return Ok(CatalogFetch::NotModified);
            }
            if status.is_redirection() {
                if hop == MAX_REDIRECTS {
                    return Err(MarketplaceError::TooManyRedirects);
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(MarketplaceError::Status(status.as_u16()))?;
                current = current.join(location).map_err(|_| MarketplaceError::Refused("the redirect is not valid"))?;
                continue;
            }
            if !status.is_success() {
                return Err(MarketplaceError::Status(status.as_u16()));
            }
            return self.read_body(response).await;
        }
        Err(MarketplaceError::TooManyRedirects)
    }

    async fn read_body(&self, mut response: reqwest::Response) -> Result<CatalogFetch, MarketplaceError> {
        let header = |name| response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
        let (etag, last_modified) = (header(ETAG), header(LAST_MODIFIED));
        if response.content_length().is_some_and(|length| length > self.cap as u64) {
            return Err(MarketplaceError::TooLarge(self.cap));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|err| network_error(&err))? {
            if bytes.len() + chunk.len() > self.cap {
                return Err(MarketplaceError::TooLarge(self.cap));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(CatalogFetch::Fresh { bytes, etag, last_modified })
    }
}

fn network_error(err: &reqwest::Error) -> MarketplaceError {
    if err.is_timeout() {
        MarketplaceError::Timeout
    } else if err.is_connect() {
        MarketplaceError::Network("cannot connect to the server")
    } else {
        MarketplaceError::Network("the request failed")
    }
}

#[cfg(test)]
mod tests;
