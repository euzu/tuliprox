//! What opening and reading a provider stream needs out of the running server.
//!
//! The client side of a proxied stream is not here. It stays in `api`, because
//! it reaches panel provisioning, which reaches the whole root state - see the
//! note in `api::model::streams`.

use crate::connection_manager::ConnectionManager;
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, HttpClients};

/// What opening and reading a provider stream needs.
#[derive(Clone)]
pub struct ProviderStreamCtx {
    pub active_provider: Arc<crate::ActiveProviderManager>,
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// Connection admission and teardown for the provider side.
    pub connection_manager: Arc<ConnectionManager>,
    /// Outbound clients; provider requests use the no-redirect variants.
    pub http_clients: Arc<HttpClients>,
}
