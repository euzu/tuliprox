use super::{
    activate_session_before_stream_open, cleanup_forced_reopen_addrs, create_test_app_state,
    create_test_app_state_for_config, create_test_app_state_with_stream_config, create_test_fingerprint,
    create_test_live_channel, create_test_local_channel, create_test_provider_app_config,
    create_test_provider_app_state, create_test_shared_target, force_provider_stream_response, get_stream_options,
    load_test_channel, load_test_session, load_test_user, session_reacquire_cleanup_addrs,
    spawn_controlled_fake_origin, spawn_range_aware_test_origin, stream_response, FakeOriginMode,
    ForceStreamRequestContext, PlaybackRequestClass, SessionActivationRequest, StreamResponseMode,
};
use crate::{
    api::model::{create_channel_unavailable_stream, UserSession},
    auth::Fingerprint,
    model::{Config, ConfigInput, ConfigInputAlias, ProxyUserCredentials, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use shared::{
    model::{AdmissionStrategy, InputType, PlaylistItemType, UserConnectionPermission, VirtualId, XtreamCluster},
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn direct_ts_eof_before_first_byte_releases_slot_without_idle_lease() {
    let (origin_addr, origin_task) = spawn_controlled_fake_origin(FakeOriginMode::EmptyBody, 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_601));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_ts_eof".to_string();

    let session = UserSession {
        token: "session-eof".to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/live/42.ts").intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };

    let channel = load_test_channel(&input, origin_addr);

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = response.into_body().collect().await.expect("empty body collects").to_bytes();
    assert!(body_bytes.is_empty(), "expected 0 payload bytes on immediate EOF");

    let requests = origin_task.await.expect("origin finishes");
    assert_eq!(requests.len(), 1);

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0
            && app_state.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "physical slot must be released");
    let lease_usage = app_state.active_provider.provider_lease_usage(&input.name);
    assert_eq!(lease_usage.starting, 0, "no starting leases remaining");
    assert_eq!(lease_usage.active, 0, "no active leases remaining");
    assert_eq!(lease_usage.idle, 0, "unconfirmed stream must NOT transition to an idle lease");
    assert_eq!(lease_usage.total(), 0, "total leases must be 0");
    assert_eq!(app_state.active_users.active_streams().await.len(), 0, "active user stream must be cleaned up");
}

#[tokio::test]
async fn direct_ts_abort_while_waiting_for_first_byte_releases_exact_request() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let (origin_addr, origin_task) =
        spawn_controlled_fake_origin(FakeOriginMode::BlockFirstChunk(Arc::clone(&notify)), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_602));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_ts_abort".to_string();

    let session = UserSession {
        token: "session-abort-before-first-byte".to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/live/42.ts").intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };

    let channel = load_test_channel(&input, origin_addr);

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    drop(response);
    notify.notify_waiters();

    let _ = origin_task.await;

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0
            && app_state.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        0,
        "physical slot must be released on abort"
    );
    let lease_usage = app_state.active_provider.provider_lease_usage(&input.name);
    assert_eq!(lease_usage.total(), 0, "no residual lease claim after unconfirmed abort");
    assert_eq!(app_state.active_users.active_streams().await.len(), 0, "active user stream must be cleaned up");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn provider_error_after_first_byte_finishes_only_matching_request() {
    const TS_CHUNK_188: &[u8] = &[0x47; 188];
    const SIBLING_CHUNK: &[u8] = &[0x47; 376];

    let (origin_fail_addr, origin_fail_task) =
        spawn_controlled_fake_origin(FakeOriginMode::ErrorAfterFirstChunk(TS_CHUNK_188.to_vec()), 1).await;
    let (origin_ok_addr, origin_ok_task) =
        spawn_controlled_fake_origin(FakeOriginMode::FixedBytes(SIBLING_CHUNK.to_vec()), 1).await;

    let input_fail = Arc::new(ConfigInput {
        id: 1,
        name: "provider_fail".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_fail_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let input_ok = Arc::new(ConfigInput {
        id: 2,
        name: "provider_ok".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_ok_addr}"),
        enabled: true,
        priority: 1,
        max_connections: 2,
        ..ConfigInput::default()
    });

    let mut config = create_test_provider_app_config();
    config.sources = Arc::new(ArcSwap::from_pointee(SourcesConfig {
        inputs: vec![Arc::clone(&input_fail), Arc::clone(&input_ok)],
        ..SourcesConfig::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let addr_fail = SocketAddr::from(([127, 0, 0, 1], 55_603));
    let addr_ok = SocketAddr::from(([127, 0, 0, 1], 55_604));

    let user = load_test_user("viewer_multistream");
    let session_fail = load_test_session(&input_fail, origin_fail_addr, "session-fail", addr_fail);
    let session_ok = load_test_session(&input_ok, origin_ok_addr, "session-ok", addr_ok);

    let channel_fail = load_test_channel(&input_fail, origin_fail_addr);
    let channel_ok = load_test_channel(&input_ok, origin_ok_addr);

    let resp_fail = force_provider_stream_response(
        &create_test_fingerprint(addr_fail),
        &app_state,
        &session_fail,
        channel_fail,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input_fail,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let resp_ok = force_provider_stream_response(
        &create_test_fingerprint(addr_ok),
        &app_state,
        &session_ok,
        channel_ok,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input_ok,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp_fail.status(), StatusCode::OK);
    assert_eq!(resp_ok.status(), StatusCode::OK);
    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        2,
        "both providers have an active connection"
    );

    let mut fail_body = resp_fail.into_body();
    let first_frame = fail_body.frame().await.expect("first frame exists").expect("first frame is valid");
    let first_chunk = first_frame.into_data().expect("data chunk exists");
    assert_eq!(first_chunk.as_ref(), TS_CHUNK_188, "first byte was confirmed with exact chunk bytes");

    let second_frame_res = fail_body.frame().await;
    assert!(
        second_frame_res.is_none() || second_frame_res.as_ref().is_some_and(Result::is_err),
        "stream must terminate or error after first chunk"
    );
    drop(fail_body);

    for _ in 0..50 {
        if app_state.active_provider.provider_lease_usage(&input_fail.name).total() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        app_state.active_provider.provider_lease_usage(&input_fail.name).total(),
        0,
        "failed provider leases cleaned up"
    );
    assert_eq!(
        app_state.active_provider.provider_lease_usage(&input_ok.name).total(),
        1,
        "sibling provider lease remains intact"
    );
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1, "sibling connection still active");

    let ok_bytes = resp_ok.into_body().collect().await.expect("sibling body collects").to_bytes();
    assert_eq!(ok_bytes.as_ref(), SIBLING_CHUNK);

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "all slots returned to baseline");
    let _ = origin_fail_task.await;
    let _ = origin_ok_task.await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn no_first_byte_failure_allows_next_request_to_prefer_provider_a() {
    const TS_CHUNK: &[u8] = &[0x47; 188];
    let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("origin A binds");
    let primary_origin_addr = listener_a.local_addr().expect("origin A address");
    let task_a = tokio::spawn(async move {
        let mut requests = Vec::new();
        for round in 0..2 {
            let Ok((mut socket, _)) = listener_a.accept().await else { break };
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut chunk = [0u8; 512];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                req.extend_from_slice(&chunk[..read]);
            }
            requests.push(String::from_utf8_lossy(&req).to_ascii_lowercase());
            if round == 0 {
                let head =
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.shutdown().await;
            } else {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    TS_CHUNK.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(TS_CHUNK).await;
                let _ = socket.shutdown().await;
            }
        }
        requests
    });

    let (secondary_origin_addr, _task_b) =
        spawn_controlled_fake_origin(FakeOriginMode::FixedBytes(TS_CHUNK.to_vec()), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_A".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{primary_origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_B".intern(),
            url: format!("http://{secondary_origin_addr}"),
            username: None,
            password: None,
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });

    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_605));
    let fingerprint = create_test_fingerprint(client_addr);
    let user = load_test_user("viewer_prefer_a");

    let session_1 = load_test_session(&input, primary_origin_addr, "session-1", client_addr);
    let channel_1 = load_test_channel(&input, primary_origin_addr);

    let resp_1 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_1,
        channel_1,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp_1.status(), StatusCode::OK);
    let b1 = resp_1.into_body().collect().await.expect("body 1 collects").to_bytes();
    assert!(b1.is_empty(), "request 1 had early EOF");

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0
            && app_state.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "slot on A must be free");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0, "no phantom lease on A");

    let session_2 = load_test_session(&input, primary_origin_addr, "session-2", client_addr);
    let channel_2 = load_test_channel(&input, primary_origin_addr);

    let resp_2 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_2,
        channel_2,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp_2.status(), StatusCode::OK);
    let b2 = resp_2.into_body().collect().await.expect("body 2 collects").to_bytes();
    assert_eq!(b2.as_ref(), TS_CHUNK, "request 2 successfully used Provider A and read payload");

    let reqs_a = task_a.await.expect("task A completed");
    assert_eq!(reqs_a.len(), 2, "both requests 1 and 2 were routed to Provider A!");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn parallel_vod_abort_preserves_sibling_claim_and_provider_stickiness() {
    const VOD_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(VOD_BODY, 3).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_701));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_vod_parallel".to_string();

    let make_session = |token: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/movie/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };

    let make_channel = || {
        let mut channel = create_test_local_channel(&format!("http://{origin_addr}/movie/1.ts"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Video;
        channel.cluster = XtreamCluster::Video;
        channel.url = format!("http://{origin_addr}/movie/1.ts").intern();
        channel
    };

    let build_request = |range: &'static str, token: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static(range));
        let session = make_session(token);
        (headers, session)
    };

    let (first_headers, first_session) = build_request("bytes=0-9", "vod-playback");
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &first_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);

    let (second_headers, second_session) = build_request("bytes=5-14", "vod-playback");
    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &second_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "affine claims share one playback row");
    assert_eq!(app_state.active_users.playback_resource_counts().await.0, 2, "both request claims active");

    drop(first);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_users.playback_resource_counts().await.0 != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted VOD body must release only its request claim");
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "sibling playback row remains active");

    let second_body = second.into_body().collect().await.expect("second body collects").to_bytes();
    assert_eq!(second_body.as_ref(), &VOD_BODY[5..15]);

    let (third_headers, third_session) = build_request("bytes=10-19", "vod-playback");
    let third = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &third_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &third_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(third.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        third.headers().get(header::CONTENT_RANGE).and_then(|value| value.to_str().ok()),
        Some("bytes 10-19/20")
    );
    let third_body = third.into_body().collect().await.expect("third body collects").to_bytes();
    assert_eq!(third_body.as_ref(), &VOD_BODY[10..20]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 3, "all 3 range requests must reach the same provider account");
    assert!(requests[0].contains("range: bytes=0-9"));
    assert!(requests[1].contains("range: bytes=5-14"));
    assert!(requests[2].contains("range: bytes=10-19"));

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("VOD provider connections must drain");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    app_state.active_provider.clear_provider_reservation("vod-playback");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn parallel_series_abort_then_seek_reuses_account_without_stale_claim() {
    const SERIES_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(SERIES_BODY, 3).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_702));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_series_parallel".to_string();

    let make_session = |token: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 43,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/series/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };

    let make_channel = || {
        let mut channel = create_test_local_channel(&format!("http://{origin_addr}/series/1.ts"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Series;
        channel.cluster = XtreamCluster::Series;
        channel.url = format!("http://{origin_addr}/series/1.ts").intern();
        channel
    };

    let build_request = |range: &'static str, token: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static(range));
        let session = make_session(token);
        (headers, session)
    };

    let (first_headers, first_session) = build_request("bytes=0-9", "series-playback");
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &first_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);

    let (second_headers, second_session) = build_request("bytes=5-14", "series-playback");
    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &second_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "affine claims share one playback row");
    assert_eq!(app_state.active_users.playback_resource_counts().await.0, 2, "both request claims active");

    drop(first);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_users.playback_resource_counts().await.0 != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted Series body must release only its request claim");
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "sibling playback row remains active");

    let second_body = second.into_body().collect().await.expect("second body collects").to_bytes();
    assert_eq!(second_body.as_ref(), &SERIES_BODY[5..15]);

    let (third_headers, third_session) = build_request("bytes=12-19", "series-playback");
    let third = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &third_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &third_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(third.status(), StatusCode::PARTIAL_CONTENT);
    let third_body = third.into_body().collect().await.expect("third body collects").to_bytes();
    assert_eq!(third_body.as_ref(), &SERIES_BODY[12..20]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 3, "all series requests reach same provider");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("series provider connections must drain");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    app_state.active_provider.clear_provider_reservation("series-playback");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0);
}

