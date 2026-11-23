//! Adapts the server's connection managers to the DVR's capacity port.
//!
//! Everything provider-shaped stays on this side of the boundary. The adapter
//! does not write files, choose between ffmpeg and HTTP, or change a
//! recording's state; it answers whether there is a connection to record with
//! and hands one over. That is the whole of its job, and keeping it that small
//! is what lets the DVR be tested without a provider.

use futures::future::BoxFuture;
use shared::model::XtreamCluster;
use std::sync::Arc;
use tokio::sync::Notify;
use tuliprox_core::model::{AppConfig, MaintenanceGate, ProviderHandle};
use tuliprox_dvr::recording::recording_capacity::{ProviderCapacity, RecordingCapacityPort};
use tuliprox_repository::PlaylistStorageState;
use tuliprox_session::{ActiveProviderManager, ConnectionManager, SharedStreamManager};

/// The running server's provider capacity, as the DVR sees it.
pub struct ProviderCapacityAdapter {
    active_provider: Arc<ActiveProviderManager>,
    connection_manager: Arc<ConnectionManager>,
    maintenance: Arc<dyn MaintenanceGate>,
    app_config: Option<Arc<AppConfig>>,
    playlists: Option<Arc<PlaylistStorageState>>,
    shared_stream_manager: Option<Arc<SharedStreamManager>>,
}

impl ProviderCapacityAdapter {
    pub fn new<G: MaintenanceGate + 'static>(
        active_provider: Arc<ActiveProviderManager>,
        connection_manager: Arc<ConnectionManager>,
        maintenance: Arc<G>,
    ) -> Arc<Self> {
        Arc::new(Self {
            active_provider,
            connection_manager,
            maintenance,
            app_config: None,
            playlists: None,
            shared_stream_manager: None,
        })
    }

    pub fn with_shared_stream_manager<G: MaintenanceGate + 'static>(
        active_provider: Arc<ActiveProviderManager>,
        connection_manager: Arc<ConnectionManager>,
        maintenance: Arc<G>,
        app_config: Arc<AppConfig>,
        playlists: Arc<PlaylistStorageState>,
        shared_stream_manager: Arc<SharedStreamManager>,
    ) -> Arc<Self> {
        Arc::new(Self {
            active_provider,
            connection_manager,
            maintenance,
            app_config: Some(app_config),
            playlists: Some(playlists),
            shared_stream_manager: Some(shared_stream_manager),
        })
    }
}

impl RecordingCapacityPort for ProviderCapacityAdapter {
    fn capacities_for_input<'a>(&'a self, input_name: &'a Arc<str>) -> BoxFuture<'a, Vec<ProviderCapacity>> {
        Box::pin(std::future::ready(self.active_provider.provider_capacities_for_input(input_name)))
    }

    fn acquire<'a>(&'a self, input_name: &'a Arc<str>, priority: i8) -> BoxFuture<'a, Option<ProviderHandle>> {
        Box::pin(std::future::ready(self.active_provider.acquire_connection_for_download(input_name, priority)))
    }

    fn release(&self, handle: Option<ProviderHandle>) -> BoxFuture<'_, ()> {
        self.connection_manager.release_provider_handle(handle);
        Box::pin(std::future::ready(()))
    }

    fn capacity_changed(&self) -> Arc<Notify> { self.connection_manager.capacity_notified() }

    fn is_input_in_maintenance(&self, input_name: &str) -> bool { self.maintenance.is_in_maintenance(input_name) }

    fn shared_stream_available<'a>(
        &'a self,
        target_name: &'a str,
        input_name: &'a str,
        virtual_id: u32,
        cluster: XtreamCluster,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let (Some(app_config), Some(playlists), Some(shared_stream_manager)) =
                (&self.app_config, &self.playlists, &self.shared_stream_manager)
            else {
                return false;
            };

            let resolved = {
                let sources = app_config.sources.load();
                tuliprox_dvr::recording::recording_source_resolution::resolve_recording_config(
                    sources.as_ref(),
                    target_name,
                    input_name,
                )
            };
            let Some(resolved) = resolved else {
                return false;
            };

            if crate::api::api_utils::recording_headers_differ(app_config, &resolved.input) {
                return false;
            }

            if !crate::api::api_utils::is_stream_share_enabled(shared::model::PlaylistItemType::Live, &resolved.target)
            {
                return false;
            }

            let shared_key = crate::api::api_utils::resolve_target_shared_lookup_key(
                app_config,
                playlists,
                &resolved.target,
                &resolved.input,
                virtual_id,
                cluster,
            )
            .await;

            let Some(shared_key) = shared_key else {
                return false;
            };

            shared_stream_manager.get_shared_state(shared_key.as_str()).await.is_some()
        })
    }
}
