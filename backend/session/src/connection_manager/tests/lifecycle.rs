use super::*;

#[tokio::test]
async fn close_connection_with_reason_returns_false_for_dropped_oneshot_receiver() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
    let close_rx = manager.register_close_socket(addr);
    drop(close_rx);

    assert!(
        !manager.close_connection_with_reason(&addr, DisconnectReason::ClientKicked),
        "must return false when target oneshot receiver was dropped"
    );
}

#[tokio::test]
async fn release_connection_as_kicked_cleans_provider_when_unreceived() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:2236".parse().unwrap_or_else(|_| unreachable!());
    let input_name = "provider_1".intern();
    let handle = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            0,
            crate::ConnectionKind::Normal,
            Some("test-owner"),
        )
        .expect("acquire provider");
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);

    let closed = manager.release_connection_as_kicked(&addr).await;
    assert!(!closed, "should return false when no receiver is subscribed");
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 0);

    drop(handle);
}

#[tokio::test]
async fn release_connection_as_kicked_does_not_release_provider_prematurely_on_repeated_kick() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:2237".parse().unwrap_or_else(|_| unreachable!());
    let input_name = "provider_1".intern();
    let handle = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            0,
            crate::ConnectionKind::Normal,
            Some("test-owner"),
        )
        .expect("acquire provider");
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);

    let mut close_rx = manager.register_close_socket(addr);

    // First kick sends close signal to socket and transitions to Closing
    let closed_first = manager.release_connection_as_kicked(&addr).await;
    assert!(closed_first, "first kick must report success when socket is registered");
    assert_eq!(
        manager.provider_manager.get_provider_connections_count(),
        1,
        "provider must remain allocated after first kick while socket is in Closing state"
    );

    // Second kick while socket is still Closing must not prematurely release provider
    let closed_second = manager.release_connection_as_kicked(&addr).await;
    assert!(closed_second, "second kick during Closing state must report success");
    assert_eq!(
        manager.provider_manager.get_provider_connections_count(),
        1,
        "provider must not be released prematurely by second kick before socket finishes"
    );

    // Now simulate the socket processing the signal and finishing its cleanup
    let reason = close_rx.try_recv().expect("close signal must be pending in oneshot");
    assert_eq!(reason, DisconnectReason::ClientKicked);
    manager.release_provider_deferred(&addr).await;
    manager.unregister_close_socket(&addr);
    assert_eq!(
        manager.provider_manager.get_provider_connections_count(),
        0,
        "provider must be released after socket finishes and unregisters"
    );

    drop(handle);
}

#[tokio::test]
async fn release_connection_as_kicked_sends_kick_close_signal() {
    let manager = create_test_connection_manager();
    let mut rx = manager.get_close_connection_channel();
    let addr: SocketAddr = "127.0.0.1:2234".parse().unwrap_or_else(|_| unreachable!());

    manager.release_connection_as_kicked(&addr).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv()).await.ok().and_then(Result::ok),
        Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientKicked))
    );
}

#[tokio::test]
async fn release_connection_as_kicked_is_idempotent() {
    let manager = create_test_connection_manager();
    let mut rx = manager.get_close_connection_channel();
    let addr: SocketAddr = "127.0.0.1:2235".parse().unwrap_or_else(|_| unreachable!());

    manager.add_connection(&addr).await;
    manager.release_connection_as_kicked(&addr).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv()).await.ok().and_then(Result::ok),
        Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientKicked))
    );

    manager.release_connection_as_kicked(&addr).await;
}

