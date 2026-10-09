use super::{
    create_test_app_state_for_config, create_test_fingerprint, create_test_live_channel, create_test_local_channel,
    create_test_provider_app_config, force_provider_stream_response, get_session_reservation_ttl_secs, load_test_user,
    redirect_response, spawn_legacy_hls_test_origin, spawn_range_aware_test_origin, ForceStreamRequestContext,
    RedirectParams,
};
use crate::{
    api::model::UserSession,
    model::{Config, ConfigInput, ConfigInputAlias, ConfigTarget, ProxyUserCredentials, SourcesConfig},
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use shared::{
    defaults::default_hls_session_ttl_secs,
    foundation::Filter,
    model::{
        InputFetchMethod, InputType, PlaylistEntry, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        ProcessingOrder, ProxyType, TargetType, UserConnectionPermission, XtreamCluster,
    },
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use url::Url;

#[tokio::test]
async fn forced_reopen_stays_on_pinned_provider_account() {
    const ACCOUNT_A_BODY: &[u8] = b"account-a-marker";
    const ACCOUNT_B_BODY: &[u8] = b"account-b-marker";

    let head_a = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ACCOUNT_A_BODY.len()
    );
    let head_b = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ACCOUNT_B_BODY.len()
    );
    let (origin_a, task_a) = spawn_legacy_hls_test_origin(head_a, ACCOUNT_A_BODY.to_vec()).await;
    let (origin_b, task_b) = spawn_legacy_hls_test_origin(head_b, ACCOUNT_B_BODY.to_vec()).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_a}"),
        username: Some("user-a".to_string()),
        password: Some("pass-a".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: format!("http://{origin_b}"),
            username: Some("user-b".to_string()),
            password: Some("pass-b".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let app_config = Arc::new(create_test_provider_app_config());
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(app_config);

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_400));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    let session = UserSession {
        token: "stickiness-token".to_string(),
        transition_version: 1,
        virtual_id: 41,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_a}/live/1.ts").intern(),
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
    let mut stream_channel = create_test_live_channel(&format!("http://{origin_a}/live/1.ts"));
    stream_channel.provider_id = 1;
    stream_channel.input_name = Arc::clone(&input.name);
    stream_channel.item_type = PlaylistItemType::Catchup;
    stream_channel.cluster = XtreamCluster::Live;
    stream_channel.url = session.stream_url.clone();

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.expect("stream body").to_bytes();
    assert_eq!(body.as_ref(), ACCOUNT_A_BODY, "forced reopen must stay on the pinned provider account A");

    let request_a = task_a.await.expect("origin A task completes").to_ascii_lowercase();
    assert!(!request_a.is_empty(), "origin A must have received the seek request");

    let request_b = tokio::time::timeout(std::time::Duration::from_millis(250), task_b).await;
    assert!(request_b.is_err(), "origin B must not receive a seek from an A-affine session");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn forced_series_reopen_uses_free_pool_account_and_updates_session_pin() {
    const ACCOUNT_B_BODY: &[u8] = b"account-b-marker";

    let head_a = "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\na".to_string();
    let head_b = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/x-matroska\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ACCOUNT_B_BODY.len()
    );
    let (origin_a, mut task_a) = spawn_legacy_hls_test_origin(head_a, vec![b'a']).await;
    let (origin_b, task_b) = spawn_legacy_hls_test_origin(head_b, ACCOUNT_B_BODY.to_vec()).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_a}"),
        username: Some("user-a".to_string()),
        password: Some("pass-a".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: format!("http://{origin_b}"),
            username: Some("user-b".to_string()),
            password: Some("pass-b".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let app_config = Arc::new(create_test_provider_app_config());
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(app_config);
    let busy_addr: SocketAddr = "127.0.0.1:55410".parse().unwrap_or_else(|_| unreachable!());
    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &input.name,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup must occupy the pinned provider account");

    let client_addr: SocketAddr = "127.0.0.1:55411".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer-pool-failover".to_string();
    let stream_url = format!("http://{origin_a}/series/user-a/pass-a/1.mkv");
    let session = UserSession {
        token: "series-pool-token".to_string(),
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: stream_url.clone().intern(),
        addr: client_addr,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };
    let mut stream_channel = create_test_live_channel(&stream_url);
    stream_channel.virtual_id = session.virtual_id;
    stream_channel.provider_id = 1;
    stream_channel.input_name = Arc::clone(&input.name);
    stream_channel.item_type = PlaylistItemType::Series;
    stream_channel.cluster = XtreamCluster::Series;
    stream_channel.url = session.stream_url.clone();

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.expect("stream body").to_bytes();
    assert_eq!(body.as_ref(), ACCOUNT_B_BODY, "reopen must use the free provider account B");
    task_b.await.expect("origin B task completes");

    let updated = app_state
        .active_users
        .get_and_update_user_session(&user.username, &session.token)
        .await
        .expect("reopened session is persisted");
    assert_eq!(updated.provider.as_ref(), "provider_2");
    assert!(updated.stream_url.contains("/series/user-b/pass-b/1.mkv"));

    assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut task_a).await.is_err());
    task_a.abort();
    app_state.active_provider.release_connection(&busy_addr);
}

