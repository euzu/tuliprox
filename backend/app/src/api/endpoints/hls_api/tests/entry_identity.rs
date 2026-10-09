use super::{
    access_lease_id_from_variant_uri, access_lease_session_token, cache_test_m3u_hls_item,
    configure_default_test_server, enable_channel_unavailable_custom_response, enable_hls_cache, get_response,
    hls_custom_video_test_user, proxy_session_id_from_variant_uri, response_body, single_hls_provider_input,
    single_variant_master_playlist, single_variant_uri, store_test_sources_with_target, test_addr_with_port,
    test_app_state, test_app_state_with_inputs, test_fingerprint, test_fingerprint_with_addr,
    test_hls_entry_stream_context, test_hls_input, test_hls_share_target, test_m3u_hls_item, test_m3u_hls_share_target,
};
use crate::{
    api::model::{
        build_proxy_session_id, ConnectionKind, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseTouch,
        HlsOriginSourceKind, HlsPlaybackFamilyKey, HlsSessionKey, ProxySessionId,
    },
    model::{Config, ConfigInput, ConfigTarget, ProxyUserCredentials, ReverseProxyConfig},
    processing::parser::hls::{rewrite_hls, RewriteHlsProps},
};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::{
    model::{
        HlsCacheConfigDto, InputType, PlaylistItem, PlaylistItemHeader, PlaylistItemType, ReverseProxyConfigDto,
        UserConnectionPermission, XtreamCluster,
    },
    utils::Internable,
};
use std::sync::Arc;

#[test]
fn virtual_hls_entry_path_uses_single_manifest_extension() {
    let user = hls_custom_video_test_user();
    let xtream_target = ConfigTarget::from(&shared::model::ConfigTargetDto {
        output: vec![shared::model::TargetOutputDto::Xtream(shared::model::XtreamTargetOutputDto::default())],
        ..Default::default()
    });
    let xtream_input = ConfigInput { input_type: InputType::Xtream, ..ConfigInput::default() };
    let m3u_target = ConfigTarget::from(&shared::model::ConfigTargetDto::default());
    let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };

    let xtream_path = super::super::build_virtual_hls_entry_path(&xtream_target, &xtream_input, &user, 59);
    let m3u_path = super::super::build_virtual_hls_entry_path(&m3u_target, &m3u_input, &user, 59);

    assert_eq!(xtream_path, "/live/viewer/secret/59.m3u8");
    assert_eq!(m3u_path, "/m3u-stream/live/viewer/secret/59.m3u8");
    assert!(!xtream_path.contains("..m3u8"));
    assert!(!m3u_path.contains("..m3u8"));
}

#[test]
fn hls_origin_source_kind_covers_xtream_m3u_and_direct_media_playlist() {
    assert_eq!(super::super::hls_origin_source_kind(InputType::Xtream), HlsOriginSourceKind::XtreamLive);
    assert_eq!(super::super::hls_origin_source_kind(InputType::M3u), HlsOriginSourceKind::M3uMediaPlaylist);
    assert_eq!(super::super::hls_origin_source_kind(InputType::Library), HlsOriginSourceKind::DirectMediaPlaylist);
}

#[test]
fn hls_origin_resolution_preserves_legacy_built_xtream_origin_url() {
    let input = ConfigInput {
        id: 7,
        name: Arc::from("xtream"),
        input_type: InputType::Xtream,
        url: "http://origin.example.com/base".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        ..ConfigInput::default()
    };

    let origin =
        super::super::build_hls_origin_resolution(&input, "http://other.example.com/live/other/creds/1025126.m3u8")
            .expect("xtream origin should resolve");

    assert_eq!(origin.session_entry_url.as_str(), "http://other.example.com/live/other/creds/1025126.m3u8");
    assert_eq!(origin.hls_url, origin.session_entry_url.as_str());
    assert!(origin.session_entry_url.url_failover_provider().is_none());
}

