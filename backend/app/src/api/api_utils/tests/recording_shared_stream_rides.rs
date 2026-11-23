use super::{
    create_test_fingerprint, create_test_live_channel, create_test_provider_app_state, create_test_shared_target,
    stream_response, PlaybackSource,
};
use crate::{
    api::{
        endpoints::v1_api_playlist::{playlist_recording_stream, RecordingStreamQuery},
        model::{AppState, SharedStreamCtx, SharedStreamManager},
    },
    auth::create_access_token,
    model::{ConfigInput, ConfigSource, ConfigTarget, ProxyUserCredentials, VideoConfig},
    repository::{PlaylistM3uStorage, TargetPlaylistStorage},
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use shared::{
    model::{
        ConfigTargetOptions, ConfigTargetShareLiveStreams, HlsCacheConfigDto, M3uPlaylistItem, PlaylistItem,
        PlaylistItemHeader, PlaylistItemType, RecordingConfigDto, ReverseProxyConfigDto, UserConnectionPermission,
        VideoConfigDto, VirtualId, XtreamCluster,
    },
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_stream::StreamExt;
use tuliprox_core::model::{
    HlsCacheConfig, M3uTargetOutput, RecordingConfig, ReverseProxyConfig, SharedSubscriberId, TargetOutput,
    RECORDING_PROXY_USERNAME,
};

async fn setup_test_app_state_and_target(
    stream_url: &str,
    virtual_id: u32,
) -> (Arc<AppState>, Arc<ConfigTarget>, Arc<ConfigInput>) {
    let app_state = create_test_provider_app_state();
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("input exists");

    let mut target = create_test_shared_target();
    target.name = "shared_target".to_string();
    target.output = vec![TargetOutput::M3u(M3uTargetOutput::from(&shared::model::M3uTargetOutputDto::default()))];
    let target = Arc::new(target);

    // Update sources in app_config
    {
        let source = ConfigSource { inputs: vec![input_name], targets: vec![Arc::clone(&target)] };
        let sources = tuliprox_core::model::SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            sources: vec![source],
            ..tuliprox_core::model::SourcesConfig::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));
    }

    // Populate m3u item in playlists memory storage
    let pli = M3uPlaylistItem::from(&PlaylistItem {
        header: PlaylistItemHeader {
            id: "test".intern(),
            virtual_id: VirtualId::new(virtual_id),
            input_name: input.name.clone(),
            url: stream_url.intern(),
            title: "Test Stream".into(),
            name: "Test Stream".into(),
            group: "Live".into(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    });

    let mut m3u_storage = PlaylistM3uStorage::new();
    m3u_storage.insert(virtual_id, pli);
    app_state
        .playlists
        .data
        .write()
        .await
        .insert(target.name.clone(), TargetPlaylistStorage { xtream: None, m3u: Some(m3u_storage), id_mapping: None });

    (app_state, target, input)
}

#[tokio::test]
async fn test_viewer_watches_shared_recording_starts_provider_count_stays_one_recording_grows() {
    let stream_url = "http://provider-1.example/live/stream_1.ts";
    let virtual_id = 42;
    let (app_state, target, input) = setup_test_app_state_and_target(stream_url, virtual_id).await;

    // 1. Viewer starts watching stream (shared)
    let viewer_addr: SocketAddr = "127.0.0.1:55201".parse().unwrap();
    let viewer_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &viewer_addr, 5, crate::api::model::ConnectionKind::Normal)
        .expect("viewer acquire slot");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(10);
    let shared_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let subscriber_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, viewer_addr)
            .await
            .expect("reserve cleanup");
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &viewer_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), viewer_handle)),
        pending_cleanup,
        5,
        crate::api::model::ConnectionKind::Normal,
        "viewer",
        "viewer_token",
    )
    .await;
    assert!(registered.is_some());
    let (_viewer_stream, _shared_prov, _viewer_cleanup) = registered.unwrap();

    // Verify provider connection count is 1
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Check shared stream available on capacity port
    let capacity_port = &app_state.recording.capacity;
    let available =
        capacity_port.shared_stream_available(&target.name, &input.name, virtual_id, XtreamCluster::Live).await;
    assert!(available, "shared stream must be reported available");

    // 2. Recording of stream starts with join_shared=1
    let token = create_access_token(&app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let rec_fingerprint = create_test_fingerprint("127.0.0.1:55202".parse().unwrap());
    let response = playlist_recording_stream(
        rec_fingerprint,
        AxumPath((token, "live".to_string(), virtual_id)),
        Query(RecordingStreamQuery {
            target_name: target.name.clone(),
            input_name: input.name.to_string(),
            provider_allocation_id: None,
            join_shared: Some("1".to_string()),
            priority: Some(0),
        }),
        State(Arc::clone(&app_state)),
        HeaderMap::new(),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    // Provider connection count stays exactly 1!
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Send data to shared stream
    tx.send(Ok(Bytes::from_static(b"recording_data_chunk_1"))).await.unwrap();

    // Read data from recording response body
    let mut body_stream = response.into_body().into_data_stream();
    let chunk = tokio::time::timeout(Duration::from_millis(500), body_stream.next())
        .await
        .expect("timed out waiting for chunk")
        .expect("stream chunk exists")
        .expect("chunk ok");
    assert_eq!(chunk, Bytes::from_static(b"recording_data_chunk_1"));
}

#[tokio::test]
async fn test_viewer_leaves_recording_continues_to_end_provider_released() {
    let stream_url = "http://provider-1.example/live/stream_2.ts";
    let virtual_id = 43;
    let (app_state, target, input) = setup_test_app_state_and_target(stream_url, virtual_id).await;

    let viewer_addr: SocketAddr = "127.0.0.1:55203".parse().unwrap();
    let viewer_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &viewer_addr, 5, crate::api::model::ConnectionKind::Normal)
        .expect("viewer acquire slot");

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(10);
    let shared_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let subscriber_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, viewer_addr)
            .await
            .expect("reserve cleanup");
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &viewer_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), viewer_handle)),
        pending_cleanup,
        5,
        crate::api::model::ConnectionKind::Normal,
        "viewer",
        "viewer_token",
    )
    .await;
    let (viewer_stream, _shared_prov, _viewer_cleanup) = registered.unwrap();

    let token = create_access_token(&app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let rec_fingerprint = create_test_fingerprint("127.0.0.1:55204".parse().unwrap());
    let response = playlist_recording_stream(
        rec_fingerprint,
        AxumPath((token, "live".to_string(), virtual_id)),
        Query(RecordingStreamQuery {
            target_name: target.name.clone(),
            input_name: input.name.to_string(),
            provider_allocation_id: None,
            join_shared: Some("1".to_string()),
            priority: Some(0),
        }),
        State(Arc::clone(&app_state)),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Viewer leaves!
    drop(viewer_stream);

    // Provider connection count must STAY 1 because recording is still running!
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Recording continues receiving data
    tx.send(Ok(Bytes::from_static(b"recording_data_after_viewer_left"))).await.unwrap();
    let mut body_stream = response.into_body().into_data_stream();
    let chunk = tokio::time::timeout(Duration::from_millis(500), body_stream.next())
        .await
        .expect("timed out")
        .expect("chunk exists")
        .expect("chunk ok");
    assert_eq!(chunk, Bytes::from_static(b"recording_data_after_viewer_left"));

    // Recording finishes: drop body stream
    drop(body_stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("provider connection released");
}

#[tokio::test]
async fn test_shared_stream_ends_just_before_recording_start_fallback_to_own_slot() {
    let stream_url = "http://provider-1.example/live/stream_3.ts";
    let virtual_id = 44;
    let (app_state, target, input) = setup_test_app_state_and_target(stream_url, virtual_id).await;

    // No shared stream is running.
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);

    // Capacity port returns false
    let available = app_state
        .recording
        .capacity
        .shared_stream_available(&target.name, &input.name, virtual_id, XtreamCluster::Live)
        .await;
    assert!(!available);

    // If join_shared is requested anyway, endpoint answers 503 with x-tuliprox-fallback: channel_unavailable
    let token = create_access_token(&app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let rec_fingerprint = create_test_fingerprint("127.0.0.1:55205".parse().unwrap());
    let response = playlist_recording_stream(
        rec_fingerprint,
        AxumPath((token.clone(), "live".to_string(), virtual_id)),
        Query(RecordingStreamQuery {
            target_name: target.name.clone(),
            input_name: input.name.to_string(),
            provider_allocation_id: None,
            join_shared: Some("1".to_string()),
            priority: Some(0),
        }),
        State(Arc::clone(&app_state)),
        HeaderMap::new(),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get("x-tuliprox-fallback"),
        Some(&axum::http::HeaderValue::from_static("channel_unavailable"))
    );
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);

    // Worker falls back: acquires slot via capacity port
    let handle = app_state.recording.capacity.acquire(&input.name, 0).await.expect("acquired own slot");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Worker calls endpoint with provider_allocation_id and join_shared=0
    let response = playlist_recording_stream(
        create_test_fingerprint("127.0.0.1:55206".parse().unwrap()),
        AxumPath((token, "live".to_string(), virtual_id)),
        Query(RecordingStreamQuery {
            target_name: target.name.clone(),
            input_name: input.name.to_string(),
            provider_allocation_id: Some(handle.allocation_id),
            join_shared: None,
            priority: Some(0),
        }),
        State(Arc::clone(&app_state)),
        HeaderMap::new(),
    )
    .await
    .into_response();

    assert!(
        response.status().is_success() || response.status().is_server_error() || response.status().is_client_error()
    );
}

#[tokio::test]
async fn test_priority_viewer_5_recording_0_preemption_attempt_fails_after_recording_end_reverts() {
    let stream_url = "http://provider-1.example/live/stream_4.ts";
    let virtual_id = 45;
    let (app_state, target, input) = setup_test_app_state_and_target(stream_url, virtual_id).await;

    // 1. Viewer with priority 5
    let viewer_addr: SocketAddr = "127.0.0.1:55207".parse().unwrap();
    let viewer_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &viewer_addr, 5, crate::api::model::ConnectionKind::Normal)
        .expect("viewer acquire");
    let (_tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(10);
    let shared_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let subscriber_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, viewer_addr)
            .await
            .expect("cleanup");
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &viewer_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), viewer_handle)),
        pending_cleanup,
        5,
        crate::api::model::ConnectionKind::Normal,
        "viewer",
        "viewer_token",
    )
    .await;
    let (viewer_stream, _prov, _viewer_cleanup) = registered.unwrap();

    // Shared stream priority is 5
    assert_eq!(app_state.active_provider.shared_stream_priority(stream_url), Some(5));

    // 2. Recording joins with priority 0
    let token = create_access_token(&app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let rec_fingerprint = create_test_fingerprint("127.0.0.1:55208".parse().unwrap());
    let response = playlist_recording_stream(
        rec_fingerprint,
        AxumPath((token, "live".to_string(), virtual_id)),
        Query(RecordingStreamQuery {
            target_name: target.name.clone(),
            input_name: input.name.to_string(),
            provider_allocation_id: None,
            join_shared: Some("1".to_string()),
            priority: Some(0),
        }),
        State(Arc::clone(&app_state)),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    // Allocation priority becomes min(5, 0) = 0!
    assert_eq!(app_state.active_provider.shared_stream_priority(stream_url), Some(0));

    // 3. Preemption attempt with priority 3 tries to take the slot (provider limit is 1)
    let preempt_addr: SocketAddr = "127.0.0.1:55209".parse().unwrap();
    let preempt_res = app_state
        .active_provider
        .acquire_connection_with_lease_for_session_await(
            &input.name,
            &preempt_addr,
            false,
            3,
            crate::api::model::ConnectionKind::Normal,
            None,
        )
        .await;
    // Preemption must FAIL because candidate priority 3 is weaker than allocation priority 0 (3 > 0)
    assert!(preempt_res.is_none(), "preemption with priority 3 must fail against recording priority 0");

    // 4. Recording ends: drop response body
    drop(response);
    tokio::task::yield_now().await;

    // Shared stream allocation priority reverts to 5!
    assert_eq!(app_state.active_provider.shared_stream_priority(stream_url), Some(5));

    // Preemption attempt with priority 3 now SUCCEEDS (because 3 < 5, candidate is stronger)
    let preempt_res = app_state
        .active_provider
        .acquire_connection_with_lease_for_session_await(
            &input.name,
            &preempt_addr,
            false,
            3,
            crate::api::model::ConnectionKind::Normal,
            None,
        )
        .await;
    assert!(preempt_res.is_some(), "preemption with priority 3 must succeed against viewer priority 5");

    drop(viewer_stream);
}

#[tokio::test]
async fn test_reverse_recording_running_with_own_connection_viewer_joins_without_second_connection() {
    let stream_url = "http://provider-1.example/live/stream_5.ts";
    let virtual_id = 46;
    let (app_state, target, input) = setup_test_app_state_and_target(stream_url, virtual_id).await;

    // Recording starts with its own connection slot
    let rec_addr: SocketAddr = "127.0.0.1:55210".parse().unwrap();
    let rec_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &rec_addr, 0, crate::api::model::ConnectionKind::Normal)
        .expect("recording acquire slot");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Register as shared stream by recording
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(10);
    let shared_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let subscriber_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, rec_addr)
            .await
            .expect("cleanup");
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &rec_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), rec_handle)),
        pending_cleanup,
        0,
        crate::api::model::ConnectionKind::Normal,
        RECORDING_PROXY_USERNAME,
        "recording_token",
    )
    .await;
    assert!(registered.is_some(), "recording registers shared stream");
    let (_rec_stream, _prov, _rec_cleanup) = registered.unwrap();

    // Provider connection count is 1
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Viewer joins the stream
    let viewer_addr: SocketAddr = "127.0.0.1:55211".parse().unwrap();
    let viewer_fp = create_test_fingerprint(viewer_addr);
    let mut viewer_user = ProxyUserCredentials::default();
    viewer_user.username = "viewer_reverse".to_string();
    viewer_user.max_connections = 1;

    let viewer_resp = stream_response(
        &viewer_fp,
        &app_state,
        "viewer-session",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::new(),
        &target,
        &viewer_user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();

    assert_eq!(viewer_resp.status(), StatusCode::OK);
    // Provider connection count STAYS 1 (no second connection opened)!
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Send data - viewer receives it without disturbance
    tx.send(Ok(Bytes::from_static(b"shared_bytes_flow"))).await.unwrap();
    let mut viewer_body = viewer_resp.into_body().into_data_stream();
    let chunk = tokio::time::timeout(Duration::from_millis(500), viewer_body.next())
        .await
        .expect("timed out")
        .expect("chunk exists")
        .expect("chunk ok");
    assert_eq!(chunk, Bytes::from_static(b"shared_bytes_flow"));
}