#[tokio::test]
async fn stale_direct_cleanup_after_rebind_preserves_successor_request() {
    let app_state = create_test_provider_app_state();
    let provider_name: Arc<str> = Arc::from("provider_1");
    let owner = "test-rebind-owner";
    let addr_x = SocketAddr::from(([127, 0, 0, 1], 55_631));
    let addr_y = SocketAddr::from(([127, 0, 0, 1], 55_632));

    let req_x = tuliprox_core::model::PlaybackRequestId::from_raw(101);
    let lease_ref_x =
        tuliprox_session::PlaybackLeaseRef { owner, kind: tuliprox_core::model::PlaybackKind::Vod, request_id: req_x };
    let handle_x = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_x,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_x),
        )
        .expect("request X acquires provider");
    let stale_tag = handle_x.binding_tag.expect("request X binding tag");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_x, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_x);
    app_state.active_provider.release_handle(&handle_x);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );
    assert_eq!(app_state.active_provider.provider_lease_usage(&provider_name).idle, 1);

    let req_y = tuliprox_core::model::PlaybackRequestId::from_raw(102);
    let lease_ref_y =
        tuliprox_session::PlaybackLeaseRef { owner, kind: tuliprox_core::model::PlaybackKind::Vod, request_id: req_y };
    let handle_y = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_y,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_y),
        )
        .expect("request Y reacquires provider");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_y, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_y);

    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );

    let usage_after_stale_finish = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(usage_after_stale_finish.active, 1, "successor request Y must keep the active lease alive");

    app_state.active_provider.clear_identified_provider_reservation(owner, &provider_name, Some(stale_tag));

    let usage_after_stale_tag = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(usage_after_stale_tag.active, 1, "stale clear tag must not erase active successor lease");

    app_state.active_provider.release_handle(&handle_y);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_y,
        tuliprox_core::model::PlaybackRequestOutcome::ServerShutdown,
    );

    let final_usage = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(final_usage.total(), 0, "request Y has finished");
}