#[test]
fn hls_origin_resolution_uses_m3u_playlist_item_url() {
    let input = ConfigInput {
        id: 9,
        name: Arc::from("m3u"),
        input_type: InputType::M3u,
        url: "http://playlist.example.com/list.m3u".to_string(),
        ..ConfigInput::default()
    };

    let origin = super::super::build_hls_origin_resolution(&input, "http://media.example.com/live/channel/index.m3u8")
        .expect("m3u hls origin should resolve");
    let source = super::super::build_hls_origin_source(&input, "stable-item");

    assert_eq!(origin.session_entry_url.as_str(), "http://media.example.com/live/channel/index.m3u8");
    assert_eq!(source.source_kind, HlsOriginSourceKind::M3uMediaPlaylist);
    assert_eq!(source.session_key().stable_value(), "input:9|hls|stable-item");
}

#[tokio::test]
async fn hls_cache_entry_returns_master_playlist_without_origin_refresh_or_session_creation() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();

    let response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        Some("/iptv"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).and_then(|value| value.to_str().ok()),
        Some("private, no-store, no-cache, must-revalidate")
    );
    assert!(response.headers().get(header::LOCATION).is_none());
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    assert!(response.headers().get(header::VARY).is_none());
    let content_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .expect("content length");
    let body = response_body(response).await;
    assert_eq!(content_length, body.len());
    let body = std::str::from_utf8(&body).expect("master playlist should be UTF-8");
    assert_eq!(body.matches("/iptv").count(), 1);
    let variant_uri = body.lines().nth(2).expect("single variant URI");
    assert!(variant_uri.starts_with("/iptv/hls/shared/live/"));
    assert!(variant_uri.ends_with("/manifest.m3u8"));
    let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(variant_uri).to_string());
    assert_eq!(access_lease_id.0.len(), 22);
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(variant_uri).to_string());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .expect("entry access lease");
    assert!(app_state
        .active_users
        .get_and_update_user_session(&lease.username, &lease.user_session_token)
        .await
        .is_some());
    assert!(app_state.hls.proxy.sessions().get_by_key(&session_key).await.is_none());
    assert_eq!(app_state.hls.proxy.metrics().snapshot().refresh_started, 0);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn hls_cache_entry_master_playlist_uses_cache_when_target_hls_share_enabled() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    configure_default_test_server(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.password = "hls-pass".to_string();
    let input = test_hls_input();
    let target = test_hls_share_target(true);
    let original_hls_entry_path = super::super::build_virtual_hls_entry_path(&target, &input, &user, 1001);

    let response = super::super::handle_hls_stream_request(
        &test_fingerprint(),
        &app_state,
        &user,
        &target,
        None,
        None,
        "http://origin.example.com/live/user/pass/1001.m3u8",
        None,
        test_hls_entry_stream_context(1001, "80510", Some(2_500_000)),
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &original_hls_entry_path,
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let (bandwidth, variant_uri) = single_variant_master_playlist(response).await;
    assert_eq!(bandwidth, 3_000_000);
    assert!(variant_uri.starts_with("/hls/shared/live/"));
    assert!(variant_uri.ends_with("/manifest.m3u8"));
    assert_eq!(app_state.hls.proxy.access_leases().read().await.len(), 1);

    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&variant_uri).to_string());
    let origin_key = HlsSessionKey::new(input.id, "80510");
    let virtual_id_key = HlsSessionKey::new(input.id, "1001");
    assert_eq!(origin_key.stable_value(), "input:1|hls|80510");
    assert_eq!(proxy_session_id, build_proxy_session_id(&origin_key, &app_state.get_encrypt_secret()));
    assert_ne!(proxy_session_id, build_proxy_session_id(&virtual_id_key, &app_state.get_encrypt_secret()));

    let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&variant_uri).to_string());
    let lease = app_state
        .hls
        .proxy
        .access_leases()
        .write()
        .await
        .response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .expect("access lease");
    assert_eq!(lease.stream_ref, "80510");
    assert_eq!(lease.virtual_id, 1001);
    assert_eq!(lease.known_bitrate_bps, Some(2_500_000));
}

