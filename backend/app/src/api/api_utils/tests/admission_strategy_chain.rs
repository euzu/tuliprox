use super::{
    create_test_app_state_with_stream_config, create_test_fingerprint, create_test_live_channel,
    create_test_provider_app_config, resolve_admission_with_strategies, AdmissionRequest, EvictionReentryGuard,
};
use crate::model::ProxyUserCredentials;
use shared::{
    model::{AdmissionStrategy, UserConnectionPermission},
    utils::Internable,
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_session::{
    admission::{evaluate_remaining_strategies_after_grace, get_effective_admission_strategies},
    GraceResolutionContext,
};

#[tokio::test]
async fn effective_admission_strategies_use_legacy_grace_when_field_missing() {
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
        admission_strategies: None,
    });

    assert_eq!(
        get_effective_admission_strategies(&app_state.admission_ctx()).as_ref(),
        &[shared::model::AdmissionStrategy::GraceHoldStream][..]
    );
}

#[tokio::test]
async fn effective_admission_strategies_respect_explicit_empty_list() {
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
        admission_strategies: Some(vec![]),
    });

    assert!(get_effective_admission_strategies(&app_state.admission_ctx()).is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn evaluate_remaining_strategies_evicts_after_used_grace() {
    // Strategies: [GraceHoldStream, EvictUserOldest]
    // Grace was used at index 0, so only EvictUserOldest (index 1) is evaluated.
    // The new request stays retryable while the evicted stream still owns its provider slot.
    let strategies = vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest];
    let grace_context = GraceResolutionContext { strategy_index: 0, strategies: strategies.into(), kind: None };

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
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
    });
    let provider_config = create_test_provider_app_config();
    app_state.app_config.sources.store(provider_config.sources.load_full());
    app_state.active_provider.update_config(&app_state.app_config);

    let addr1: SocketAddr = "127.0.0.1:55701".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:55702".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "remaining-evict".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted",
            virtual_id: 55701,
            provider: "provider-evict",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55701,
            meter_uid: 55701,
            username: "remaining-evict",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-evict".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted"),
        })
        .await
        .expect("stream should be created");

    let provider_handle = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &"provider_1".intern(),
            &addr1,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some("tok-counted"),
        )
        .expect("old stream should occupy the only provider slot");
    assert!(app_state.active_provider.register_body_owner(provider_handle.allocation_id));
    let close_rx = app_state.connection_manager.register_close_socket(addr1);
    let manager = Arc::clone(&app_state.connection_manager);
    let provider = Arc::clone(&app_state.active_provider);
    let release_body = Arc::new(tokio::sync::Notify::new());
    let release_body_after_timeout = Arc::clone(&release_body);
    let close_task = tokio::spawn(async move {
        assert_eq!(close_rx.await.expect("kick close signal"), shared::model::DisconnectReason::ClientKicked);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        manager.release_provider_deferred(&addr1).await;
        assert_eq!(provider.get_provider_connections_count(), 1, "body owner still holds provider capacity");
        release_body_after_timeout.notified().await;
        provider.release_handle(&provider_handle);
        provider_handle.completion_token.as_ref().expect("body completion token").cancel();
        manager.unregister_close_socket(&addr1);
    });

    let request = || AdmissionRequest {
        username: "remaining-evict",
        max_connections: 1,
        soft_connections: 0,
        client_ip: &fingerprint2.client_ip,
        request_addr: &fingerprint2.addr,
        use_session_admission: true,
        session_token: Some("tok-new"),
        activate_unbound_session: true,
        eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        evaluate_remaining_strategies_after_grace(
            &app_state.admission_ctx(),
            request(),
            &grace_context,
            Some(crate::api::model::ConnectionKind::Normal),
        ),
    )
    .await
    .expect("admission must remain retryable while the provider slot is held");

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let retry = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        resolve_admission_with_strategies(&app_state.admission_ctx(), request()),
    )
    .await
    .expect("retry must remain bounded while the provider slot is held");
    assert_eq!(retry.admission.permission(), UserConnectionPermission::Exhausted);

    release_body.notify_one();
    close_task.await.expect("old transport cleanup");
    let result = resolve_admission_with_strategies(&app_state.admission_ctx(), request()).await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::Allowed,
        "EvictUserOldest should free the slot"
    );
    assert!(result.grace_context.is_none(), "no grace context on eviction success");
    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        0,
        "admission must not return while the evicted stream still owns the provider slot"
    );
    let replacement = app_state.active_provider.acquire_connection_with_grace_for_session(
        &"provider_1".intern(),
        &addr2,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
        Some("tok-new"),
    );
    assert!(replacement.is_some(), "newly admitted stream must acquire the freed provider slot");
    if let Some(replacement) = replacement {
        app_state.active_provider.release_handle(&replacement);
    }
}