#[tokio::test]
async fn stale_hls_cleanup_after_rebind_preserves_current_origin_and_segments() {
    let app_state = create_test_provider_app_state();
    let provider_name: Arc<str> = Arc::from("provider_1");
    let owner = "test-hls-rebind-owner";
    let addr_x = SocketAddr::from(([127, 0, 0, 1], 55_633));
    let addr_y = SocketAddr::from(([127, 0, 0, 1], 55_634));

    let req_x = tuliprox_core::model::PlaybackRequestId::from_raw(201);
    let lease_ref_x = tuliprox_session::PlaybackLeaseRef {
        owner,
        kind: tuliprox_core::model::PlaybackKind::LiveHls,
        request_id: req_x,
    };
    let handle_x = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_x,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_x),
        )
        .expect("request X acquires HLS provider");
    let stale_tag = handle_x.binding_tag.expect("request X binding tag");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_x, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_x);
    app_state.active_provider.release_handle(&handle_x);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );

    let req_y = tuliprox_core::model::PlaybackRequestId::from_raw(202);
    let lease_ref_y = tuliprox_session::PlaybackLeaseRef {
        owner,
        kind: tuliprox_core::model::PlaybackKind::LiveHls,
        request_id: req_y,
    };
    let handle_y = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_y,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_y),
        )
        .expect("request Y reacquires HLS provider");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_y, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_y);

    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );

    let usage = app_state.active_provider.provider_lease_usage(&provider_name);
    assert!(usage.active >= 1 || usage.idle >= 1, "active/reconnect lease remains for successor");

    app_state.active_provider.clear_identified_provider_reservation(owner, &provider_name, Some(stale_tag));

    let usage2 = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(usage2.total(), usage.total(), "stale clear tag must not drop the HLS lease");

    app_state.active_provider.release_handle(&handle_y);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_y,
        tuliprox_core::model::PlaybackRequestOutcome::ServerShutdown,
    );
    assert_eq!(app_state.active_provider.provider_lease_usage(&provider_name).total(), 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn shared_proxy_clients_keep_independent_bodies_and_cleanup() {
    let mut target = create_test_shared_target();
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    let target = Arc::new(target);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("shared origin binds");
    let origin_addr = listener.local_addr().expect("shared origin addr");
    let app = axum::Router::new().fallback(|| async {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        tokio::spawn(async move {
            for i in 0..10u8 {
                let _ = tx.send(Ok(Bytes::from(vec![i; 188]))).await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "video/mp2t")
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap()
    });
    let origin_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_shared_p".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 5,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let proxy_addr = SocketAddr::from(([127, 0, 0, 1], 55_190));
    let fp1 = Fingerprint::new("client1|ua1".to_string(), "127.0.0.1".to_string(), proxy_addr);
    let fp2 = Fingerprint::new("client2|ua2".to_string(), "127.0.0.1".to_string(), proxy_addr);

    let user1 = load_test_user("user_shared_1");
    let user2 = load_test_user("user_shared_2");
    let stream_url = format!("http://{origin_addr}/live/user/pass/shared.ts");
    let mut channel1 = load_test_channel(&input, origin_addr);
    channel1.url = stream_url.as_str().intern();
    let mut channel2 = load_test_channel(&input, origin_addr);
    channel2.url = stream_url.as_str().intern();

    let resp1 = stream_response(
        &fp1,
        &app_state,
        "shared-session-1",
        None,
        channel1,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user1,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();
    assert_eq!(resp1.status(), StatusCode::OK);

    let resp2 = stream_response(
        &fp2,
        &app_state,
        "shared-session-2",
        None,
        channel2,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user2,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();
    assert_eq!(resp2.status(), StatusCode::OK);

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1, "only 1 shared origin connection");

    drop(resp1);

    let body2 = resp2.into_body().collect().await.expect("client 2 body collects").to_bytes();
    assert!(!body2.is_empty(), "client 2 receives stream bytes after client 1 dropped");
    drop(body2);

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    origin_task.abort();
}

#[tokio::test]
async fn shared_start_abort_does_not_leak_meter_registration() {
    let mut target = create_test_shared_target();
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    let target = Arc::new(target);

    let notify = Arc::new(tokio::sync::Notify::new());
    let (origin_addr, origin_task) =
        spawn_controlled_fake_origin(FakeOriginMode::BlockFirstChunk(Arc::clone(&notify)), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_shared_abort".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    config.config = Arc::new(ArcSwap::from_pointee(Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(crate::model::StreamConfig { metrics_enabled: true, ..crate::model::StreamConfig::default() }),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_196));
    let fp = create_test_fingerprint(client_addr);
    let user = load_test_user("user_shared_abort");
    let stream_url = format!("http://{origin_addr}/live/user/pass/shared_abort.ts");
    let mut channel = load_test_channel(&input, origin_addr);
    channel.url = stream_url.as_str().intern();

    let resp = stream_response(
        &fp,
        &app_state,
        "shared-session-abort",
        None,
        channel,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    drop(resp);
    notify.notify_waiters();
    let _ = origin_task.await;

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0
            || app_state.active_provider.provider_lease_usage(&input.name).total() != 0
            || !app_state.active_users.active_streams().await.is_empty()
            || app_state.shared_stream_manager.meter_count() != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shared abort resources must return to baseline");

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "provider slot released");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0, "provider request released");
    assert_eq!(app_state.active_users.active_streams().await.len(), 0, "user claim released");
    assert_eq!(app_state.shared_stream_manager.meter_count(), 0, "aborted origin meter registration released");
}

// stale FollowUp revalidation
#[tokio::test]
async fn activate_session_before_stream_open_stale_follow_up_reclassified_on_counted_lease_release() {
    // Scenario: pre-computed FollowUp, but session's counted lease was released before
    // the guard was acquired. Must reclassify to Activate so admission runs.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        provider_affinity_ttl_secs: 120,
        hls_wrap_media_playlist: true,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "stale-followup-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55230.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55230;

    // Session created in Active (counted) state.
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-stale-followup",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    // Simulate the counted lease being released before activation:
    // expire the session so it no longer has a counted lease.
    app_state.active_users.terminate_session(&user.username, "tok-stale-followup").await;

    // Call activate with stale FollowUp. Must NOT skip admission — reclassification
    // to Activate must run so the placeholder is created.
    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-stale-followup",
            request_class: Some(PlaybackRequestClass::FollowUp),
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: None,
            socket_bound: true,
        },
    )
    .await;

    // Must NOT skip — placeholder must be created since session is expired.
    assert!(
        activation.placeholder_transition_version.is_some(),
        "stale FollowUp with expired session must run admission and create placeholder"
    );
}

#[test]
fn session_reacquire_cleanup_addrs_excludes_current_and_deduplicates() {
    let primary: SocketAddr = "127.0.0.1:55191".parse().unwrap_or_else(|_| unreachable!());
    let overlap: SocketAddr = "127.0.0.1:55192".parse().unwrap_or_else(|_| unreachable!());
    let seek: SocketAddr = "127.0.0.1:55193".parse().unwrap_or_else(|_| unreachable!());
    let session = UserSession {
        token: "tok-vod".to_string(),
        transition_version: 1,
        virtual_id: 9001,
        provider: "provider-a".intern(),
        stream_url: "http://localhost/movie.mkv".intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr: seek,
        socket_bound: false,
        active_addrs: vec![primary, overlap, seek, overlap],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };

    assert_eq!(session_reacquire_cleanup_addrs(&session, &seek), vec![primary, overlap]);
}

#[tokio::test]
async fn forced_reopen_cleanup_for_adaptive_streams_does_not_close_client_socket() {
    let app_state = create_test_app_state();
    let addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
    let mut close_rx = app_state.connection_manager.get_close_connection_channel();

    cleanup_forced_reopen_addrs(&app_state, "adaptive-owner", &[addr]).await;

    let signal =
        tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv()).await.ok().and_then(Result::ok);
    assert!(signal.is_none(), "adaptive cleanup should not hard-close the previous client socket");
}

#[tokio::test]
async fn forced_reopen_cleanup_for_non_adaptive_streams_preserves_client_socket() {
    let app_state = create_test_app_state();
    let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
    let mut close_rx = app_state.connection_manager.get_close_connection_channel();

    cleanup_forced_reopen_addrs(&app_state, "live-owner", &[addr]).await;

    let signal =
        tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv()).await.ok().and_then(Result::ok);
    assert!(signal.is_none(), "a reopen must not close unrelated requests on the same proxy socket");
}

