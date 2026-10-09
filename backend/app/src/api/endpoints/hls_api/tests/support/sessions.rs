use super::super::{
    AppState, Arc, ConfigInput, ConnectionKind, CreateUserSessionParams, Duration, Fingerprint, HlsAccessLease,
    HlsAccessLeaseId, HlsAccessLeaseTiming, HlsOriginAccountBinding, HlsPlaybackFamilyKey, HlsSessionHandle,
    PlaylistItemType, ProxySessionId, ProxyUserCredentials, SocketAddr, UserConnectionPermission,
};

pub(in crate::api::endpoints::hls_api::tests) async fn create_bound_hls_test_session(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_ref: &str,
    account_name: &str,
    now_ms: u64,
) -> HlsSessionHandle {
    let origin_source = super::super::super::build_hls_origin_source(input, stream_ref);
    let session_key = origin_source.session_key();
    let (session, _) = app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            session_key,
            origin_source,
            &app_state.get_encrypt_secret(),
            now_ms,
        )
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    {
        let mut session_guard = session.write().await;
        session_guard.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from(account_name),
            &proxy_session_id,
            now_ms,
        ));
    }
    session
}

pub(in crate::api::endpoints::hls_api::tests) async fn create_unbound_hls_test_session(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_ref: &str,
    now_ms: u64,
) -> HlsSessionHandle {
    let origin_source = super::super::super::build_hls_origin_source(input, stream_ref);
    let session_key = origin_source.session_key();
    app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            session_key,
            origin_source,
            &app_state.get_encrypt_secret(),
            now_ms,
        )
        .await
        .0
}

pub(in crate::api::endpoints::hls_api::tests) async fn activate_test_hls_access_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_id: &str,
    now_ms: u64,
    ttl_ms: u64,
) {
    let lease_id = HlsAccessLeaseId(lease_id.to_string());
    let valid_window_ms = ttl_ms.saturating_mul(10).max(ttl_ms);
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            valid_window_ms,
        ))
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: ttl_ms, valid_window_ms },
        )
        .await
        .is_activated());
}

pub(in crate::api::endpoints::hls_api::tests) fn test_addr() -> SocketAddr {
    "127.0.0.1:55123".parse().unwrap_or_else(|_| unreachable!())
}

pub(in crate::api::endpoints::hls_api::tests) fn test_addr_with_port(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

pub(in crate::api::endpoints::hls_api::tests) fn test_fingerprint() -> Fingerprint {
    Fingerprint::new("test".to_string(), "127.0.0.1".to_string(), test_addr())
}

pub(in crate::api::endpoints::hls_api::tests) fn test_fingerprint_with_addr(addr: SocketAddr) -> Fingerprint {
    Fingerprint::new(format!("test-{}", addr.port()), "127.0.0.1".to_string(), addr)
}

pub(in crate::api::endpoints::hls_api::tests) async fn create_active_hls_user_session(app_state: &Arc<AppState>) {
    create_active_hls_user_session_with(
        app_state,
        "hls-session-token",
        "origin-provider",
        "http://origin.example.com/live/12345.m3u8",
        test_addr(),
    )
    .await;
}

pub(in crate::api::endpoints::hls_api::tests) async fn create_active_hls_user_session_with(
    app_state: &Arc<AppState>,
    session_token: &str,
    provider: &str,
    stream_url: &str,
    addr: SocketAddr,
) {
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.max_connections = 1;
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 12345,
            provider,
            stream_url,
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
}

pub(in crate::api::endpoints::hls_api::tests) async fn mark_hls_user_session_exhausted(app_state: &Arc<AppState>) {
    let user = app_state.app_config.get_user_credentials("hls-user").expect("configured HLS test user");
    let addr = test_addr();
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "hls-session-token",
            virtual_id: 12345,
            provider: "origin-provider",
            stream_url: "http://origin.example.com/live/12345.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Exhausted,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_pending_test_hls_access_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) {
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            access_lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "12345".to_string(),
            12345,
            super::super::super::current_time_millis(),
            super::super::super::hls_access_lease_ttl_ms(app_state),
        ))
        .await;
}

pub(in crate::api::endpoints::hls_api::tests) async fn grant_hls_proxy_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
) -> String {
    create_active_hls_user_session(app_state).await;
    let now_ms = super::super::super::current_time_millis();
    let lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
    let family_key = HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key);
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            family_key,
            ProxySessionId(proxy_session_id.to_string()),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            super::super::super::hls_access_lease_ttl_ms(app_state),
        ))
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &ProxySessionId(proxy_session_id.to_string()),
            now_ms,
            HlsAccessLeaseTiming {
                active_window_ms: 5_000,
                valid_window_ms: super::super::super::hls_access_lease_ttl_ms(app_state),
            },
        )
        .await
        .is_activated());
    lease_id.0
}

pub(in crate::api::endpoints::hls_api::tests) async fn wait_for_provider_connection_count(
    app_state: &Arc<AppState>,
    expected: usize,
) {
    for _ in 0..50 {
        let actual = app_state.active_provider.get_provider_connections_count();
        if actual == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(app_state.active_provider.get_provider_connections_count(), expected);
}

pub(in crate::api::endpoints::hls_api::tests) async fn hls_session_last_media_at_ms(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
) -> Option<u64> {
    app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.to_string()))
        .await
        .expect("session should exist")
        .read()
        .await
        .activity
        .last_authorized_media_at_ms
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_no_hls_cache_stream_registered(
    app_state: &Arc<AppState>,
) {
    assert!(app_state.active_users.active_streams().await.is_empty());
}

pub(in crate::api::endpoints::hls_api::tests) async fn access_lease_session_token(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> String {
    app_state
        .hls
        .proxy
        .access_lease(access_lease_id, proxy_session_id, super::super::super::current_time_millis())
        .await
        .expect("access lease should exist")
        .user_session_token
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_hls_cache_stream_registered(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
) {
    let streams = app_state.active_users.active_streams().await;
    assert_eq!(streams.len(), 1);
    let stream = &streams[0];
    assert_eq!(stream.username, "hls-user");
    assert_eq!(stream.session_token.as_deref(), Some("hls-session-token"));
    assert_eq!(stream.provider.as_ref(), "origin-provider");
    assert_eq!(stream.channel.item_type, PlaylistItemType::LiveHls);
    assert!(stream.channel.shared);
    assert_eq!(
        stream.channel.shared_stream_id,
        Some(super::super::super::hls_cache_shared_stream_id(&ProxySessionId(proxy_session_id.to_string())))
    );
    assert_eq!(stream.channel.shared_joined_existing, Some(false));
    assert_eq!(stream.channel.url.as_ref(), format!("/hls/shared/live/{proxy_session_id}/manifest.m3u8"));
    assert!(!stream.channel.url.contains("test-access-lease"));
    assert!(!stream.channel.url.contains("hls-session-token"));
    assert!(!stream.channel.url.contains("origin.example.com"));
    assert!(!stream.channel.url.contains("/hls/hls-user/"));
}
