//! What opening and reading a provider stream needs out of the running server.
//!
//! The client side of a proxied stream is not here. It stays in `api`, because
//! it reaches panel provisioning, which reaches the whole root state - see the
//! note in `api::model::streams`.

use crate::connection_manager::ConnectionManager;
use arc_swap::ArcSwap;
use reqwest::Client;
use std::sync::Arc;
use tuliprox_core::model::AppConfig;

/// What opening and reading a provider stream needs.
#[derive(Clone)]
pub struct ProviderStreamCtx {
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// Connection admission and teardown for the provider side.
    pub connection_manager: Arc<ConnectionManager>,
    /// Client that does not follow redirects, for provider requests.
    pub http_client_no_redirect: Arc<ArcSwap<Client>>,
    /// The same, but without proxy configuration applied.
    pub public_http_client_no_redirect: Arc<ArcSwap<Client>>,
}