#[tokio::test]
async fn test_channel_unavailable_fallback_does_not_register_body_owner_or_leak_slot() {
    let app_state = create_test_provider_app_state();
    let provider_name = "provider_1".intern();
    let mut cfg = (**app_state.app_config.config.load()).clone();
    cfg.custom_stream_response_enabled = true;
    app_state.app_config.config.store(std::sync::Arc::new(cfg));
    let addr: std::net::SocketAddr = "127.0.0.1:49080".parse().unwrap();

    let custom_video = crate::model::CustomStreamResponse {
        channel_unavailable: Some(crate::api::model::TransportStreamBuffer::new(b"channel-unavailable-bytes".to_vec())),
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    };
    app_state.app_config.custom_stream_response.store(Some(std::sync::Arc::new(custom_video)));

    let handle = app_state
        .active_provider
        .acquire_connection(&provider_name, &addr, 0, tuliprox_session::ConnectionKind::Normal)
        .expect("connection should be acquired");

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let (channel_unavail_stream, info) =
        create_channel_unavailable_stream(&app_state.app_config, &[], axum::http::StatusCode::OK);
    assert!(channel_unavail_stream.is_some());

    let factory_response = tuliprox_session::ProviderStreamFactoryResponse {
        stream: channel_unavail_stream.unwrap(),
        info,
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        has_upstream_owner: false,
    };

    if factory_response.has_upstream_owner {
        app_state.active_provider.register_body_owner(handle.allocation_id);
    }

    app_state.active_provider.release_handle(&handle);

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    assert_eq!(app_state.active_provider.active_connections().map_or(0, |m| m.values().sum::<usize>()), 0);
}