#[tokio::test]
async fn hls_cache_entry_shares_content_session_across_targets_but_keeps_distinct_virtual_leases() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    configure_default_test_server(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.password = "hls-pass".to_string();
    let input = test_hls_input();
    let first_target = test_hls_share_target(true);
    let mut second_target = test_hls_share_target(true);
    second_target.id = 2;

    let first_response = super::super::handle_hls_stream_request(
        &test_fingerprint(),
        &app_state,
        &user,
        &first_target,
        None,
        None,
        "http://origin.example.com/live/user/pass/1001.m3u8",
        None,
        test_hls_entry_stream_context(1001, "80510", None),
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &super::super::build_virtual_hls_entry_path(&first_target, &input, &user, 1001),
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();
    let second_response = super::super::handle_hls_stream_request(
        &test_fingerprint_with_addr(test_addr_with_port(55124)),
        &app_state,
        &user,
        &second_target,
        None,
        None,
        "http://origin.example.com/live/user/pass/9007.m3u8",
        None,
        test_hls_entry_stream_context(9007, "80510", None),
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &super::super::build_virtual_hls_entry_path(&second_target, &input, &user, 9007),
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();

    assert_eq!(first_response.status(), StatusCode::OK);
    assert_eq!(second_response.status(), StatusCode::OK);
    let first_variant_uri = single_variant_uri(first_response).await;
    let second_variant_uri = single_variant_uri(second_response).await;
    assert_eq!(
        proxy_session_id_from_variant_uri(&first_variant_uri),
        proxy_session_id_from_variant_uri(&second_variant_uri)
    );
    assert_ne!(
        access_lease_id_from_variant_uri(&first_variant_uri),
        access_lease_id_from_variant_uri(&second_variant_uri)
    );

    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
    let first_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
    let second_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());
    let now_ms = super::super::current_time_millis();
    let mut leases = app_state.hls.proxy.access_leases().write().await;
    let first_lease = leases.response_snapshot(&first_lease_id, &proxy_session_id, now_ms).expect("first access lease");
    let second_lease =
        leases.response_snapshot(&second_lease_id, &proxy_session_id, now_ms).expect("second access lease");
    assert_eq!(first_lease.virtual_id, 1001);
    assert_eq!(second_lease.virtual_id, 9007);
    assert_eq!(first_lease.stream_ref, "80510");
    assert_eq!(second_lease.stream_ref, "80510");
}

#[tokio::test]
async fn hls_cache_entry_uses_legacy_path_when_target_hls_share_disabled() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    configure_default_test_server(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.password = "hls-pass".to_string();
    let input = test_hls_input();
    let target = test_hls_share_target(false);
    let original_hls_entry_path = super::super::build_virtual_hls_entry_path(&target, &input, &user, 12345);

    let response = super::super::handle_hls_stream_request(
        &test_fingerprint(),
        &app_state,
        &user,
        &target,
        None,
        None,
        "http://origin.example.com/live/user/pass/12345.m3u8",
        None,
        test_hls_entry_stream_context(12345, "80510", None),
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &original_hls_entry_path,
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();

    let location = response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).unwrap_or("");
    assert!(!location.contains("/hls/shared/live/"));
    assert!(app_state.hls.proxy.access_leases().read().await.is_empty());
    assert_eq!(app_state.hls.proxy.metrics().snapshot().refresh_started, 0);
}

#[tokio::test]
async fn legacy_hls_token_route_renders_channel_unavailable_inline_when_target_hls_share_enabled() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    app_state.app_config.config.store(Arc::new(Config {
        custom_stream_response_enabled: true,
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            hls_cache: Some(HlsCacheConfigDto::default()),
            ..Default::default()
        })),
        ..Default::default()
    }));
    configure_default_test_server(&app_state);
    enable_channel_unavailable_custom_response(&app_state);
    let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
    let input = test_hls_input();
    let target = test_hls_share_target(true);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    let encrypt_secret = app_state.get_encrypt_secret();
    let legacy_manifest = rewrite_hls(
        &user,
        &RewriteHlsProps {
            secret: &encrypt_secret,
            base_url: "",
            content: "#EXTM3U\n#EXTINF:4.0,\nseg.ts\n",
            hls_url: "http://origin.example.com/live/user/pass/12345.m3u8".to_string(),
            target_id: target.id,
            virtual_id: 12345,
            input_id: input.id,
            user_token: Some("legacy-session-token"),
            origin_provider: None,
            playlist_kind: None,
        },
    );
    let token = legacy_manifest
        .lines()
        .find_map(|line| line.rsplit_once('/').map(|(_, token)| token.trim().to_string()))
        .expect("legacy hls segment token should be rendered");

    let response = super::super::hls_api_stream_resolved(
        test_fingerprint(),
        HeaderMap::new(),
        Arc::clone(&app_state),
        Arc::clone(&user),
        Arc::new(target),
        input.id,
        12345,
        token,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
    assert!(app_state.hls.proxy.access_leases().read().await.is_empty());
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn legacy_hls_token_route_with_invalid_token_returns_bad_request_when_target_hls_share_enabled() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
    let input = test_hls_input();
    let target = test_hls_share_target(true);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());

    let response = super::super::hls_api_stream_resolved(
        test_fingerprint(),
        HeaderMap::new(),
        Arc::clone(&app_state),
        user,
        Arc::new(target),
        input.id,
        12345,
        "not-a-valid-legacy-token.ts".to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(app_state.hls.proxy.access_leases().read().await.is_empty());
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn hls_cache_entry_leases_update_effective_origin_acquire_policy_for_shared_session() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut soft_user = ProxyUserCredentials::default();
    soft_user.username = "soft-user".to_string();
    soft_user.soft_priority = 20;
    let mut normal_user = ProxyUserCredentials::default();
    normal_user.username = "normal-user".to_string();
    normal_user.priority = -5;
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());

    let soft_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &soft_user,
        origin_source.clone(),
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Soft),
        None,
    )
    .await;
    assert_eq!(soft_response.status(), StatusCode::OK);
    let soft_snapshot =
        app_state.hls.proxy.access_lease_session_snapshot(&proxy_session_id, super::super::current_time_millis()).await;
    let soft_policy = soft_snapshot.effective_origin_policy.expect("soft policy");
    assert_eq!(soft_policy.connection_kind, ConnectionKind::Soft);
    assert_eq!(soft_policy.priority, soft_user.soft_priority);

    // Different user/family, same shared HLS session. Normal media admission must upgrade
    // the future origin-account acquire policy without changing the shared session identity.
    let normal_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &normal_user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    assert_eq!(normal_response.status(), StatusCode::OK);
    let normal_snapshot =
        app_state.hls.proxy.access_lease_session_snapshot(&proxy_session_id, super::super::current_time_millis()).await;
    let normal_policy = normal_snapshot.effective_origin_policy.expect("normal policy");
    assert_eq!(normal_policy.connection_kind, ConnectionKind::Normal);
    assert_eq!(normal_policy.priority, normal_user.priority);

    let (session, _) = app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            session_key,
            super::super::build_hls_origin_source(&input, "12345"),
            &app_state.get_encrypt_secret(),
            super::super::current_time_millis(),
        )
        .await;
    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            &proxy_session_id,
            super::super::current_time_millis(),
        )
        .await;
    let session_policy = session.read().await.effective_origin_acquire_policy_or_default();
    assert_eq!(session_policy.connection_kind, ConnectionKind::Normal);
    assert_eq!(session_policy.priority, normal_user.priority);
}