#[tokio::test]
async fn evaluate_remaining_strategies_skips_no_match_and_uses_later_eviction() {
    // Strategies: [GraceHoldStream, EvictUserSameIpOldest, EvictUserOldest]
    // Grace was at index 0, remaining are EvictUserSameIpOldest (index 1) and EvictUserOldest (index 2).
    // The existing counted session is at a DIFFERENT IP, so EvictUserSameIpOldest -> NoMatch.
    // EvictUserOldest succeeds -> Allowed.
    let strategies = vec![
        AdmissionStrategy::GraceHoldStream,
        AdmissionStrategy::EvictUserSameIpOldest,
        AdmissionStrategy::EvictUserOldest,
    ];
    let grace_context = GraceResolutionContext { strategy_index: 0, strategies: strategies.into(), kind: None };

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
        admission_strategies: Some(vec![
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::EvictUserOldest,
        ]),
    });

    let addr1: SocketAddr = "127.0.0.1:55801".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:55802".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "remaining-skip-no-match".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted",
            virtual_id: 55801,
            provider: "provider-skip",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55801,
            meter_uid: 55801,
            username: "remaining-skip-no-match",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-skip".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "remaining-skip-no-match",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Normal),
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::Allowed,
        "EvictUserSameIpOldest should NoMatch, EvictUserOldest should succeed"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_empty_slice_denies() {
    // Strategies: [GraceHoldStream]
    // Grace was at index 0, remaining slice is empty -> exhausted.
    let strategies = vec![AdmissionStrategy::GraceHoldStream];
    let grace_context = GraceResolutionContext { strategy_index: 0, strategies: strategies.into(), kind: None };

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
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let addr: SocketAddr = "10.0.0.5:55901".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "no-remaining-strategies",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        None,
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted, "empty remaining slice should deny");
}