#[tokio::test]
async fn test_provider_open_cancelled_during_header_wait_releases_slot_without_leak() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let accepted = std::sync::Arc::new(tokio::sync::Notify::new());
    let accepted_clone = std::sync::Arc::clone(&accepted);

    let server_task = tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            accepted_clone.notify_waiters();
            let mut buf = [0u8; 1024];
            loop {
                use tokio::io::AsyncReadExt;
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    });

    let app_state = create_test_provider_app_state();
    let provider_name = "provider_1".intern();
    let client_addr: std::net::SocketAddr = "127.0.0.1:49081".parse().unwrap();

    let handle = app_state
        .active_provider
        .acquire_connection(&provider_name, &client_addr, 0, tuliprox_session::ConnectionKind::Normal)
        .expect("connection should be acquired");

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let url = url::Url::parse(&format!("http://{local_addr}/stream.ts")).unwrap();
    let req_headers = axum::http::HeaderMap::new();
    let stream_options = get_stream_options(&app_state.app_config, StreamResponseMode::Stream);
    let mut options =
        crate::api::model::ProviderStreamFactoryOptions::new(&crate::api::model::ProviderStreamFactoryParams {
            addr: client_addr,
            item_type: PlaylistItemType::Live,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers: None,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: tuliprox_session::ProviderContentRepresentationMode::PreserveOrigin,
        });
    options.set_provider_handle_tokens(
        handle.cancel_token.clone(),
        handle.completion_token.clone(),
        Some(handle.close_reason.clone()),
    );
    app_state.active_provider.mark_opening(handle.allocation_id);

    let ctx = app_state.provider_stream_ctx();
    let client = app_state.http_clients.default.load().as_ref().clone();

    let open_task =
        tokio::spawn(async move { crate::api::model::create_provider_stream(&ctx, &client, options).await });

    // Wait until TCP connection is accepted and waiting for response headers
    accepted.notified().await;

    // Now cancel the handle while it is waiting for response headers
    handle.cancel_token.as_ref().unwrap().cancel();

    // The open task should finish quickly because it is cancelled
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), open_task)
        .await
        .expect("open task should abort upon cancellation")
        .expect("task join");
    assert!(outcome.is_none());

    // Release the handle
    app_state.active_provider.release_handle(&handle);

    // Yield to allow background completion tasks to process
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    assert_eq!(app_state.active_provider.active_connections().map_or(0, |m| m.values().sum::<usize>()), 0);

    let _ = server_task.await;
}
