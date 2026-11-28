//! The metadata worker schedules the opt-in container-extension sampling.
//!
//! Two invariants belong to the manager rather than to the sampling run: a request for an input whose
//! run is already queued is dropped (the budget is per update), and the marker is released on every
//! path, so a later update can sample again.

use super::super::MetadataUpdateManager;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn manager() -> Arc<MetadataUpdateManager> { Arc::new(MetadataUpdateManager::new(CancellationToken::new())) }

#[tokio::test]
async fn a_second_request_while_the_first_run_is_queued_is_dropped() {
    let manager = manager();
    // Hold the pause gate, so the spawned run cannot pass the window and clear its marker.
    let gate = manager.acquire_update_pause_guard().await;
    let input_name: Arc<str> = "portal".into();

    manager.spawn_container_extension_sampling(Arc::clone(&input_name));
    assert!(manager.sampling_inputs.contains_key(&input_name), "the first request marks the input");
    manager.spawn_container_extension_sampling(Arc::clone(&input_name));
    assert_eq!(manager.sampling_inputs.len(), 1, "the second request does not queue a second run");

    drop(gate);
    for _ in 0..100 {
        if manager.sampling_inputs.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(manager.sampling_inputs.is_empty(), "the marker is released when the run ends");
}

#[tokio::test]
async fn a_shutdown_manager_queues_nothing() {
    let manager = manager();
    manager.shutdown();

    manager.spawn_container_extension_sampling("portal".into());

    assert!(manager.sampling_inputs.is_empty());
}

#[tokio::test]
async fn a_run_without_a_bound_context_clears_its_marker() {
    let manager = manager();
    let input_name: Arc<str> = "portal".into();

    manager.spawn_container_extension_sampling(Arc::clone(&input_name));

    for _ in 0..100 {
        if manager.sampling_inputs.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(manager.sampling_inputs.is_empty(), "an unbound manager must not keep the input marked forever");
}

async fn spawn_sampling_test_portal(
    first_call_done: Arc<tokio::sync::Notify>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind portal");
    let addr = listener.local_addr().expect("local addr");
    let create_link_calls = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let calls = Arc::clone(&create_link_calls);
            let notify = Arc::clone(&first_call_done);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = if req.contains("action=handshake") {
                    r#"{"js":{"token":"tok"}}"#
                } else if req.contains("action=create_link") {
                    if calls.fetch_add(1, Ordering::AcqRel) == 0 {
                        notify.notify_waiters();
                    }
                    r#"{"js":{"cmd":"ffmpeg http://cdn.example/movie/1.mkv"}}"#
                } else {
                    r#"{"js":[]}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, handle)
}

fn build_sampling_test_app_config(
    storage_dir: &std::path::Path,
    portal_addr: std::net::SocketAddr,
) -> (Arc<tuliprox_core::model::AppConfig>, tuliprox_core::model::Config, Arc<tuliprox_core::model::ConfigInput>) {
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{ConfigInputOptionsDto, ConfigPaths, ContainerExtensionSamplingDto, InputType},
        utils::Internable,
    };
    use tuliprox_core::{
        model::{
            AppConfig, Config, ConfigInput, ConfigInputOptions, MediaToolCapabilities, SourcesConfig,
            StalkerDeviceProfile, StalkerInputConfig,
        },
        utils::FileLockManager,
    };

    let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
        container_extension_sampling: ContainerExtensionSamplingDto {
            enabled: true,
            max_calls_per_update: 2,
            min_interval_secs: 60,
            stop_after_refusals: 2,
        },
        ..ConfigInputOptionsDto::default()
    });
    let stalker_cfg = StalkerInputConfig {
        device: Some(StalkerDeviceProfile { mac_address: Some("00:1A:79:00:00:0A".to_string()), ..Default::default() }),
        ..StalkerInputConfig::default()
    };
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "portal".intern(),
        input_type: InputType::Stalker,
        url: format!("http://{portal_addr}/stalker_portal/c/"),
        enabled: true,
        options: Some(options),
        stalker: Some(stalker_cfg),
        ..ConfigInput::default()
    });
    let config = Config { storage_dir: storage_dir.display().to_string(), ..Config::default() };
    let app_config = Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config.clone())),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            ..SourcesConfig::default()
        })),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        plugins: Arc::default(),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: storage_dir.display().to_string(),
            config_path: storage_dir.display().to_string(),
            storage_path: storage_dir.display().to_string(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
        failover: Arc::default(),
    });
    (app_config, config, input)
}

