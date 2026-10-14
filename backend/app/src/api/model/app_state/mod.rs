use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, ConnectionManager, EventManager, PlaylistStorage,
        PlaylistStorageState, RecordingQueue, SharedStreamManager, StalkerResolveCoordinator,
    },
    model::{
        AppConfig, GracePeriodOptions, HdHomeRunDeviceConfig, HttpClients, MaintenanceRegistry, MaintenanceWrite,
        ReverseProxyDisabledHeaderConfig,
    },
    repository::GeoIp,
    utils::LRUResourceCache,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{EventMessage, UserConnectionPermission};
use std::sync::{atomic::AtomicI8, Arc};
use tokio::sync::RwLock;
use tuliprox_metadata::manager::MetadataUpdateManager;

mod auth_state;
mod cancel_tokens;
mod change_detection;
mod hls_state;
mod http_clients;
mod playlist_update_control;
mod reload;
mod resource_cache;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod view;

#[cfg(test)]
pub use self::test_support::{create_test_app_state, create_test_maintenance};
pub use self::{
    auth_state::*, cancel_tokens::*, hls_state::*, http_clients::*, playlist_update_control::*, reload::*,
    resource_cache::*, view::*,
};

/// Root server state. It always lives behind one `Arc<AppState>` and is never
/// cloned, so a container that no subsystem shares on its own is held by
/// value; only handles that are passed out individually carry an `Arc`.
pub struct AppState {
    pub app_config: Arc<AppConfig>,
    pub http_clients: Arc<HttpClients>,
    pub recordings: Arc<RecordingQueue>,
    pub cache: Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    pub hls: HlsState,
    pub(crate) stalker_resolve_coordinator: StalkerResolveCoordinator,
    pub active_users: Arc<ActiveUserManager>,
    pub active_provider: Arc<ActiveProviderManager>,
    pub connection_manager: Arc<ConnectionManager>,
    /// Provider capacity as the DVR sees it; the adapter that keeps provider
    /// details out of the recording engine.
    pub recording_capacity: Arc<dyn tuliprox_dvr::recording::recording_capacity::RecordingCapacityPort>,
    pub event_manager: Arc<EventManager>,
    pub cancel_tokens: ArcSwap<CancelTokens>,
    pub playlists: Arc<PlaylistStorageState>,
    pub geoip: Arc<ArcSwapOption<GeoIp>>,
    pub metadata_manager: Arc<MetadataUpdateManager>,
    pub auth: AuthState,
    pub playlist_updates: PlaylistUpdateControl,
    /// Inputs taken out of service; read on every new stream request.
    pub maintenance: Arc<MaintenanceRegistry>,
    pub member_health: Arc<tuliprox_session::MemberHealthIndex>,
}

impl AppState {
    /// The configured input, unless it is in maintenance: for code that would contact its provider.
    pub fn contactable_input(&self, input_name: &Arc<str>) -> Option<Arc<crate::model::ConfigInput>> {
        self.app_config.get_input_by_name(input_name).filter(|input| !self.maintenance.is_in_maintenance(&input.name))
    }

    pub fn is_known_input_name(&self, input_name: &str) -> bool {
        self.app_config.sources.load().inputs.iter().any(|input| input.name.as_ref() == input_name)
    }

    pub fn known_input_names(&self) -> Vec<Arc<str>> {
        self.app_config.sources.load().inputs.iter().map(|input| Arc::clone(&input.name)).collect()
    }

    /// Emits the change event for a write that changed something.
    pub fn publish_maintenance(&self, write: MaintenanceWrite) -> std::io::Result<()> {
        if let Some(entries) = write? {
            self.event_manager.send_event(EventMessage::MaintenanceChanged(entries));
        }
        Ok(())
    }

    pub async fn get_active_connections_for_user(&self, username: &str) -> u32 {
        self.active_users.user_connections(username).await
    }

    pub async fn get_connection_permission(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
    ) -> UserConnectionPermission {
        self.active_users.connection_permission(username, max_connections, soft_connections).await
    }

    pub async fn cache_playlist(&self, target_name: &str, playlist: PlaylistStorage) {
        self.playlists.cache_playlist(target_name, playlist).await;
    }

    pub fn get_disabled_headers(&self) -> Option<ReverseProxyDisabledHeaderConfig> {
        self.app_config.get_disabled_headers()
    }

    pub fn get_grace_options(&self) -> GracePeriodOptions { self.app_config.get_grace_options() }

    pub fn should_use_manual_redirects(&self) -> bool { crate::model::should_use_manual_redirects(&self.app_config) }

    pub fn get_encrypt_secret(&self) -> [u8; 16] { self.app_config.get_encrypt_secret() }
}

#[derive(Clone)]
pub struct HdHomerunAppState {
    pub app_state: Arc<AppState>,
    pub device: Arc<HdHomeRunDeviceConfig>,
    pub hd_scan_state: Arc<AtomicI8>,
}

pub struct AppQosAggregationListener {
    pub member_health: Arc<tuliprox_session::MemberHealthIndex>,
}

impl tuliprox_session::qos_aggregation_manager::QosAggregationListener for AppQosAggregationListener {
    fn on_snapshots(&self, snapshots: &[tuliprox_repository::QosSnapshotRecord]) {
        let entries = snapshots.iter().map(|record| {
            let key = if record.provider_id > 0 {
                tuliprox_session::QosKey::Provider {
                    input: Arc::clone(&record.input_name),
                    provider_id: record.provider_id,
                }
            } else {
                tuliprox_session::QosKey::TargetMember {
                    target: Arc::clone(&record.target_name),
                    member_vid: record.virtual_id,
                }
            };
            let part = tuliprox_session::QosPart {
                score_24h: record.window_24h.score,
                score_7d: record.window_7d.score,
                confidence: record.window_24h.confidence,
                failure_streak: record.window_24h.successive_failure_streak,
            };
            (key, part)
        });
        self.member_health.replace_qos(entries);
    }
}