#[tokio::test]
async fn hls_entry_origin_reservation_blocks_foreign_sessions_only_after_media_confirmation() {
    let input = single_hls_provider_input("available-provider");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let session_owner = "hls-cache:test-session";

    let reservation = super::super::try_reserve_hls_entry_origin_account_for_redirect(
        &app_state,
        &test_fingerprint(),
        &user,
        &input,
        12345,
        "http://account.example.com/live/account-user/account-pass/12345.m3u8",
        "hls-session-token",
        session_owner,
        crate::model::PlaybackKind::LiveHls,
        super::super::hls_origin_account_reservation_ttl_secs_fallback(),
        UserConnectionPermission::Allowed,
        ConnectionKind::Normal,
        false,
    )
    .await
    .expect("provider reservation should be acquired before provisioning redirect");

    assert_eq!(reservation.request_url, "http://account.example.com/live/account-user/account-pass/12345.m3u8");
    assert!(reservation.selected_provider_config.is_some());
    assert!(app_state.active_users.active_streams().await.is_empty());
    app_state.connection_manager.release_provider_handle(reservation.provider_handle);

    // A manifest-only entry reservation is still unconfirmed, so it must not block a
    // different HLS session: an abandoned start or a manifest retry has to leave the
    // provider capacity free for unrelated clients behind the same reverse proxy.
    let other_handle = app_state.active_provider.acquire_connection_with_grace_for_session(
        &input.name,
        &test_addr_with_port(55251),
        false,
        0,
        ConnectionKind::Normal,
        Some("other-owner"),
    );
    assert!(other_handle.is_some(), "an unconfirmed entry reservation must not block other HLS sessions");
    app_state.connection_manager.release_provider_handle(other_handle);

    let same_owner_handle = app_state.active_provider.acquire_connection_with_grace_for_session(
        &input.name,
        &test_addr_with_port(55252),
        false,
        0,
        ConnectionKind::Normal,
        Some(session_owner),
    );
    assert!(same_owner_handle.is_some(), "reserved provider must be reusable by the same HLS session owner");
    app_state.connection_manager.release_provider_handle(same_owner_handle);

    // Once real media activity confirms the lease, it reserves the provider slot and
    // other sessions are routed away from it.
    app_state.active_provider.confirm_playback_activity(session_owner);
    assert!(
        app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &test_addr_with_port(55253),
                false,
                0,
                ConnectionKind::Normal,
                Some("other-owner"),
            )
            .is_none(),
        "a confirmed reservation must stay blocked for other HLS sessions"
    );
}