#[tokio::test]
async fn forced_reopen_sends_provider_cookies_only_to_the_origin_that_set_them() {
    for (cookie_from_stream_origin, expect_cookie) in [(true, true), (false, false)] {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 4\r\nConnection: close\r\n\r\n";
        let (origin, task) = spawn_legacy_hls_test_origin(head.to_string(), b"live".to_vec()).await;
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            url: format!("http://{origin}"),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            enabled: true,
            max_connections: 1,
            ..ConfigInput::default()
        });
        let app_config = Arc::new(create_test_provider_app_config());
        app_config
            .sources
            .store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
        let app_state = create_test_app_state_for_config(app_config);

        let stream_url = format!("http://{origin}/live/1.ts");
        let cookie_source =
            if cookie_from_stream_origin { stream_url.clone() } else { "http://cdn.example/live/1.ts".to_string() };
        let mut cookies = tuliprox_session::ProviderSessionCookieStore::default();
        cookies.update(
            &Url::parse(&cookie_source).expect("cookie source url"),
            &tuliprox_session::ProviderSessionHeaders {
                headers: HashMap::new(),
                cookies: vec!["sid=scoped; Path=/".to_string()],
            },
        );
        let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_410));
        let mut user = ProxyUserCredentials::default();
        user.username = "viewer".to_string();
        let session = UserSession {
            token: "cookie-scope-token".to_string(),
            virtual_id: 41,
            provider: Arc::clone(&input.name),
            stream_url: stream_url.as_str().intern(),
            provider_session_cookies: Arc::new(cookies),
            media_started: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            addr: client_addr,
            active_addrs: vec![client_addr],
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            lifecycle: crate::api::model::PlaybackLifecycle::Active,
            ..Default::default()
        };
        let mut stream_channel = create_test_live_channel(&stream_url);
        stream_channel.provider_id = 1;
        stream_channel.input_name = Arc::clone(&input.name);

        let response = force_provider_stream_response(
            &create_test_fingerprint(client_addr),
            &app_state,
            &session,
            stream_channel,
            ForceStreamRequestContext {
                req_headers: &HeaderMap::new(),
                input: &input,
                user: &user,
                session_reservation_ttl_secs: 0,
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            None,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        drop(response.into_body().collect().await.expect("stream body"));

        let request = task.await.expect("origin task completes").to_ascii_lowercase();
        assert_eq!(
            request.contains("cookie: sid=scoped"),
            expect_cookie,
            "cookie_from_stream_origin={cookie_from_stream_origin}: {request}"
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn parallel_series_range_requests_keep_both_claims_active() {
    const SERIES_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(SERIES_BODY, 2).await;

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

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_501));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();

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

    let first_headers = {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
        headers
    };
    let second_headers = {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=5-14"));
        headers
    };

    // Open both range requests before consuming either body, so both claims are
    // active at the same time. This exercises concurrent claims of one playback
    // rather than a sequential request/consume cycle.
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &make_session("series-range-1"),
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &make_session("series-range-2"),
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(app_state.active_users.active_streams().await.len(), 2, "both range claims must be active concurrently");

    let first_body = first.into_body().collect().await.expect("first series range body").to_bytes();
    assert_eq!(first_body.as_ref(), &SERIES_BODY[0..10]);

    let second_body = second.into_body().collect().await.expect("second series range body").to_bytes();
    assert_eq!(second_body.as_ref(), &SERIES_BODY[5..15]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 2, "both series range requests must reach the same provider account");
    assert!(requests[0].contains("range: bytes=0-9"));
    assert!(requests[1].contains("range: bytes=5-14"));
}

#[tokio::test]
async fn dash_stream_request_remains_redirect_and_preserves_provider_affinity() {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_dash".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-dash.example".to_string(),
        username: Some("dash-user".to_string()),
        password: Some("dash-pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let target = Arc::new(ConfigTarget {
        curation: None,
        id: 1,
        name: "target_dash".to_string(),
        enabled: true,
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    });

    let mut app_cfg = create_test_provider_app_config();
    app_cfg.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: None,
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));

    let pli = PlaylistItem {
        header: PlaylistItemHeader {
            id: "1".intern(),
            name: "DASH Live".intern(),
            group: "Live".intern(),
            title: "DASH Live".intern(),
            url: "http://provider-dash.example/live/dash-user/dash-pass/1.mpd".intern(),
            item_type: PlaylistItemType::LiveDash,
            xtream_cluster: XtreamCluster::Live,
            virtual_id: shared::model::VirtualId(1),
            ..PlaylistItemHeader::default()
        },
    };

    let mut user = load_test_user("dash_viewer");
    user.proxy = ProxyType::Reverse(None);
    for target_type in [TargetType::M3u, TargetType::Xtream] {
        let redirect_params = RedirectParams {
            item: &pli,
            provider_id: pli.get_provider_id(),
            cluster: XtreamCluster::Live,
            target_type,
            target: &target,
            input: &input,
            user: &user,
            stream_ext: Some(shared::defaults::DASH_EXT),
            req_context: crate::api::endpoints::xtream_api::ApiStreamContext::Live,
            action_path: "",
        };

        let resp = redirect_response(&app_state, &redirect_params)
            .await
            .expect("DASH must return a redirect response")
            .into_response();
        assert_eq!(resp.status(), StatusCode::FOUND, "DASH request must be HTTP 302 Found redirect");
        let location = resp.headers().get(axum::http::header::LOCATION).expect("location header present");
        let location_str = location.to_str().expect("valid location string");
        assert!(
            location_str.contains("provider-dash.example"),
            "redirect location must point to provider URL, got {location_str}"
        );
    }

    let dash_ttl = get_session_reservation_ttl_secs(&app_state, PlaylistItemType::LiveDash);
    assert_eq!(dash_ttl, default_hls_session_ttl_secs(), "DASH preserves provider affinity TTL");
}
