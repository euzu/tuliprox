use super::{
    create_catchup_session_key, create_test_app_state_for_config, create_test_fingerprint, create_test_local_channel,
    create_test_provider_app_config, force_provider_stream_response, load_test_user, probe_catchup_payload,
    spawn_range_aware_test_origin, CatchupPayload, ForceStreamRequestContext,
};
use crate::{
    api::model::{StreamError, UserSession},
    auth::Fingerprint,
    model::{ConfigInput, ConfigInputAlias, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::{stream, StreamExt};
use http_body_util::BodyExt;
use shared::{
    model::{InputType, PlaylistItemType, UserConnectionPermission, XtreamCluster},
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tuliprox_hls::api::MAX_HLS_MANIFEST_BYTES;

#[tokio::test]
async fn catchup_payload_probe_detects_fragmented_hls() {
    let source =
        stream::iter([Ok::<_, StreamError>(Bytes::from_static(b"#EX")), Ok(Bytes::from_static(b"TM3U\nsegment.ts\n"))])
            .boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(matches!(&result, Ok(CatchupPayload::HlsManifest(_))));
    if let Ok(CatchupPayload::HlsManifest(manifest)) = result {
        assert_eq!(manifest, b"#EXTM3U\nsegment.ts\n".as_slice());
    }
}

#[tokio::test]
async fn catchup_payload_probe_replays_ts_bytes() {
    let expected = Bytes::from_static(b"\x47direct-ts-payload");
    let source = stream::iter([Ok::<_, StreamError>(expected.clone())]).boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(matches!(&result, Ok(CatchupPayload::Direct(_))));
    if let Ok(CatchupPayload::Direct(mut stream)) = result {
        let mut actual = Vec::new();
        while let Some(chunk) = stream.next().await {
            if let Ok(chunk) = chunk {
                actual.extend_from_slice(&chunk);
            }
        }
        assert_eq!(actual, expected.as_ref());
    }
}

#[tokio::test]
async fn catchup_payload_probe_replays_partial_signature_at_eof() {
    let expected = Bytes::from_static(b"#EXT");
    let source = stream::iter([Ok::<_, StreamError>(expected.clone())]).boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(matches!(&result, Ok(CatchupPayload::Direct(_))));
    if let Ok(CatchupPayload::Direct(mut stream)) = result {
        let actual = stream.next().await.and_then(Result::ok);
        assert_eq!(actual.as_ref(), Some(&expected));
        assert!(stream.next().await.is_none());
    }
}

#[tokio::test]
async fn catchup_payload_probe_rejects_oversized_manifest() {
    let oversized = vec![b'x'; MAX_HLS_MANIFEST_BYTES];
    let source =
        stream::iter([Ok::<_, StreamError>(Bytes::from_static(b"#EXTM3U")), Ok(Bytes::from(oversized))]).boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(result.is_err());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn catchup_abort_seek_and_window_change_preserve_correct_affinity() {
    const CATCHUP_BODY_A: &[u8] = b"catchup-origin-a-archive-payload";
    const CATCHUP_BODY_B: &[u8] = b"catchup-origin-b-archive-payload";

    let (origin_a, task_a) = spawn_range_aware_test_origin(CATCHUP_BODY_A, 2).await;
    let (origin_b, task_b) = spawn_range_aware_test_origin(CATCHUP_BODY_B, 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_a}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: format!("http://{origin_b}"),
            username: None,
            password: None,
            priority: 1,
            max_connections: 2,
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

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_620));
    let fingerprint = create_test_fingerprint(client_addr);
    let user = load_test_user("viewer_catchup");

    let make_catchup_session = |token: &str, window: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 41,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_a}/timeshift/1.ts?window={window}").intern(),
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

    let make_catchup_channel = |window: &str| {
        let mut channel = create_test_local_channel(&format!("http://{origin_a}/timeshift/1.ts?window={window}"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Live;
        channel.url = format!("http://{origin_a}/timeshift/1.ts?window={window}").intern();
        channel
    };

    let session_w1_1 = make_catchup_session("catchup-w1", "w1");
    let session_w1_2 = make_catchup_session("catchup-w1", "w1");

    let mut h1 = HeaderMap::new();
    h1.insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
    let mut h2 = HeaderMap::new();
    h2.insert(header::RANGE, HeaderValue::from_static("bytes=5-14"));

    let resp1 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_w1_1,
        make_catchup_channel("w1"),
        ForceStreamRequestContext {
            req_headers: &h1,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 10,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let resp2 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_w1_2,
        make_catchup_channel("w1"),
        ForceStreamRequestContext {
            req_headers: &h2,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 10,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp1.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp2.status(), StatusCode::PARTIAL_CONTENT);

    let mut session_w2 = make_catchup_session("catchup-w2", "w2");
    session_w2.provider = "provider_2".intern();
    session_w2.stream_url = format!("http://{origin_b}/timeshift/1.ts?window=w2").intern();
    let mut channel_w2 = make_catchup_channel("w2");
    channel_w2.url = Arc::clone(&session_w2.stream_url);
    let mut h_w2 = HeaderMap::new();
    h_w2.insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
    let resp_w2 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_w2,
        channel_w2,
        ForceStreamRequestContext {
            req_headers: &h_w2,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 10,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(resp_w2.status(), StatusCode::PARTIAL_CONTENT);
    let bytes_w2 = resp_w2.into_body().collect().await.expect("window w2 body").to_bytes();
    assert_eq!(bytes_w2.as_ref(), &CATCHUP_BODY_B[..10], "independent window uses available provider B");

    drop(resp1);
    let bytes2 = resp2.into_body().collect().await.expect("resp2 body").to_bytes();
    assert_eq!(bytes2.as_ref(), &CATCHUP_BODY_A[5..15]);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parallel catchup requests must release their physical connections");

    let reqs_a = task_a.await.expect("task A completes");
    assert_eq!(reqs_a.len(), 2, "both requests in window w1 stayed on provider A");
    let reqs_b = task_b.await.expect("task B completes");
    assert_eq!(reqs_b.len(), 1, "the independent window used provider B while A was full");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("catchup provider connections must drain");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    app_state.active_provider.clear_provider_reservation("catchup-w1");
    app_state.active_provider.clear_provider_reservation("catchup-w2");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0);
    assert_eq!(app_state.active_provider.provider_lease_usage(&"provider_2".intern()).total(), 0);
}

#[test]
fn catchup_session_key_is_sticky_only_within_the_same_archive_window() {
    let addr: SocketAddr = "127.0.0.1:55181".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = Fingerprint::new("10.0.0.8|player".to_string(), "10.0.0.8".to_string(), addr);

    let first = create_catchup_session_key(&fingerprint, "user1", 7004, "/timeshift/3600/1700000000/");
    let same = create_catchup_session_key(&fingerprint, "user1", 7004, "timeshift/3600/1700000000");
    let seeked = create_catchup_session_key(&fingerprint, "user1", 7004, "timeshift/3600/1700000300");
    let resized = create_catchup_session_key(&fingerprint, "user1", 7004, "timeshift/1800/1700000000");

    assert_eq!(first, same, "cosmetic path separators must not break provider stickiness");
    assert_ne!(first, seeked, "seeking must identify the new archive window");
    assert_ne!(first, resized, "changing duration must identify the new archive window");
}
