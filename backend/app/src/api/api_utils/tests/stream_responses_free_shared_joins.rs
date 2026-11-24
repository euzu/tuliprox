use super::{
    create_test_fingerprint, create_test_live_channel, create_test_provider_app_state, create_test_shared_target,
    evaluate_free_join, stream_response, PlaybackSource,
};
use crate::model::ProxyUserCredentials;
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use shared::{model::UserConnectionPermission, utils::Internable};
use std::{borrow::Cow, net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{SharedSubscriberId, RECORDING_PROXY_USERNAME};
use tuliprox_session::{PlaybackLifecycle, SharedStreamCtx, SharedStreamManager};

async fn register_test_shared_stream(
    app_state: &Arc<crate::api::model::AppState>,
    stream_url: &str,
    owner_addr: SocketAddr,
    owner_user: &str,
    owner_token: &str,
) {
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let owner_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &owner_addr, 0, crate::api::model::ConnectionKind::Normal)
        .expect("owner allocation should exist");
    let subscriber_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, owner_addr)
            .await
            .expect("shared cleanup admission should succeed");
    let shared_stream = futures::stream::pending::<Result<Bytes, std::io::Error>>();
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &owner_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), owner_handle)),
        pending_cleanup,
        0,
        crate::api::model::ConnectionKind::Normal,
        owner_user,
        owner_token,
    )
    .await;
    assert!(registered.is_some(), "shared stream should register");
}

async fn wait_for_settled<F, Fut>(predicate: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition did not settle within timeout");
}

#[tokio::test]
async fn test_free_shared_join_admission_and_max_limit() {
    let app_state = create_test_provider_app_state();
    let stream_url_b = "http://provider-1.example/live/shared_b.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55140".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url_b, owner_addr, "owner_user", "owner_token").await;

    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_1".to_string();
    user.max_connections = 1;
    user.player_features.free_shared_joins = true;
    user.player_features.max_free_shared_joins = 1;

    let addr_a: SocketAddr = "127.0.0.1:55150".parse().unwrap_or_else(|_| unreachable!());
    let fp_a = create_test_fingerprint(addr_a);
    let channel_a = create_test_live_channel("http://provider-1.example/live/a.ts");
    app_state.active_users.add_connection(&addr_a).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fp_a,
            provider: input.name.clone(),
            stream_channel: &channel_a,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-a"),
        })
        .await
        .expect("stream A should register");

    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);

    let addr_b: SocketAddr = "127.0.0.1:55151".parse().unwrap_or_else(|_| unreachable!());
    let fp_b = create_test_fingerprint(addr_b);
    let is_free_join = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(is_free_join);
    let response = stream_response(
        &fp_b,
        &app_state,
        "session-b",
        None,
        create_test_live_channel(stream_url_b),
        PlaybackSource::Single { input: &input, stream_url: stream_url_b },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    // Counted connections stay 1 (no counted slot consumed, no eviction)
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);

    // Second free join with max_free_shared_joins 1 falls back to exhausted
    let stream_url_c = "http://provider-1.example/live/shared_c.ts";
    let owner_c_addr: SocketAddr = "127.0.0.1:55142".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url_c, owner_c_addr, "owner_user_c", "owner_token_c").await;

    let addr_c: SocketAddr = "127.0.0.1:55152".parse().unwrap_or_else(|_| unreachable!());
    let fp_c = create_test_fingerprint(addr_c);
    let is_free_join_c = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(!is_free_join_c);
    let response_c = stream_response(
        &fp_c,
        &app_state,
        "session-c",
        None,
        create_test_live_channel(stream_url_c),
        PlaybackSource::Single { input: &input, stream_url: stream_url_c },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join_c,
        false,
    )
    .await
    .into_response();
    assert_ne!(response_c.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);
}