#[tokio::test]
async fn low_priority_preempted_cleanup_blocks_same_user_stream_reentry() {
    let manager = create_test_connection_manager();
    let user = create_test_proxy_user("preempted-user");
    let addr: SocketAddr = "127.0.0.1:6234".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 409,
        title: "channel-409".intern(),
        ..make_stream_info("provider_1", "channel-409").channel
    };

    manager.add_connection(&addr).await;
    manager
        .user_manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preempted",
            virtual_id: channel.virtual_id,
            provider: "provider_1",
            stream_url: "http://provider-1.example/live/409.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    manager
        .user_manager
        .update_connection(ActiveUserConnectionParams {
            uid: 9001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::ConnectionKind::Normal,
            priority: 9,
            soft_priority: 9,
            fingerprint: &fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("tok-preempted"),
        })
        .await;

    let deps = CleanupWorkerDeps {
        user_manager: Arc::clone(&manager.user_manager),
        provider_manager: Arc::clone(&manager.provider_manager),
        shared_stream_manager: Arc::clone(&manager.shared_stream_manager),
        event_manager: Arc::clone(&manager.event_manager),
        capacity_notify: Arc::clone(&manager.capacity_notify),
        history_writer: Arc::clone(&manager.history_writer),
    };

    handle_update_detail_and_release_provider(&deps, addr, CustomVideoStreamType::LowPriorityPreempted, None, None)
        .await;

    assert!(
        manager
            .user_manager
            .is_user_blocked_for_stream(&user.username, shared::model::VirtualId::new(channel.virtual_id))
            .await,
        "preempted playback should be blocked briefly to prevent immediate reconnect ping-pong"
    );
}

#[tokio::test]
async fn low_priority_preempted_cleanup_blocks_same_user_hls_reentry() {
    let manager = create_test_connection_manager();
    let user = create_test_proxy_user("preempted-hls-user");
    let addr: SocketAddr = "127.0.0.1:6235".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let mut channel = make_stream_info("provider_1", "channel-410").channel;
    channel.virtual_id = 410;
    channel.item_type = PlaylistItemType::LiveHls;
    channel.title = "channel-410.m3u8".intern();
    channel.url = "http://provider-1.example/live/410.m3u8".intern();

    manager.add_connection(&addr).await;
    manager
        .user_manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preempted-hls",
            virtual_id: channel.virtual_id,
            provider: "provider_1",
            stream_url: "http://provider-1.example/live/410.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .user_manager
        .update_connection(ActiveUserConnectionParams {
            uid: 9002,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::ConnectionKind::Normal,
            priority: 9,
            soft_priority: 9,
            fingerprint: &fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("tok-preempted-hls"),
        })
        .await;

    let deps = CleanupWorkerDeps {
        user_manager: Arc::clone(&manager.user_manager),
        provider_manager: Arc::clone(&manager.provider_manager),
        shared_stream_manager: Arc::clone(&manager.shared_stream_manager),
        event_manager: Arc::clone(&manager.event_manager),
        capacity_notify: Arc::clone(&manager.capacity_notify),
        history_writer: Arc::clone(&manager.history_writer),
    };

    handle_update_detail_and_release_provider(&deps, addr, CustomVideoStreamType::LowPriorityPreempted, None, None)
        .await;

    assert!(
        manager
            .user_manager
            .is_user_blocked_for_stream(&user.username, shared::model::VirtualId::new(channel.virtual_id))
            .await,
        "preempted HLS playback should be blocked briefly to prevent immediate reconnect ping-pong"
    );
}

#[tokio::test]
async fn deferred_cleanup_executes_in_background_worker() {
    let conn_manager = create_test_connection_manager();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let ran_clone = Arc::clone(&ran);
    let notify_clone = Arc::clone(&notify);

    conn_manager.send_cleanup(CleanupEvent::Defer(Box::pin(async move {
        ran_clone.store(true, Ordering::SeqCst);
        notify_clone.notify_one();
    })));

    tokio::time::timeout(Duration::from_secs(1), notify.notified()).await.expect("deferred task must execute");
    assert!(ran.load(Ordering::SeqCst));
}

/// Dropping a just-registered request without handing it to a body must roll the
/// claim back, so a cancellation between registration and body construction cannot
/// leak a user claim.
#[tokio::test]
async fn registration_rollback_releases_claim_when_body_never_constructed() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:56234".parse().unwrap();
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 410,
        title: "channel-410".intern(),
        ..make_stream_info("provider_1", "channel-410").channel
    };

    manager.add_connection(&addr).await;
    let registered = manager
        .update_connection_with_uid(
            ConnectionParams {
                meter_uid: 0,
                username: "rollback-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: None,
            },
            ConnectionHistoryMode::EmitConnect,
            1,
            None,
        )
        .await;
    assert!(registered.display_stream.is_some(), "registration must succeed");

    // Drop without committing: the rollback owner releases the claim.
    drop(registered);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager.user_manager.active_streams().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the rollback must release the claim");
}

