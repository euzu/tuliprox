//! The handles the DVR needs out of the running server.
//!
//! The supervisors, the rule scheduler and the recording service each used to
//! take `&Arc<AppState>` and reach into it for the same four fields. Naming the
//! four directly keeps the DVR independent of the shape of the server's root
//! state, which is what lets it live outside `api`.

use crate::download::DownloadQueue;
use shared::model::EventSink;
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, HttpClients};

/// Everything the DVR reads from the running server.
#[derive(Clone)]
pub struct RecordingCtx<E: EventSink> {
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// The recording queue: scheduled, active and finished tasks.
    pub downloads: Arc<DownloadQueue>,
    /// Where `RecordingChanged` and `RecordingRulesChanged` are published.
    pub events: E,
    /// Outbound clients, swapped when the proxy configuration changes.
    pub http_clients: Arc<HttpClients>,
}

impl<E: EventSink + Clone + 'static> RecordingCtx<E> {
    /// The configured storage directory.
    pub fn storage_dir(&self) -> String { self.app_config.config.load().storage_dir.clone() }
}
