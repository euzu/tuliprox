use crate::api::model::{ActiveProviderManager, ProviderHandle};
use std::sync::Arc;

pub(crate) struct ProbeHandleGuard {
    manager: Arc<ActiveProviderManager>,
    handle: Option<ProviderHandle>,
}

impl ProbeHandleGuard {
    pub(crate) fn new(manager: &Arc<ActiveProviderManager>, handle: ProviderHandle) -> Self {
        Self {
            manager: Arc::clone(manager),
            handle: Some(handle),
        }
    }

    #[inline]
    pub(crate) fn handle(&self) -> Option<&ProviderHandle> { self.handle.as_ref() }

    pub(crate) async fn release(mut self) {
        if let Some(handle) = self.handle.take() {
            self.manager.release_handle(&handle).await;
        }
    }
}

impl Drop for ProbeHandleGuard {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let manager = Arc::clone(&self.manager);
        tokio::spawn(async move {
            manager.release_handle(&handle).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::ProbeHandleGuard;
    use crate::{
        api::model::{ActiveProviderManager, EventManager},
        model::{AppConfig, Config, ConfigInput, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType},
        utils::{default_probe_user_priority, Internable},
    };
    use std::{collections::HashMap, sync::Arc};

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            ..ConfigInput::default()
        });

        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
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
            ffprobe_available: Arc::default(),
        }
    }

    #[tokio::test]
    async fn probe_handle_guard_releases_provider_slot_on_drop() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name = "provider_1".intern();

        let handle = manager
            .acquire_connection_for_probe(&input_name, default_probe_user_priority())
            .await
            .expect("probe allocation should succeed");
        assert_eq!(manager.get_provider_connections_count().await, 1);

        let guard = ProbeHandleGuard::new(&manager, handle);
        drop(guard);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(manager.get_provider_connections_count().await, 0);
    }
}