#[tokio::test]
async fn test_free_shared_join_feature_off_fallback() {
    let app_state = create_test_provider_app_state();
    let stream_url_b = "http://provider-1.example/live/shared_off.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55143".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url_b, owner_addr, "owner_user", "owner_token").await;

    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_off".to_string();
    user.max_connections = 1;
    user.player_features.free_shared_joins = false;
    user.player_features.max_free_shared_joins = 1;

    let addr_a: SocketAddr = "127.0.0.1:55153".parse().unwrap_or_else(|_| unreachable!());
    let fp_a = create_test_fingerprint(addr_a);
    let channel_a = create_test_live_channel("http://provider-1.example/live/a_off.ts");
    app_state.active_users.add_connection(&addr_a).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1002,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fp_a,
            provider: input.name.clone(),
            stream_channel: &channel_a,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-a-off"),
        })
        .await
        .expect("stream A should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);

    let addr_b: SocketAddr = "127.0.0.1:55154".parse().unwrap_or_else(|_| unreachable!());
    let fp_b = create_test_fingerprint(addr_b);
    let is_free_join = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(!is_free_join);
    let response = stream_response(
        &fp_b,
        &app_state,
        "session-b-off",
        None,
        create_test_live_channel(stream_url_b),
        PlaybackSource::Single { input: &input, stream_url: stream_url_b },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join,
        false,
    )
    .await
    .into_response();
    assert_ne!(response.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 0);
}

#[tokio::test]
async fn test_paid_viewer_leaves_conversion_with_free_slot() {
    let app_state = create_test_provider_app_state();
    let stream_url_b = "http://provider-1.example/live/shared_conv_slot.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55144".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url_b, owner_addr, "owner_user", "owner_token").await;

    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_conv_slot".to_string();
    user.max_connections = 1;
    user.player_features.free_shared_joins = true;
    user.player_features.max_free_shared_joins = 1;

    let addr_a: SocketAddr = "127.0.0.1:55155".parse().unwrap_or_else(|_| unreachable!());
    let fp_a = create_test_fingerprint(addr_a);
    let channel_a = create_test_live_channel("http://provider-1.example/live/a_conv.ts");
    app_state.active_users.add_connection(&addr_a).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1003,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fp_a,
            provider: input.name.clone(),
            stream_channel: &channel_a,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-a-conv"),
        })
        .await
        .expect("stream A should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);

    let addr_b: SocketAddr = "127.0.0.1:55156".parse().unwrap_or_else(|_| unreachable!());
    let fp_b = create_test_fingerprint(addr_b);
    let is_free_join = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(is_free_join);
    let response = stream_response(
        &fp_b,
        &app_state,
        "session-b-conv",
        None,
        create_test_live_channel(stream_url_b),
        PlaybackSource::Single { input: &input, stream_url: stream_url_b },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);

    // User stops watching stream A -> frees a slot
    app_state.active_users.release_connection(&addr_a).await;
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);

    // Paid viewer leaves B -> free joiner converted to counted connection
    app_state.shared_stream_manager.release_connection(&owner_addr, false).await;

    wait_for_settled(|| async {
        app_state.shared_stream_manager.user_free_shared_joins(&user.username).await == 0
            && app_state.active_users.user_connections(&user.username).await == 1
    })
    .await;

    // Free joiner was converted: free_shared_joins_in_use drops to 0, counted connections becomes 1
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 0);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "session-b-conv")
        .await
        .expect("session should still exist");
    assert_eq!(session.permission, UserConnectionPermission::Allowed);
    assert_eq!(session.lifecycle, PlaybackLifecycle::Active);
}

