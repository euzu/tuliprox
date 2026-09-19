//! The handles the metadata worker reads out of the running server.
//!
//! The worker used to hold a `Weak<AppState>`, set after construction because
//! the root state does not exist yet when the worker is built, and weak because
//! the root state owns the worker. Naming the eight handles it actually reads
//! removes the cycle - none of them reach back here - so they are held strongly.
//! The late binding stays: they are still constructed after the worker.

use arc_swap::ArcSwap;
use reqwest::Client;
use shared::model::EventSink;
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, UpdateGuard};
use tuliprox_repository::PlaylistStorageState;
use tuliprox_session::{ActiveProviderManager, ConnectionManager, EventManager};

/// Everything the metadata worker reads from the running server.
#[derive(Clone)]
pub struct MetadataUpdateCtx<E: EventSink> {
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// Provider allocation, for the upstream a probe fetches from.
    pub active_provider: Arc<ActiveProviderManager>,
    /// Connection admission and teardown for probe requests.
    pub connection_manager: Arc<ConnectionManager>,
    /// Where metadata progress is published.
    pub events: E,
    /// The in-memory playlist cache the worker writes resolved detail into.
    pub playlists: Arc<PlaylistStorageState>,
    /// Guards a playlist update against concurrent metadata writes.
    pub update_guard: UpdateGuard,
    /// Shared HTTP client, swapped when the proxy configuration changes.
    pub http_client: Arc<ArcSwap<Client>>,
    /// Client that does not follow redirects, for provider requests.
    pub http_client_no_redirect: Arc<ArcSwap<Client>>,
}

/// The instantiation the long-lived worker stores.
///
/// Functions that only read a context stay generic over the sink, so they
/// can be exercised against `NoopSink`. `MetadataUpdateManager` itself is
/// held by `AppState` as one concrete `Arc`, so it has to name one sink -
/// making it generic would make `AppState` generic, and then everything
/// that touches `AppState`.
pub type BoundMetadataUpdateCtx = MetadataUpdateCtx<Arc<EventManager>>;