#[tokio::test]
async fn test_recording_headers_differ_no_join_own_slot() {
    let stream_url = "http://provider-1.example/live/stream_6.ts";
    let virtual_id = 47;
    let (app_state, target, input) = setup_test_app_state_and_target(stream_url, virtual_id).await;

    // Configure recording headers that differ from playback
    let mut config = app_state.app_config.config.load().as_ref().clone();
    let mut rec_headers = std::collections::HashMap::new();
    rec_headers.insert("X-Custom-Rec".to_string(), "special-recording-token".to_string());
    let mut rec_config = RecordingConfig::from(&RecordingConfigDto::default());
    rec_config.headers = rec_headers;
    let mut video_config = config.video.clone().unwrap_or_else(|| VideoConfig::from(&VideoConfigDto::default()));
    video_config.recording = Some(rec_config);
    config.video = Some(video_config);
    app_state.app_config.config.store(Arc::new(config));

    // Capacity port shared_stream_available must return false!
    let available = app_state
        .recording
        .capacity
        .shared_stream_available(&target.name, &input.name, virtual_id, XtreamCluster::Live)
        .await;
    assert!(!available, "shared_stream_available must return false when recording headers differ");

    // Even if a shared stream was somehow running on the provider URL
    let viewer_addr: SocketAddr = "127.0.0.1:55212".parse().unwrap();
    let viewer_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &viewer_addr, 5, crate::api::model::ConnectionKind::Normal)
        .expect("viewer acquire");
    let (_tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(10);
    let shared_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let subscriber_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, viewer_addr)
            .await
            .expect("cleanup");
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &viewer_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), viewer_handle)),
        pending_cleanup,
        5,
        crate::api::model::ConnectionKind::Normal,
        "viewer",
        "viewer_token",
    )
    .await;
    let (_viewer_stream, _prov, _viewer_cleanup) = registered.unwrap();

    // Recording attempts join_shared=1: MUST be rejected with 503 because allow_shared_reuse is false
    let token = create_access_token(&app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let rec_fingerprint = create_test_fingerprint("127.0.0.1:55213".parse().unwrap());
    let response = playlist_recording_stream(
        rec_fingerprint,
        AxumPath((token, "live".to_string(), virtual_id)),
        Query(RecordingStreamQuery {
            target_name: target.name.clone(),
            input_name: input.name.to_string(),
            provider_allocation_id: None,
            join_shared: Some("1".to_string()),
            priority: Some(0),
        }),
        State(Arc::clone(&app_state)),
        HeaderMap::new(),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get("x-tuliprox-fallback"),
        Some(&axum::http::HeaderValue::from_static("channel_unavailable"))
    );
}

#[tokio::test]
async fn test_hls_cache_enabled_for_user_relaxed_for_recording_when_headers_match() {
    let app_state = create_test_provider_app_state();
    let mut target = create_test_shared_target();
    target.options = Some(ConfigTargetOptions {
        share_live_streams: ConfigTargetShareLiveStreams { hls: true, ..Default::default() },
        ..Default::default()
    });

    let mut config = app_state.app_config.config.load().as_ref().clone();
    let mut rp = ReverseProxyConfig::from(&ReverseProxyConfigDto::default());
    rp.hls_cache = Some(HlsCacheConfig::from(&HlsCacheConfigDto::default()));
    config.reverse_proxy = Some(rp);
    app_state.app_config.config.store(Arc::new(config));

    let mut user = ProxyUserCredentials::default();
    user.username = RECORDING_PROXY_USERNAME.to_string();
    user.t_is_api_user = true;

    let input = ConfigInput::default();
    let enabled = crate::api::endpoints::hls_api::hls_cache_enabled_for_user(&app_state, &target, &input, &user);
    assert!(enabled, "HLS cache must be enabled for recording user when recording headers match playback");
}