#[tokio::test]
async fn test_paid_viewer_leaves_conversion_without_free_slot() {
    let app_state = create_test_provider_app_state();
    let stream_url_b = "http://provider-1.example/live/shared_conv_noslot.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55145".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url_b, owner_addr, "owner_user", "owner_token").await;

    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_conv_noslot".to_string();
    user.max_connections = 1;
    user.player_features.free_shared_joins = true;
    user.player_features.max_free_shared_joins = 1;

    let addr_a: SocketAddr = "127.0.0.1:55157".parse().unwrap_or_else(|_| unreachable!());
    let fp_a = create_test_fingerprint(addr_a);
    let channel_a = create_test_live_channel("http://provider-1.example/live/a_noslot.ts");
    app_state.active_users.add_connection(&addr_a).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1004,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fp_a,
            provider: input.name.clone(),
            stream_channel: &channel_a,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-a-noslot"),
        })
        .await
        .expect("stream A should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);

    let addr_b: SocketAddr = "127.0.0.1:55158".parse().unwrap_or_else(|_| unreachable!());
    let fp_b = create_test_fingerprint(addr_b);
    let is_free_join = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(is_free_join);
    let response = stream_response(
        &fp_b,
        &app_state,
        "session-b-noslot",
        None,
        create_test_live_channel(stream_url_b),
        PlaybackSource::Single { input: &input, stream_url: stream_url_b },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);

    // User is STILL watching stream A (no slot free)
    // Paid viewer leaves B -> conversion fails and subscriber ended with fallback
    app_state.shared_stream_manager.release_connection(&owner_addr, false).await;

    wait_for_settled(|| async { app_state.shared_stream_manager.user_free_shared_joins(&user.username).await == 0 })
        .await;

    // Free join was removed
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 0);
    // Counted connections remain 1 (from stream A)
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "session-b-noslot")
        .await
        .expect("session exists");
    assert_eq!(session.permission, UserConnectionPermission::Exhausted);
}

#[tokio::test]
async fn test_recording_subscriber_keeps_free_joiner_free() {
    let app_state = create_test_provider_app_state();
    let stream_url_b = "http://provider-1.example/live/shared_rec.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55146".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url_b, owner_addr, "owner_user", "owner_token").await;

    // Add a recording subscriber to stream B
    let rec_addr: SocketAddr = "127.0.0.1:55199".parse().unwrap_or_else(|_| unreachable!());
    let rec_id = SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let rec_sub = SharedStreamManager::subscribe_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url_b,
        &rec_addr,
        rec_id,
        0,
        crate::api::model::ConnectionKind::Normal,
        RECORDING_PROXY_USERNAME,
        "rec_token",
        None,
    )
    .await
    .expect("subscribe recording should not fail")
    .expect("recording subscriber should join");
    drop(rec_sub);

    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_rec".to_string();
    user.max_connections = 1;
    user.player_features.free_shared_joins = true;
    user.player_features.max_free_shared_joins = 1;

    let addr_a: SocketAddr = "127.0.0.1:55159".parse().unwrap_or_else(|_| unreachable!());
    let fp_a = create_test_fingerprint(addr_a);
    let channel_a = create_test_live_channel("http://provider-1.example/live/a_rec.ts");
    app_state.active_users.add_connection(&addr_a).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1005,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fp_a,
            provider: input.name.clone(),
            stream_channel: &channel_a,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-a-rec"),
        })
        .await
        .expect("stream A should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);

    let addr_b: SocketAddr = "127.0.0.1:55160".parse().unwrap_or_else(|_| unreachable!());
    let fp_b = create_test_fingerprint(addr_b);
    let is_free_join = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(is_free_join);
    let response = stream_response(
        &fp_b,
        &app_state,
        "session-b-rec",
        None,
        create_test_live_channel(stream_url_b),
        PlaybackSource::Single { input: &input, stream_url: stream_url_b },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);

    // Paid viewer leaves B -> recording subscriber remains, so free joiner stays free
    app_state.shared_stream_manager.release_connection(&owner_addr, false).await;

    // Free joiner remains free!
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "session-b-rec")
        .await
        .expect("session exists");
    assert_eq!(session.permission, UserConnectionPermission::Allowed);
}

