use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, ConnectionManager, EpgScanCoordinator, EventManager,
        PlaylistStorageState, SharedStreamManager, StalkerResolveCoordinator,
    },
    model::{
        AppConfig, GracePeriodOptions, HdHomeRunDeviceConfig, HttpClients, MaintenanceRegistry, MaintenanceWrite,
        ReverseProxyDisabledHeaderConfig,
    },
    repository::GeoIp,
    utils::LRUResourceCache,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::EventMessage;
use std::sync::{atomic::AtomicI8, Arc};
use tokio::sync::RwLock;
use tuliprox_metadata::manager::MetadataUpdateManager;

mod auth_state;
mod cancel_tokens;
mod change_detection;
mod hls_state;
mod http_clients;
mod network_access;
mod playlist_update_control;
mod recording_state;
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
    auth_state::*, cancel_tokens::*, hls_state::*, http_clients::*, playlist_update_control::*, recording_state::*,
    reload::*, resource_cache::*, view::*,
};

/// Root server state. It always lives behind one `Arc<AppState>` and is never
/// cloned, so a container that no subsystem shares on its own is held by
/// value; only handles that are passed out individually carry an `Arc`.
pub struct AppState {
    pub app_config: Arc<AppConfig>,
    pub http_clients: Arc<HttpClients>,
    pub cache: Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    pub hls: HlsState,
    pub recording: RecordingState,
    pub(crate) stalker_resolve_coordinator: StalkerResolveCoordinator,
    /// Limits and caches the full scans of the per-target EPG databases (EPG inspection).
    pub(crate) epg_scans: EpgScanCoordinator,
    pub active_users: Arc<ActiveUserManager>,
    pub active_provider: Arc<ActiveProviderManager>,
    pub connection_manager: Arc<ConnectionManager>,
    pub event_manager: Arc<EventManager>,
    pub cancel_tokens: ArcSwap<CancelTokens>,
    pub playlists: Arc<PlaylistStorageState>,
    pub geoip: Arc<ArcSwapOption<GeoIp>>,
    pub metadata_manager: Arc<MetadataUpdateManager>,
    pub auth: AuthState,
    pub playlist_updates: PlaylistUpdateControl,
    /// Inputs taken out of service; read on every new stream request.
    pub maintenance: Arc<MaintenanceRegistry>,
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
