//! The handles the HLS proxy needs out of the running server.
//!
//! The refresh loop, the availability evaluator, the garbage collector and the
//! playback paths each used to take `&Arc<AppState>` and reach into it for the
//! same five fields. Naming those five keeps the HLS proxy independent of the
//! shape of the server's root state.

use crate::manager::HlsProxyManager;
use std::sync::{Arc, Weak};
use tuliprox_core::model::AppConfig;
use tuliprox_session::{ActiveProviderManager, ActiveUserManager, ConnectionManager};

/// Everything the HLS proxy reads from the running server.
#[derive(Clone)]
pub struct HlsCtx {
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// The HLS proxy itself: sessions, leases and the object cache.
    pub hls_proxy: Arc<HlsProxyManager>,
    /// Provider allocation, for the upstream a refresh fetches from.
    pub active_provider: Arc<ActiveProviderManager>,
    /// Connection admission and teardown.
    pub connection_manager: Arc<ConnectionManager>,
    /// Per-user session accounting.
    pub active_users: Arc<ActiveUserManager>,
}

impl HlsCtx {
    /// A weak handle, for state the proxy itself owns.
    pub fn downgrade(&self) -> WeakHlsCtx {
        WeakHlsCtx {
            app_config: Arc::clone(&self.app_config),
            hls_proxy: Arc::downgrade(&self.hls_proxy),
            active_provider: Arc::clone(&self.active_provider),
            connection_manager: Arc::clone(&self.connection_manager),
            active_users: Arc::clone(&self.active_users),
        }
    }
}

/// An [`HlsCtx`] whose reference to the proxy is weak.
///
/// State owned by the proxy cannot hold the proxy strongly - that is a cycle,
/// and the refresh runtime stored inside a session is exactly such state. Only
/// `hls_proxy` participates in the cycle, so only it is weakened.
#[derive(Clone)]
pub struct WeakHlsCtx {
    app_config: Arc<AppConfig>,
    hls_proxy: Weak<HlsProxyManager>,
    active_provider: Arc<ActiveProviderManager>,
    connection_manager: Arc<ConnectionManager>,
    active_users: Arc<ActiveUserManager>,
}

impl WeakHlsCtx {
    /// `None` once the proxy has been dropped.
    pub fn upgrade(&self) -> Option<HlsCtx> {
        Some(HlsCtx {
            app_config: Arc::clone(&self.app_config),
            hls_proxy: self.hls_proxy.upgrade()?,
            active_provider: Arc::clone(&self.active_provider),
            connection_manager: Arc::clone(&self.connection_manager),
            active_users: Arc::clone(&self.active_users),
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl HlsCtx {
    /// An [`HlsCtx`] over `config`, with the surrounding managers wired the way
    /// the server wires them.
    ///
    /// Tests used to build a whole `AppState` to get here. This constructs only
    /// what the proxy actually reads.
    #[must_use]
    pub fn for_test(config: tuliprox_core::model::Config) -> Self {
        use arc_swap::{ArcSwap, ArcSwapOption};
        use tuliprox_session::SharedStreamManager;

        let app_config = Arc::new(tuliprox_core::model::AppConfig {
            config: Arc::new(ArcSwap::from_pointee(config)),
            sources: Arc::new(ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
        });

        let event_manager = Arc::new(tuliprox_session::EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_config, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let loaded = app_config.config.load();
        let geoip = Arc::new(ArcSwapOption::default());
        let active_users = Arc::new(ActiveUserManager::new(&loaded, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        Self {
            hls_proxy: Arc::new(crate::manager::HlsProxyManager::new()),
            app_config: Arc::clone(&app_config),
            active_provider,
            connection_manager,
            active_users,
        }
    }
}