#[tokio::test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn test_paid_viewer_leaves_two_free_joiners_with_free_slots_converts_exactly_one() {
    let app_state = create_test_provider_app_state();
    let stream_url = "http://provider-1.example/live/shared_two_free_with_slots.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55200".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url, owner_addr, "owner_user", "owner_token").await;

    // Set up user B with max_connections = 1 and free_shared_joins = true
    let mut user_b = ProxyUserCredentials::default();
    user_b.username = "viewer_b_slot".to_string();
    user_b.max_connections = 1;
    user_b.player_features.free_shared_joins = true;
    user_b.player_features.max_free_shared_joins = 1;

    let addr_b_other: SocketAddr = "127.0.0.1:55201".parse().unwrap_or_else(|_| unreachable!());
    let fp_b_other = create_test_fingerprint(addr_b_other);
    let channel_b_other = create_test_live_channel("http://provider-1.example/live/b_other.ts");
    app_state.active_users.add_connection(&addr_b_other).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 2001,
            meter_uid: 0,
            username: &user_b.username,
            max_connections: user_b.max_connections,
            soft_connections: user_b.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user_b.priority,
            soft_priority: user_b.soft_priority,
            fingerprint: &fp_b_other,
            provider: input.name.clone(),
            stream_channel: &channel_b_other,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-b-other"),
        })
        .await
        .expect("stream B other should register");

    let admission_b = app_state
        .active_users
        .connection_admission(&user_b.username, user_b.max_connections, user_b.soft_connections)
        .await;
    assert_eq!(admission_b.permission(), UserConnectionPermission::Exhausted);

    let addr_b_shared: SocketAddr = "127.0.0.1:55202".parse().unwrap_or_else(|_| unreachable!());
    let fp_b_shared = create_test_fingerprint(addr_b_shared);
    let is_free_join_b = evaluate_free_join(&app_state, &user_b, admission_b.permission(), true).await;
    assert!(is_free_join_b);
    let response_b = stream_response(
        &fp_b_shared,
        &app_state,
        "session-b-shared",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &user_b,
        admission_b.permission(),
        admission_b.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join_b,
        false,
    )
    .await
    .into_response();
    assert_eq!(response_b.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user_b.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user_b.username).await, 1);

    // Set up user C with max_connections = 1 and free_shared_joins = true
    let mut user_c = ProxyUserCredentials::default();
    user_c.username = "viewer_c_slot".to_string();
    user_c.max_connections = 1;
    user_c.player_features.free_shared_joins = true;
    user_c.player_features.max_free_shared_joins = 1;

    let addr_c_other: SocketAddr = "127.0.0.1:55203".parse().unwrap_or_else(|_| unreachable!());
    let fp_c_other = create_test_fingerprint(addr_c_other);
    let channel_c_other = create_test_live_channel("http://provider-1.example/live/c_other.ts");
    app_state.active_users.add_connection(&addr_c_other).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 2002,
            meter_uid: 0,
            username: &user_c.username,
            max_connections: user_c.max_connections,
            soft_connections: user_c.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user_c.priority,
            soft_priority: user_c.soft_priority,
            fingerprint: &fp_c_other,
            provider: input.name.clone(),
            stream_channel: &channel_c_other,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-c-other"),
        })
        .await
        .expect("stream C other should register");

    let admission_c = app_state
        .active_users
        .connection_admission(&user_c.username, user_c.max_connections, user_c.soft_connections)
        .await;
    assert_eq!(admission_c.permission(), UserConnectionPermission::Exhausted);

    let addr_c_shared: SocketAddr = "127.0.0.1:55204".parse().unwrap_or_else(|_| unreachable!());
    let fp_c_shared = create_test_fingerprint(addr_c_shared);
    let is_free_join_c = evaluate_free_join(&app_state, &user_c, admission_c.permission(), true).await;
    assert!(is_free_join_c);
    let response_c = stream_response(
        &fp_c_shared,
        &app_state,
        "session-c-shared",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &user_c,
        admission_c.permission(),
        admission_c.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join_c,
        false,
    )
    .await
    .into_response();
    assert_eq!(response_c.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user_c.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user_c.username).await, 1);

    assert_eq!(app_state.shared_stream_manager.subscriber_count(stream_url).await, 3);

    // Both users stop watching their other streams -> each now has a free slot
    app_state.active_users.release_connection(&addr_b_other).await;
    app_state.active_users.release_connection(&addr_c_other).await;
    assert_eq!(app_state.active_users.user_connections(&user_b.username).await, 0);
    assert_eq!(app_state.active_users.user_connections(&user_c.username).await, 0);

    // Paid viewer leaves: settlement converts exactly ONE free joiner into a counted connection,
    // and the other joiner remains free because the stream now has a paid holder again.
    app_state.shared_stream_manager.release_connection(&owner_addr, false).await;

    wait_for_settled(|| async {
        let b_conn = app_state.active_users.user_connections(&user_b.username).await;
        let c_conn = app_state.active_users.user_connections(&user_c.username).await;
        b_conn + c_conn == 1
    })
    .await;

    let b_free = app_state.shared_stream_manager.user_free_shared_joins(&user_b.username).await;
    let c_free = app_state.shared_stream_manager.user_free_shared_joins(&user_c.username).await;
    let b_conn = app_state.active_users.user_connections(&user_b.username).await;
    let c_conn = app_state.active_users.user_connections(&user_c.username).await;

    assert_eq!(b_conn + c_conn, 1, "exactly one joiner converted to counted connection");
    assert_eq!(b_free + c_free, 1, "the other joiner stays free");
    assert!(
        (b_conn == 1 && b_free == 0 && c_conn == 0 && c_free == 1)
            || (c_conn == 1 && c_free == 0 && b_conn == 0 && b_free == 1),
        "converted joiner is counted and not free, other joiner is free and not counted"
    );
    assert_eq!(app_state.shared_stream_manager.subscriber_count(stream_url).await, 2, "both joiners remain subscribed");
    assert!(app_state.shared_stream_manager.get_shared_state(stream_url).await.is_some());
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let (converted_user, converted_session, free_user, free_session) = if b_conn == 1 {
        (&user_b.username, "session-b-shared", &user_c.username, "session-c-shared")
    } else {
        (&user_c.username, "session-c-shared", &user_b.username, "session-b-shared")
    };
    let session_conv = app_state
        .active_users
        .get_and_update_user_session(converted_user, converted_session)
        .await
        .expect("converted session exists");
    assert_eq!(session_conv.permission, UserConnectionPermission::Allowed);
    assert_eq!(session_conv.lifecycle, PlaybackLifecycle::Active);

    let session_free =
        app_state.active_users.get_and_update_user_session(free_user, free_session).await.expect("free session exists");
    assert_eq!(session_free.lifecycle, PlaybackLifecycle::Prepared);
}

