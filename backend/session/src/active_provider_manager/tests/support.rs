use super::{ActiveProviderManager, ConnectionKind, PlaybackLeaseRef};
use crate::EventManager;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    defaults::default_user_priority,
    model::{ConfigPaths, InputFetchMethod, InputType},
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{
        AppConfig, Config, ConfigInput, ConfigInputAlias, MediaToolCapabilities, PlaybackKind, PlaybackRequestOutcome,
        SourcesConfig,
    },
    utils::FileLockManager,
};

pub(in crate::active_provider_manager::tests) fn build_test_app_config(
    aliases: Option<Vec<ConfigInputAlias>>,
    max_connections: u16,
) -> AppConfig {
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
        max_connections,
        method: InputFetchMethod::default(),
        aliases,
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
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

pub(in crate::active_provider_manager::tests) fn create_test_app_config_with_dual_provider_pool() -> AppConfig {
    build_test_app_config(
        Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: "http://provider-2.example".to_string(),
            username: Some("user2".to_string()),
            password: Some("pass2".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        1,
    )
}

pub(in crate::active_provider_manager::tests) fn create_test_app_config_single_provider_pool() -> AppConfig {
    build_test_app_config(None, 1)
}

pub(in crate::active_provider_manager::tests) fn create_test_app_config_single_unlimited_provider_pool() -> AppConfig {
    build_test_app_config(None, 0)
}

/// Pool where the higher-priority provider (A) and its lower-priority alias (B)
/// each carry their own capacity, mirroring the reported A=2 / B=3 setup.
pub(in crate::active_provider_manager::tests) fn create_test_app_config_with_pool(
    primary_max: u16,
    alias_max: u16,
) -> AppConfig {
    build_test_app_config(
        Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: "http://provider-2.example".to_string(),
            username: Some("user2".to_string()),
            password: Some("pass2".to_string()),
            priority: 1,
            max_connections: alias_max,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        primary_max,
    )
}

/// Primary `provider_1` with an alias `provider_2` behind it.
pub(in crate::active_provider_manager::tests) fn alias_pool(
    primary_max: u16,
    alias_max: u16,
) -> (ActiveProviderManager, Arc<str>, Arc<str>) {
    let app_cfg = create_test_app_config_with_pool(primary_max, alias_max);
    let manager = ActiveProviderManager::new(&app_cfg, &Arc::new(EventManager::new()));
    (manager, "provider_1".intern(), "provider_2".intern())
}

pub(in crate::active_provider_manager::tests) fn acquire_live_hls_from_lineup(
    manager: &ActiveProviderManager,
    input: &Arc<str>,
    token: &str,
    port: u16,
) -> Result<tuliprox_core::model::ProviderHandle, String> {
    manager
        .acquire_connection_with_lease_for_session(
            input,
            &SocketAddr::from(([172, 18, 0, 9], port)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(token, PlaybackKind::LiveHls)),
        )
        .ok_or_else(|| format!("no provider for {token}"))
}

/// Starts `token` on the alias while the primary is full, confirms media, ends the
/// request cleanly, frees the primary and lets the reconnect lease lapse.
pub(in crate::active_provider_manager::tests) async fn live_hls_playback_on_alias_with_lapsed_lease(
    manager: &ActiveProviderManager,
    input: &Arc<str>,
    alias: &Arc<str>,
    token: &str,
    outcome: PlaybackRequestOutcome,
) -> Result<(), String> {
    let busy_1 = acquire_live_hls_from_lineup(manager, input, "busy-1|bob|7|hls|0123456789abcdef", 50_001)?;
    let busy_2 = acquire_live_hls_from_lineup(manager, input, "busy-2|carol|8|hls|0123456789abcdef", 50_002)?;
    assert_eq!(busy_1.allocation.get_provider_name().as_ref(), Some(input));
    assert_eq!(busy_2.allocation.get_provider_name().as_ref(), Some(input));

    let handle = acquire_live_hls_from_lineup(manager, input, token, 50_010)?;
    let provider = handle.allocation.get_provider_name().ok_or("provider")?;
    assert_eq!(&provider, alias, "full primary must fall back to the alias");
    let request_id = handle.playback_request_id.ok_or("identified request")?;
    manager.refresh_adaptive_playback_lease(&provider, token, PlaybackKind::LiveHls, 15);
    manager.confirm_identified_playback_activity(token, request_id).ok_or("confirmation")?;
    manager.release_handle(&handle);
    manager.finish_identified_playback_request(token, request_id, outcome);

    manager.release_handle(&busy_1);
    manager.release_handle(&busy_2);
    tokio::time::advance(Duration::from_secs(20)).await;
    assert_eq!(manager.provider_lease_usage(alias).total(), 0, "reconnect lease must have lapsed");
    assert_eq!(manager.provider_lease_usage(input).total(), 0);
    Ok(())
}

/// Starts `token` on the alias while the primary is full and confirms media. The
/// request stays attached; the primary is freed again before returning.
pub(in crate::active_provider_manager::tests) fn confirmed_alias_request(
    manager: &ActiveProviderManager,
    input: &Arc<str>,
    alias: &Arc<str>,
    token: &str,
) -> Result<tuliprox_core::model::ProviderHandle, String> {
    let busy_1 = acquire_live_hls_from_lineup(manager, input, "busy-1|bob|7|hls|0123456789abcdef", 50_001)?;
    let busy_2 = acquire_live_hls_from_lineup(manager, input, "busy-2|carol|8|hls|0123456789abcdef", 50_002)?;
    let handle = acquire_live_hls_from_lineup(manager, input, token, 50_010)?;
    assert_eq!(handle.allocation.get_provider_name().as_ref(), Some(alias));
    let request_id = handle.playback_request_id.ok_or("identified request")?;
    manager.refresh_adaptive_playback_lease(alias, token, PlaybackKind::LiveHls, 15);
    manager.confirm_identified_playback_activity(token, request_id).ok_or("confirmation")?;
    manager.release_handle(&busy_1);
    manager.release_handle(&busy_2);
    Ok(handle)
}
