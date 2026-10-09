use super::{
    create_stream_response_details, create_test_app_state_for_config, create_test_dual_provider_app_config,
    create_test_dual_provider_app_state, create_test_fingerprint, create_test_live_channel,
    force_provider_stream_response, get_stream_options, load_test_user, resolve_streaming_strategy,
    spawn_legacy_hls_test_origin, ForceStreamRequestContext, StreamResponseMode, StreamingAcquireOptions,
};
use crate::{
    api::model::{ProviderStreamCustomReason, ProviderStreamState},
    model::{ConfigInput, ConfigInputAlias, SourcesConfig},
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use shared::{
    model::{InputType, PlaylistItemType, UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

#[tokio::test]
async fn resolve_streaming_strategy_honors_forced_provider_fallback_policy() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55301".parse().unwrap_or_else(|_| unreachable!());
    let strict_addr: SocketAddr = "127.0.0.1:55302".parse().unwrap_or_else(|_| unreachable!());
    let fallback_addr: SocketAddr = "127.0.0.1:55303".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let strict = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(strict_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: Some(&pinned_provider),
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("vod-session"),
            playback_kind: crate::model::PlaybackKind::Vod,
            accept_requested_stream_url: true,
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;
    assert!(strict.provider_handle.is_none(), "strict provider affinity should not allocate a different provider");
    assert!(
        matches!(
            strict.provider_stream_state,
            ProviderStreamState::Custom { reason: ProviderStreamCustomReason::ProviderExhausted, .. }
        ),
        "strict provider affinity should fail closed when the pinned provider is unavailable"
    );

    let fallback = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(fallback_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: Some(&pinned_provider),
            allow_forced_provider_fallback: true,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: true,
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;
    let (ProviderStreamState::Available(Some(fallback_provider), fallback_url)
    | ProviderStreamState::GracePeriod(Some(fallback_provider), fallback_url)) = fallback.provider_stream_state
    else {
        panic!("fallback-enabled request should allocate a provider")
    };
    assert_eq!(fallback_provider.as_ref(), "provider_2");
    assert_eq!(fallback_url.as_ref(), "http://provider-2.example/movie/user2/pass2/1.mkv");

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&strict_addr);
    app_state.active_provider.release_connection(&fallback_addr);
}

#[tokio::test]
async fn create_stream_response_details_preserves_stored_headers_when_fallback_open_fails() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
    let reacquire_addr: SocketAddr = "127.0.0.1:55307".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let user = load_test_user("test-fallback-headers-user");
    let session_token = "sess-fallback-headers-1";
    let initial_headers = HashMap::from([("cookie".to_string(), "old_prov_sess=1".to_string())]);
    let created = app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 1,
            provider: &pinned_provider,
            stream_url,
            addr: &reacquire_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(!created.is_empty());
    app_state.active_users.update_session_provider_headers(&user.username, session_token, &initial_headers).await;

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let channel = create_test_live_channel(stream_url);
    let details = create_stream_response_details(
        &app_state,
        &get_stream_options(&app_state.app_config, StreamResponseMode::Stream),
        stream_url,
        &user.username,
        &create_test_fingerprint(reacquire_addr),
        &HeaderMap::new(),
        &input,
        &channel,
        PlaylistItemType::Video,
        crate::api::model::ProviderContentRepresentationMode::Identity,
        false,
        UserConnectionPermission::Allowed,
        Some(&pinned_provider),
        true,
        false,
        VirtualId::new(channel.virtual_id),
        0,
        crate::api::model::ConnectionKind::Normal,
        true,
        Some(session_token),
        Some(&initial_headers),
        true,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_or_else(|err| panic!("create_stream_response_details should succeed: {err}"));

    assert_eq!(details.provider_name.as_deref(), Some("provider_2"));
    assert!(details.session_headers.is_none(), "fallback request should not pass pinned provider session headers");
    assert!(details.stream.is_none(), "test setup should fail to open the fallback provider stream");

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, session_token)
        .await
        .expect("session should exist");
    assert_eq!(
        session.provider_session_headers, initial_headers,
        "stored session headers should be retained when the fallback provider cannot be opened"
    );

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&reacquire_addr);
}

#[tokio::test]
async fn force_provider_stream_response_clears_stored_headers_after_fallback_open_succeeds() {
    const FALLBACK_BODY: &[u8] = b"fallback-provider";
    let response_head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        FALLBACK_BODY.len()
    );
    let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, FALLBACK_BODY.to_vec()).await;
    let app_config = create_test_dual_provider_app_config();
    let Some(configured_input) = app_config.sources.load().inputs.first().cloned() else { unreachable!() };
    let mut fallback_input = (*configured_input).clone();
    let Some(aliases) = fallback_input.aliases.as_mut() else { unreachable!() };
    let Some(fallback_alias) = aliases.first_mut() else { unreachable!() };
    fallback_alias.url = format!("http://{origin_addr}");
    app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(fallback_input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55308".parse().unwrap_or_else(|_| unreachable!());
    let reacquire_addr: SocketAddr = "127.0.0.1:55309".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let user = load_test_user("test-fallback-force-user");
    let session_token = "sess-fallback-force-1";
    let initial_headers = HashMap::from([("cookie".to_string(), "old_pinned_token=abc".to_string())]);
    let created = app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 1,
            provider: &pinned_provider,
            stream_url,
            addr: &reacquire_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(!created.is_empty());
    app_state.active_users.update_session_provider_headers(&user.username, session_token, &initial_headers).await;

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, session_token)
        .await
        .expect("session should exist");

    let channel = create_test_live_channel(stream_url);
    let response = force_provider_stream_response(
        &create_test_fingerprint(reacquire_addr),
        &app_state,
        &session,
        channel,
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
    let body = response.into_body().collect().await.expect("fallback stream body").to_bytes();
    assert_eq!(body.as_ref(), FALLBACK_BODY);
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), origin_task)
        .await
        .expect("fallback request was not sent within 5 seconds")
        .expect("fallback origin task completes");
    assert!(!request.is_empty(), "fallback provider must receive the stream request");

    let updated_session = app_state
        .active_users
        .get_and_update_user_session(&user.username, session_token)
        .await
        .expect("session should exist");
    assert!(
        updated_session.provider_session_headers.is_empty(),
        "stored session headers must be cleared after the fallback provider stream opens"
    );

    app_state.active_provider.release_connection(&busy_addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rewrites_stale_alias_url_to_selected_main_provider() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55304".parse().unwrap_or_else(|_| unreachable!());

    let strategy = resolve_streaming_strategy(
        &app_state,
        "http://provider-2.example/live/user2/pass2/100.ts",
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
        panic!("request should allocate the main provider")
    };
    assert_eq!(provider.as_ref(), "provider_1");
    assert_eq!(url.as_ref(), "http://provider-1.example/live/user1/pass1/100.ts");

    app_state.active_provider.release_connection(&addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rewrites_opaque_m3u_token_after_alias_allocation() {
    let app_config = create_test_dual_provider_app_config();
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider-a".intern(),
        input_type: InputType::M3u,
        url: "http://playlist.example/a.m3u?token=provider-a-token".to_string(),
        enabled: true,
        max_connections: 1,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider-b".intern(),
            url: "http://playlist.example/b.m3u?token=provider-b-token".to_string(),
            username: None,
            password: None,
            max_connections: 0,
            priority: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let provider_a = "provider-a".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
    let alias_addr: SocketAddr = "127.0.0.1:55307".parse().unwrap_or_else(|_| unreachable!());

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &provider_a,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy provider A");

    let strategy = resolve_streaming_strategy(
        &app_state,
        "http://stream.example/channel/segment.ts?token=provider-a-token",
        &create_test_fingerprint(alias_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
        panic!("request should allocate provider B")
    };
    assert_eq!(provider.as_ref(), "provider-b");
    assert_eq!(url.as_ref(), "http://stream.example/channel/segment.ts?token=provider-b-token");

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&alias_addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rejects_unmapped_provider_url() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55305".parse().unwrap_or_else(|_| unreachable!());

    let strategy = resolve_streaming_strategy(
        &app_state,
        "http://unmapped.example/live/user1/pass1/100.ts",
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;

    assert!(strategy.provider_handle.is_none());
    assert!(matches!(
        strategy.provider_stream_state,
        ProviderStreamState::Custom { reason: ProviderStreamCustomReason::UnmappedProviderUrl, .. }
    ));

    app_state.active_provider.release_connection(&addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_accepts_session_requested_stream_url() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
    let trusted_url = "http://unmapped.example/live/user1/pass1/100.ts";
    let strategy = resolve_streaming_strategy(
        &app_state,
        trusted_url,
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: true,
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
        panic!("session-requested URL should be accepted for the pinned provider")
    };
    assert_eq!(provider.as_ref(), "provider_1");
    assert_eq!(url.as_ref(), trusted_url);

    app_state.active_provider.release_connection(&addr);
}