#[tokio::test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn test_paid_viewer_leaves_two_free_joiners_without_free_slots_releases_once_and_unregisters() {
    let app_state = create_test_provider_app_state();
    let stream_url = "http://provider-1.example/live/shared_two_free_noslot.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55210".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url, owner_addr, "owner_user", "owner_token").await;

    // Set up user B with max_connections = 1
    let mut user_b = ProxyUserCredentials::default();
    user_b.username = "viewer_b_noslot".to_string();
    user_b.max_connections = 1;
    user_b.player_features.free_shared_joins = true;
    user_b.player_features.max_free_shared_joins = 1;

    let addr_b_other: SocketAddr = "127.0.0.1:55211".parse().unwrap_or_else(|_| unreachable!());
    let fp_b_other = create_test_fingerprint(addr_b_other);
    let channel_b_other = create_test_live_channel("http://provider-1.example/live/b_other_noslot.ts");
    app_state.active_users.add_connection(&addr_b_other).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 2003,
            meter_uid: 0,
            username: &user_b.username,
            max_connections: user_b.max_connections,
            soft_connections: user_b.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user_b.priority,
            soft_priority: user_b.soft_priority,
            fingerprint: &fp_b_other,
            provider: input.name.clone(),
            stream_channel: &channel_b_other,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-b-other-noslot"),
        })
        .await
        .expect("stream B other should register");

    let admission_b = app_state
        .active_users
        .connection_admission(&user_b.username, user_b.max_connections, user_b.soft_connections)
        .await;
    assert_eq!(admission_b.permission(), UserConnectionPermission::Exhausted);

    let addr_b_shared: SocketAddr = "127.0.0.1:55212".parse().unwrap_or_else(|_| unreachable!());
    let fp_b_shared = create_test_fingerprint(addr_b_shared);
    let is_free_join_b = evaluate_free_join(&app_state, &user_b, admission_b.permission(), true).await;
    assert!(is_free_join_b);
    let response_b = stream_response(
        &fp_b_shared,
        &app_state,
        "session-b-shared-noslot",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &user_b,
        admission_b.permission(),
        admission_b.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join_b,
        false,
    )
    .await
    .into_response();
    assert_eq!(response_b.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user_b.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user_b.username).await, 1);

    // Set up user C with max_connections = 1
    let mut user_c = ProxyUserCredentials::default();
    user_c.username = "viewer_c_noslot".to_string();
    user_c.max_connections = 1;
    user_c.player_features.free_shared_joins = true;
    user_c.player_features.max_free_shared_joins = 1;

    let addr_c_other: SocketAddr = "127.0.0.1:55213".parse().unwrap_or_else(|_| unreachable!());
    let fp_c_other = create_test_fingerprint(addr_c_other);
    let channel_c_other = create_test_live_channel("http://provider-1.example/live/c_other_noslot.ts");
    app_state.active_users.add_connection(&addr_c_other).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 2004,
            meter_uid: 0,
            username: &user_c.username,
            max_connections: user_c.max_connections,
            soft_connections: user_c.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user_c.priority,
            soft_priority: user_c.soft_priority,
            fingerprint: &fp_c_other,
            provider: input.name.clone(),
            stream_channel: &channel_c_other,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-c-other-noslot"),
        })
        .await
        .expect("stream C other should register");

    let admission_c = app_state
        .active_users
        .connection_admission(&user_c.username, user_c.max_connections, user_c.soft_connections)
        .await;
    assert_eq!(admission_c.permission(), UserConnectionPermission::Exhausted);

    let addr_c_shared: SocketAddr = "127.0.0.1:55214".parse().unwrap_or_else(|_| unreachable!());
    let fp_c_shared = create_test_fingerprint(addr_c_shared);
    let is_free_join_c = evaluate_free_join(&app_state, &user_c, admission_c.permission(), true).await;
    assert!(is_free_join_c);
    let response_c = stream_response(
        &fp_c_shared,
        &app_state,
        "session-c-shared-noslot",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &user_c,
        admission_c.permission(),
        admission_c.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join_c,
        false,
    )
    .await
    .into_response();
    assert_eq!(response_c.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user_c.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user_c.username).await, 1);

    // Initial state: 3 subscribers, 1 provider connection
    assert_eq!(app_state.shared_stream_manager.subscriber_count(stream_url).await, 3);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    // Neither user has a free slot (both still watching other stream)
    // Paid viewer leaves: both free joiners are ended, provider connection released once, stream unregistered
    app_state.shared_stream_manager.release_connection(&owner_addr, false).await;

    wait_for_settled(|| async {
        app_state.shared_stream_manager.user_free_shared_joins(&user_b.username).await == 0
            && app_state.shared_stream_manager.user_free_shared_joins(&user_c.username).await == 0
    })
    .await;

    // Both free joiners ended
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user_b.username).await, 0);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user_c.username).await, 0);
    assert_eq!(app_state.active_users.user_connections(&user_b.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user_c.username).await, 1);

    let session_b = app_state
        .active_users
        .get_and_update_user_session(&user_b.username, "session-b-shared-noslot")
        .await
        .expect("session B exists");
    assert_eq!(session_b.permission, UserConnectionPermission::Exhausted);

    let session_c = app_state
        .active_users
        .get_and_update_user_session(&user_c.username, "session-c-shared-noslot")
        .await
        .expect("session C exists");
    assert_eq!(session_c.permission, UserConnectionPermission::Exhausted);

    // Provider connection released exactly once (no double release)
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);

    // Shared stream is unregistered
    assert!(app_state.shared_stream_manager.get_shared_state(stream_url).await.is_none());
    assert_eq!(app_state.shared_stream_manager.subscriber_count(stream_url).await, 0);
    assert_eq!(app_state.shared_stream_manager.resource_counts().await, (0, 0));
}