/// The body cleanup transferred via `into_body_cleanup` releases the claim exactly
/// once with the real outcome, even under cleanup-queue pressure.
#[tokio::test]
async fn body_cleanup_finish_releases_claim_exactly_once() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:56236".parse().unwrap();
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 412,
        title: "channel-412".intern(),
        ..make_stream_info("provider_1", "channel-412").channel
    };

    manager.add_connection(&addr).await;
    let mut registered = manager
        .update_connection_with_uid(
            ConnectionParams {
                meter_uid: 0,
                username: "body-cleanup-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: None,
            },
            ConnectionHistoryMode::EmitConnect,
            1,
            None,
        )
        .await;
    assert!(registered.display_stream.is_some(), "registration must succeed");

    let mut cleanup = registered.into_body_cleanup().expect("body cleanup present");
    cleanup.finish(None, PROVIDER_END_CLOSED, 0, None, None);
    cleanup.finish(None, PROVIDER_END_CLOSED, 0, None, None);

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager.user_manager.active_streams().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the body cleanup must release the claim exactly once");
}

#[tokio::test]
async fn control_cleanup_lane_precedes_a_ready_body_cleanup_backlog() {
    let manager = create_test_connection_manager();
    let release_blocker = Arc::new(Notify::new());
    let (blocker_started_tx, blocker_started_rx) = tokio::sync::oneshot::channel();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    manager.send_cleanup(CleanupEvent::Defer(Box::pin({
        let release_blocker = Arc::clone(&release_blocker);
        let order = Arc::clone(&order);
        async move {
            let _ = blocker_started_tx.send(());
            release_blocker.notified().await;
            order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push('B');
        }
    })));
    blocker_started_rx.await.expect("cleanup blocker must start");

    manager.send_cleanup(CleanupEvent::Defer(Box::pin({
        let order = Arc::clone(&order);
        async move { order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push('N') }
    })));
    manager
        .control_cleanup_tx()
        .send(CleanupEvent::Defer(Box::pin({
            let order = Arc::clone(&order);
            async move { order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push('C') }
        })))
        .await
        .expect("control cleanup receiver must be open");

    release_blocker.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len() == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both queued cleanup events must run");

    assert_eq!(
        *order.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!['B', 'C', 'N'],
        "a ready control cleanup must not be starved by the body cleanup backlog"
    );
}

/// A cleanup whose user row is already gone must still finish the provider request,
/// because the provider identity travels with the cleanup owner independently.
#[tokio::test]
async fn cleanup_finishes_provider_request_when_user_row_is_gone() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:56240".parse().unwrap();
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 413,
        title: "channel-413".intern(),
        ..make_stream_info("provider_1", "channel-413").channel
    };
    let input_name = "provider_1".intern();
    let owner = "session-owner-g2";

    // Acquire a provider slot with an identified lease.
    let handle = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &addr,
            false,
            0,
            crate::ConnectionKind::Normal,
            Some(owner),
        )
        .expect("acquire provider slot");
    let request_id = handle.playback_request_id.expect("identified request id");
    assert!(manager.provider_manager.provider_lease_usage(&input_name).total() > 0);

    // Register a user claim carrying the same owner and provider request id.
    manager.add_connection(&addr).await;
    let mut registered = manager
        .update_connection_with_uid(
            ConnectionParams {
                meter_uid: 0,
                username: "g2-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some(owner),
            },
            ConnectionHistoryMode::EmitConnect,
            1,
            Some(request_id),
        )
        .await;
    assert!(registered.display_stream.is_some());

    let cleanup = registered.into_body_cleanup().expect("cleanup present");

    // Remove the user row first, so the cleanup worker hits the NotFound path.
    let _ = manager.user_manager.release_stream_request_by_uid(&addr, 1).await;

    drop(cleanup);

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager.provider_manager.provider_lease_usage(&input_name).total() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request must finish even without a user row");

    manager.provider_manager.release_handle(&handle);
}