#[tokio::test]
async fn hls_entry_origin_reservation_uses_persisted_alias_manifest_url() {
    use shared::model::PlaylistGroup;
    use tuliprox_repository::{get_input_m3u_playlist_file_path, get_input_storage_path, persist_input_m3u_playlist};

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = ConfigInput {
        id: 1,
        name: Arc::from("primary-account"),
        input_type: InputType::M3u,
        url: "http://playlist.example/list.m3u?access_key=primary-playlist-key".to_string(),
        enabled: true,
        max_connections: 1,
        aliases: Some(vec![crate::model::ConfigInputAlias {
            id: 2,
            name: Arc::from("alias-account"),
            url: "http://playlist.example/list.m3u?access_key=alias-playlist-key".to_string(),
            username: None,
            password: None,
            max_connections: 0,
            priority: 0,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    };
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let mut config = (*app_state.app_config.config.load_full()).clone();
    config.storage_dir = temp.path().to_string_lossy().into_owned();
    app_state.app_config.config.store(Arc::new(config));

    let alias_name = "alias-account".intern();
    let storage_path = get_input_storage_path(&alias_name, &app_state.app_config.config.load().storage_dir)
        .await
        .expect("alias storage should be created");
    let playlist_path = get_input_m3u_playlist_file_path(&storage_path, &alias_name);
    let alias_playlist = vec![PlaylistGroup {
        id: 1,
        title: "Live".intern(),
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                id: "news24hd".intern(),
                input_stream_id: "news24hd".intern(),
                url: "http://stream.example:4000/news24hd/mono.m3u8?token=alias-stream-token".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        }],
        xtream_cluster: XtreamCluster::Live,
    }];
    persist_input_m3u_playlist(&app_state.app_config, &playlist_path, &alias_playlist)
        .await
        .expect("alias playlist should persist");

    let primary_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace_for_session(
            &input.name,
            &test_fingerprint().addr,
            false,
            0,
            ConnectionKind::Normal,
            Some("primary-session"),
        )
        .expect("primary account should be allocated");

    let reservation = super::super::try_reserve_hls_entry_origin_account_for_redirect(
        &app_state,
        &test_fingerprint(),
        &{
            let mut creds = ProxyUserCredentials::default();
            creds.username = "hls-user".to_string();
            creds
        },
        &input,
        12345,
        "http://stream.example:4000/news24hd/mono.m3u8?token=primary-stream-token",
        "alias-session-token",
        "alias-session-owner",
        crate::model::PlaybackKind::LiveHls,
        super::super::hls_origin_account_reservation_ttl_secs_fallback(),
        UserConnectionPermission::Allowed,
        ConnectionKind::Normal,
        false,
    )
    .await
    .expect("alias provider reservation should succeed");

    assert_eq!(
        reservation.selected_provider_config.as_ref().map(|provider| provider.name.as_ref()),
        Some("alias-account")
    );
    assert_eq!(reservation.request_url, "http://stream.example:4000/news24hd/mono.m3u8?token=alias-stream-token");

    app_state.connection_manager.release_provider_handle(reservation.provider_handle);
    app_state.connection_manager.release_provider_handle(Some(primary_handle));
}

#[tokio::test]
async fn hls_virtual_entry_reservation_uses_input_stream_id_for_shared_session_owner() {
    let input = single_hls_provider_input("origin-id-reservation-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    let target = Arc::new(test_m3u_hls_share_target());
    let item = test_m3u_hls_item(
        &input,
        1001,
        "80510",
        "http://account.example.com/live/account-user/account-pass/80510.m3u8",
    );
    cache_test_m3u_hls_item(&app_state, &target, item).await;
    let stream_identity = super::super::HlsEntryStreamIdentity::new(1001, "80510").expect("input stream identity");
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();

    assert!(
        super::super::try_reserve_hls_virtual_entry_origin_account_for_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            &target,
            &input,
            &stream_identity,
        )
        .await
    );

    let expected_key = HlsSessionKey::new(input.id, "80510");
    let expected_proxy_session_id = build_proxy_session_id(&expected_key, &app_state.get_encrypt_secret());
    let expected_owner = crate::api::model::build_hls_origin_session_owner(&expected_proxy_session_id);
    let same_owner_handle = app_state.active_provider.acquire_connection_with_grace_for_session(
        &input.name,
        &test_addr_with_port(55253),
        false,
        0,
        ConnectionKind::Normal,
        Some(&expected_owner),
    );
    assert!(same_owner_handle.is_some(), "reservation must be owned by input:1|hls|80510, not virtual_id=1001");
    app_state.connection_manager.release_provider_handle(same_owner_handle);
}

#[tokio::test]
async fn hls_cache_entry_creates_new_lease_for_same_user_session_and_proxy_session() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");

    let first_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source.clone(),
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let first_variant_uri = single_variant_uri(first_response).await;
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
    let first_access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
    let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_access_lease_id).await;

    let second_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let second_variant_uri = single_variant_uri(second_response).await;
    assert_ne!(
        access_lease_id_from_variant_uri(&first_variant_uri),
        access_lease_id_from_variant_uri(&second_variant_uri)
    );
    let second_access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());
    let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &second_access_lease_id).await;
    assert_ne!(first_session_token, second_session_token);
}