#[tokio::test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn test_departed_free_joiner_before_settlement_not_admitted() {
    let app_state = create_test_provider_app_state();
    let stream_url = "http://provider-1.example/live/shared_departed_joiner.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
    register_test_shared_stream(&app_state, stream_url, owner_addr, "owner_user", "owner_token").await;

    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_departed".to_string();
    user.max_connections = 1;
    user.player_features.free_shared_joins = true;
    user.player_features.max_free_shared_joins = 1;

    let addr_other: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
    let fp_other = create_test_fingerprint(addr_other);
    let channel_other = create_test_live_channel("http://provider-1.example/live/other_departed.ts");
    app_state.active_users.add_connection(&addr_other).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 2005,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fp_other,
            provider: input.name.clone(),
            stream_channel: &channel_other,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("session-departed-other"),
        })
        .await
        .expect("stream other should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);

    let addr_shared: SocketAddr = "127.0.0.1:55222".parse().unwrap_or_else(|_| unreachable!());
    let fp_shared = create_test_fingerprint(addr_shared);
    let is_free_join = evaluate_free_join(&app_state, &user, admission.permission(), true).await;
    assert!(is_free_join);
    let response = stream_response(
        &fp_shared,
        &app_state,
        "session-departed-shared",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        is_free_join,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 1);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);

    // User stops watching the other stream -> frees a slot
    app_state.active_users.release_connection(&addr_other).await;
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);

    // Hold the playback transition guard for the free joiner's session so settlement cannot
    // run conversion until we release it.
    let transition_guard =
        app_state.active_users.acquire_playback_transition(&user.username, "session-departed-shared").await;

    // Paid viewer leaves in the background: remove_subscriber removes the owner and plans
    // FreeJoinConversion for the joiner, but settle_free_joiners blocks on transition_guard.
    let release_owner = tokio::spawn({
        let ssm = Arc::clone(&app_state.shared_stream_manager);
        async move {
            ssm.release_connection(&owner_addr, false).await;
        }
    });

    // Wait until owner is removed by remove_subscriber (subscriber count drops from 2 to 1)
    tokio::time::timeout(Duration::from_secs(2), async {
        while app_state.shared_stream_manager.subscriber_count(stream_url).await != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner should be removed and settlement pending on transition guard");

    tokio::task::yield_now().await;

    // The free joiner leaves before settlement runs for them
    app_state.shared_stream_manager.release_connection(&addr_shared, false).await;

    // Release the transition guard so settlement proceeds
    drop(transition_guard);

    release_owner.await.expect("release_owner task should complete cleanly");

    // The departed free joiner is not admitted: no counted connection appears
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
    assert_eq!(app_state.shared_stream_manager.user_free_shared_joins(&user.username).await, 0);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    assert!(app_state.shared_stream_manager.get_shared_state(stream_url).await.is_none());
    assert_eq!(app_state.shared_stream_manager.subscriber_count(stream_url).await, 0);

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "session-departed-shared")
        .await
        .expect("session exists");
    // Free join was never admitted, so session lifecycle was not activated by conversion
    assert_eq!(session.lifecycle, PlaybackLifecycle::Prepared);
}