#[tokio::test]
async fn evaluate_remaining_strategies_preserves_soft_kind_on_exhausted() {
    // Strategies: [GraceHoldStream]
    // Grace was at index 0, remaining slice is empty -> exhausted.
    // grace_context.kind is Soft — must be preserved in the exhausted result.
    let strategies = vec![AdmissionStrategy::GraceHoldStream];
    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: strategies.into(),
        kind: Some(crate::api::model::ConnectionKind::Soft),
    };

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
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let addr: SocketAddr = "10.0.0.6:55902".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "soft-kind-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-soft"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-soft"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Soft),
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted, "empty remaining slice should deny");
    assert_eq!(
        result.admission.kind(),
        Some(crate::api::model::ConnectionKind::Soft),
        "exhausted result must preserve the original Soft connection kind"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_does_not_retry_used_prefix() {
    // Strategies: [GraceHoldStream, GraceInstantStream, EvictUserOldest]
    // Grace was at index 1 (GraceInstantStream).
    // Remaining slice: [EvictUserOldest] (index 2).
    // GraceHoldStream (index 0) must NOT be re-evaluated.
    let strategies = vec![
        AdmissionStrategy::GraceHoldStream,
        AdmissionStrategy::GraceInstantStream,
        AdmissionStrategy::EvictUserOldest,
    ];
    let strategies_for_config = strategies.clone();
    let grace_context = GraceResolutionContext { strategy_index: 1, strategies: strategies.into(), kind: None };

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
        admission_strategies: Some(strategies_for_config),
    });

    let addr1: SocketAddr = "127.0.0.1:56001".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:56002".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "remaining-no-retry".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted",
            virtual_id: 56001,
            provider: "provider-no-retry",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 56001,
            meter_uid: 56001,
            username: "remaining-no-retry",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-no-retry".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "remaining-no-retry",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Normal),
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::Allowed,
        "only EvictUserOldest should be evaluated, not GraceHoldStream"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_empty_slice_uses_original_kind_not_context_kind() {
    // grace_context.kind = Normal, original_kind = Soft
    // remaining slice is empty -> exhausted result must use original_kind.
    // This proves the empty-slice branch uses original_kind, not grace_context.kind.
    let strategies = vec![AdmissionStrategy::GraceHoldStream];
    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: strategies.into(),
        kind: Some(crate::api::model::ConnectionKind::Normal),
    };
    let original_kind = Some(crate::api::model::ConnectionKind::Soft);

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
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let addr: SocketAddr = "10.0.0.7:55903".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "kind-mismatch-empty",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-empty"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-empty"),
        },
        &grace_context,
        original_kind,
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(
        result.admission.kind(),
        original_kind,
        "exhausted result must use original_kind (Soft), not grace_context.kind (Normal)"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_later_grace_uses_original_kind_not_context_kind() {
    // grace_context.kind = Normal, original_kind = Soft
    // Strategies: [GraceHoldStream, GraceInstantStream]
    // Grace was used at index 0 (GraceHoldStream).
    // Remaining slice contains GraceInstantStream (index 1).
    // When the helper returns Grace for the remaining strategy, the new
    // GraceResolutionContext.kind must be original_kind (Soft), not grace_context.kind (Normal).
    // This proves build_grace_ctx uses original_kind as source of truth.
    let strategies = vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::GraceInstantStream];
    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: strategies.into(),
        kind: Some(crate::api::model::ConnectionKind::Normal),
    };
    let original_kind = Some(crate::api::model::ConnectionKind::Soft);

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
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::GraceInstantStream]),
    });

    let addr1: SocketAddr = "127.0.0.1:55710".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.8:55711".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "kind-mismatch-grace".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted-grace",
            virtual_id: 55710,
            provider: "provider-grace-kind",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55710,
            meter_uid: 55710,
            username: "kind-mismatch-grace",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-grace-kind".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted-grace"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "kind-mismatch-grace",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new-grace"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new-grace"),
        },
        &grace_context,
        original_kind,
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::GracePeriod,
        "remaining GraceInstantStream should grant GracePeriod"
    );
    assert!(result.grace_context.is_some(), "grace_context must be present when grace is granted");
    assert_eq!(
        result.grace_context.as_ref().unwrap().kind,
        original_kind,
        "GraceResolutionContext.kind in the result must be original_kind (Soft), not grace_context.kind (Normal)"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_falls_through_after_failed_grace_grant() {
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
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
    });

    let first_addr: std::net::SocketAddr = "127.0.0.1:55151".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55152".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state.connection_manager.add_connection(&second_addr).await;

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "fallthrough".to_string();
    session_user.max_connections = 1;
    session_user.soft_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "tok-first",
            virtual_id: 1,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "fallthrough",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/live/1.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-first"),
        })
        .await;

    assert!(app_state.active_users.grant_grace("fallthrough").await);

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "tok-second",
            virtual_id: 2,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/2.ts",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "fallthrough",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: crate::api::model::ConnectionKind::Soft,
            priority: 0,
            soft_priority: 10,
            fingerprint: &second_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/live/2.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-second"),
        })
        .await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "fallthrough",
            max_connections: 1,
            soft_connections: 1,
            client_ip: "127.0.0.1",
            request_addr: &"127.0.0.1:55153".parse().unwrap_or_else(|_| unreachable!()),
            use_session_admission: true,
            session_token: Some("tok-third"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-third"),
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(crate::api::model::ConnectionKind::Normal));
    assert_eq!(grace_mode, None);
}