#[tokio::test]
async fn hls_cache_entry_creates_new_lease_after_manifest_touch() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");

    let first_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source.clone(),
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let first_variant_uri = single_variant_uri(first_response).await;
    let first_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
    let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
    let now_ms = super::super::current_time_millis();

    assert!(matches!(
        app_state
            .hls
            .proxy
            .touch_manifest_access_lease(
                &first_lease_id,
                &proxy_session_id,
                now_ms,
                None,
                Some(super::HlsAccessLeasePendingDeadline::Bootstrap {
                    deadline_ms: now_ms.saturating_add(super::super::hls_pending_bootstrap_window_ms(&app_state)),
                }),
                super::super::hls_access_lease_ttl_ms(&app_state),
            )
            .await,
        HlsAccessLeaseTouch::Touched { .. }
    ));

    let second_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let second_variant_uri = single_variant_uri(second_response).await;

    assert_ne!(
        access_lease_id_from_variant_uri(&first_variant_uri),
        access_lease_id_from_variant_uri(&second_variant_uri)
    );
    let second_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());
    let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &second_lease_id).await;
    assert_ne!(first_session_token, second_session_token);
}

#[tokio::test]
async fn hls_cache_entry_ignores_existing_pending_lease_for_new_playback() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let proxy_session_id = build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());
    let old_lease_id = HlsAccessLeaseId("old-pending-lease".to_string());
    let old_session_token = "old-hls-session-token";
    let old_issued_at_ms = super::super::current_time_millis().saturating_sub(6_000);
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            old_lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            old_session_token.to_string(),
            1,
            "12345".to_string(),
            12345,
            old_issued_at_ms,
            super::super::hls_access_lease_ttl_ms(&app_state),
        ))
        .await;

    let response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let variant_uri = single_variant_uri(response).await;
    let new_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&variant_uri).to_string());
    let new_session_token = access_lease_session_token(&app_state, &proxy_session_id, &new_lease_id).await;

    assert_ne!(old_lease_id, new_lease_id);
    assert_ne!(old_session_token, new_session_token);
}