/// Shutdown releases every active stream so no claim or provider slot survives.
#[tokio::test]
async fn shutdown_releases_active_streams() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:56235".parse().unwrap();
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 411,
        title: "channel-411".intern(),
        ..make_stream_info("provider_1", "channel-411").channel
    };

    manager.add_connection(&addr).await;
    let stream_info = manager
        .update_connection(ConnectionParams {
            meter_uid: 0,
            username: "shutdown-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: None,
        })
        .await;
    assert!(stream_info.is_some(), "registration must succeed");
    assert!(!manager.user_manager.active_streams().await.is_empty());

    manager.shutdown().await;
    assert!(manager.user_manager.active_streams().await.is_empty(), "shutdown must release active streams");
}

#[tokio::test]
async fn shutdown_parallel_requests_on_distinct_sockets_drains_all_claims() {
    let manager = create_test_connection_manager();
    let first_addr: SocketAddr = "127.0.0.1:56241".parse().unwrap();
    let second_addr: SocketAddr = "127.0.0.1:56242".parse().unwrap();
    let first_fingerprint =
        tuliprox_core::model::Fingerprint::new("range-first".to_string(), "127.0.0.1".to_string(), first_addr);
    let second_fingerprint =
        tuliprox_core::model::Fingerprint::new("range-second".to_string(), "127.0.0.1".to_string(), second_addr);
    let mut channel = make_stream_info("provider_1", "parallel-range").channel;
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    let owner = "shutdown-parallel-range";
    let input_name = "provider_1".intern();
    let handle = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(
            &input_name,
            &first_addr,
            false,
            0,
            crate::ConnectionKind::Normal,
            Some(owner),
        )
        .expect("provider allocation");
    let request_id = handle.playback_request_id.expect("provider request id");

    manager.add_connection(&first_addr).await;
    manager.add_connection(&second_addr).await;
    let mut first = manager
        .update_connection_with_uid(
            ConnectionParams {
                meter_uid: 0,
                username: "shutdown-range-user",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &first_fingerprint,
                provider: Arc::clone(&input_name),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some(owner),
            },
            ConnectionHistoryMode::EmitConnect,
            1,
            Some(request_id),
        )
        .await;
    let mut second = manager
        .update_connection_with_uid(
            ConnectionParams {
                meter_uid: 0,
                username: "shutdown-range-user",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &second_fingerprint,
                provider: Arc::clone(&input_name),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some(owner),
            },
            ConnectionHistoryMode::EmitConnect,
            2,
            None,
        )
        .await;
    assert!(first.display_stream.is_some());
    assert!(second.display_stream.is_some());
    assert_eq!(manager.user_manager.active_streams().await.len(), 1);
    assert_eq!(manager.user_manager.playback_resource_counts().await.0, 2);
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);

    let first_cleanup = first.into_body_cleanup().expect("first cleanup");
    let second_cleanup = second.into_body_cleanup().expect("second cleanup");
    manager.shutdown().await;

    assert_eq!(manager.user_manager.playback_resource_counts().await, (0, 0));
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 0);
    assert_eq!(manager.provider_manager.provider_lease_usage(&input_name).total(), 0);

    drop(first_cleanup);
    drop(second_cleanup);
    drop(handle);
}

#[tokio::test]
async fn shutdown_with_all_cleanup_permits_held_completes() {
    let manager = create_test_connection_manager();
    let mut permits = Vec::with_capacity(CLEANUP_QUEUE_CAPACITY);
    for _ in 0..CLEANUP_QUEUE_CAPACITY {
        permits.push(manager.cleanup_tx().reserve_owned().await.expect("cleanup receiver open"));
    }
    assert_eq!(manager.cleanup_tx().capacity(), 0);

    tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .expect("shutdown must not require cleanup queue capacity");
    drop(permits);
}
