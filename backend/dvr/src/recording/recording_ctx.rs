//! The handles the DVR needs out of the running server.
//!
//! The supervisors, the rule scheduler and the recording service each used to
//! take `&Arc<AppState>` and reach into it for the same four fields. Naming the
//! four directly keeps the DVR independent of the shape of the server's root
//! state, which is what lets it live outside `api`.

use crate::recording::recording_queue::RecordingQueue;
use arc_swap::ArcSwap;
use reqwest::Client;
use std::sync::Arc;
use tuliprox_core::model::AppConfig;
use tuliprox_session::{ActiveProviderManager, ConnectionManager, EventManager};

/// Everything the DVR reads from the running server.
#[derive(Clone)]
pub struct RecordingCtx {
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// The recording queue: scheduled, active and finished tasks.
    pub recordings: Arc<RecordingQueue>,
    /// Where `RecordingChanged` and `RecordingRulesChanged` are published.
    pub event_manager: Arc<EventManager>,
    /// Shared HTTP client, swapped when the proxy configuration changes.
    pub http_client: Arc<ArcSwap<Client>>,
    /// Provider capacity, used to acquire a connection slot before a
    /// recording starts and to release it when the task ends.
    pub active_provider: Arc<ActiveProviderManager>,
    /// Connection registry; its capacity signal wakes waiting recordings.
    pub connection_manager: Arc<ConnectionManager>,
}

impl RecordingCtx {
    /// The configured storage directory.
    pub fn storage_dir(&self) -> String { self.app_config.config.load().storage_dir.clone() }
}