#[tokio::test]
async fn hls_cache_entry_master_playlist_for_xtream_uses_stream_ref_session_identity() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input =
        ConfigInput { id: 7, name: Arc::from("xtream-input"), input_type: InputType::Xtream, ..ConfigInput::default() };
    let origin_source = super::super::build_hls_origin_source(&input, "80510");
    let expected_proxy_session_id =
        build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());

    let response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        80510,
        None,
        None,
        None,
        "http://origin.example.com/live/user/pass/80510.m3u8",
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let variant_uri = single_variant_uri(response).await;
    assert!(variant_uri.starts_with(&format!("/hls/shared/live/{}/", expected_proxy_session_id.0)));
    assert!(variant_uri.ends_with("/manifest.m3u8"));
    assert!(app_state.hls.proxy.sessions().get_by_key(&HlsSessionKey::new(7, "80510")).await.is_none());
    assert_eq!(app_state.hls.proxy.metrics().snapshot().refresh_started, 0);
}

#[tokio::test]
async fn hls_cache_entry_master_playlist_for_m3u_uses_stream_ref_session_identity() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input =
        ConfigInput { id: 9, name: Arc::from("m3u-input"), input_type: InputType::M3u, ..ConfigInput::default() };
    let origin_source = super::super::build_hls_origin_source(&input, "70001");
    let expected_proxy_session_id =
        build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());

    let response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        70001,
        None,
        None,
        None,
        "http://media.example.com/channel/playlist.m3u8",
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        Some("/iptv"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let variant_uri = single_variant_uri(response).await;
    assert!(variant_uri.starts_with(&format!("/iptv/hls/shared/live/{}/", expected_proxy_session_id.0)));
    assert!(variant_uri.ends_with("/manifest.m3u8"));
    assert!(app_state.hls.proxy.sessions().get_by_key(&HlsSessionKey::new(9, "70001")).await.is_none());
    assert_eq!(app_state.hls.proxy.metrics().snapshot().refresh_started, 0);
}

#[tokio::test]
async fn hls_proxy_manifest_invalid_token_starts_no_origin_work() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);

    let response = get_response(
        Arc::clone(&app_state),
        "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/not-a-valid-token/manifest.m3u8",
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(app_state.hls.proxy.metrics().snapshot().refresh_started, 0);
    assert!(app_state.hls.proxy.sessions().is_empty().await);
}