async fn bind_sampling_test_ctx(
    manager: &MetadataUpdateManager,
    storage_dir: &std::path::Path,
    portal_addr: std::net::SocketAddr,
) -> Arc<str> {
    use arc_swap::ArcSwapOption;
    use shared::{
        model::{stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem},
        utils::Internable,
    };
    use tuliprox_core::model::{HttpClients, MaintenanceRegistry, UpdateGuard};
    use tuliprox_repository::{
        stalker_generation_repository::{save_active_manifest, ClusterFiles, StalkerActiveManifest},
        stalker_repository::{ensure_stalker_storage_path, get_stalker_file_path, persist_stalker_items},
        PlaylistStorageState,
    };
    use tuliprox_session::{ActiveProviderManager, ActiveUserManager, ConnectionManager, EventManager};

    let (app_config, config, input) = build_sampling_test_app_config(storage_dir, portal_addr);
    let storage = ensure_stalker_storage_path(&app_config, &input.name).await.expect("stalker storage");
    let items = vec![
        StalkerPlaylistItem {
            stream_id: 1,
            name: "One".intern(),
            stream_kind: StalkerStreamKind::Movie,
            cmd: "ffmpeg http://portal/1".intern(),
            ..StalkerPlaylistItem::default()
        },
        StalkerPlaylistItem {
            stream_id: 2,
            name: "Two".intern(),
            stream_kind: StalkerStreamKind::Movie,
            cmd: "ffmpeg http://portal/2".intern(),
            ..StalkerPlaylistItem::default()
        },
    ];
    persist_stalker_items(&app_config, &storage, StalkerStreamKind::Movie, &items).await.expect("persist vod");
    let fingerprint = input.stalker.as_ref().expect("stalker cfg").identity_fingerprint(&input.url);
    let mut manifest = StalkerActiveManifest::empty(fingerprint);
    manifest.vod =
        Some(ClusterFiles { generation: 1, data: get_stalker_file_path(&storage, StalkerStreamKind::Movie) });
    save_active_manifest(&storage, &manifest).await.expect("save manifest");

    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_config, &event_manager));
    let shared_stream_manager = Arc::new(tuliprox_session::SharedStreamManager::new(Arc::clone(&active_provider)));
    let geoip = Arc::new(ArcSwapOption::default());
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));
    let client = reqwest::Client::new();
    let http_clients =
        Arc::new(HttpClients::new(client.clone(), client.clone(), client.clone(), client.clone(), client));

    manager
        .set_ctx(crate::ctx::BoundMetadataUpdateCtx {
            app_config,
            active_provider,
            connection_manager,
            events: event_manager,
            playlists: Arc::new(PlaylistStorageState::new()),
            update_guard: UpdateGuard::new(),
            http_clients,
            maintenance: Arc::new(MaintenanceRegistry::in_memory()),
            plugins: crate::ctx::WorkerPlugins::default(),
        })
        .await;
    Arc::clone(&input.name)
}

#[tokio::test]
async fn update_pause_gate_is_released_during_sampling_interval_sleep() {
    use std::time::Duration;
    use tokio::sync::Notify;

    let dir = tempfile::tempdir().expect("tempdir");
    let first_call_done = Arc::new(Notify::new());
    let (addr, server_handle) = spawn_sampling_test_portal(Arc::clone(&first_call_done)).await;
    let manager = manager();
    let input_name = bind_sampling_test_ctx(&manager, dir.path(), addr).await;

    manager.spawn_container_extension_sampling(input_name);
    tokio::time::timeout(Duration::from_secs(5), first_call_done.notified())
        .await
        .expect("first create_link call runs promptly");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let acquired = tokio::time::timeout(Duration::from_millis(200), manager.acquire_update_pause_guard()).await;
    manager.shutdown();
    server_handle.abort();
    assert!(
        acquired.is_ok(),
        "playlist update must acquire update_pause_guard without waiting for the 60s sampling interval sleep"
    );
}
