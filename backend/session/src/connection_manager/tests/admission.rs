use super::*;

/// A saturated cleanup queue must reject registration with an admission-timeout reason
/// (rather than hanging), so the caller can surface a defined non-success response.
#[tokio::test(start_paused = true)]
async fn cleanup_admission_timeout_rejects_registration_with_reason() {
    let manager = create_test_connection_manager();
    // Stall the cleanup worker on a never-resolving defer, then fill the channel so
    // `reserve_owned()` blocks and the bounded admission deadline fires.
    let pending = || CleanupEvent::Defer(Box::pin(std::future::pending::<()>()));
    manager.send_cleanup(pending());
    for _ in 0..CLEANUP_QUEUE_CAPACITY {
        manager.send_cleanup(pending());
    }

    let addr: SocketAddr = "127.0.0.1:56242".parse().unwrap();
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 414,
        title: "channel-414".intern(),
        ..make_stream_info("provider_1", "channel-414").channel
    };

    manager.add_connection(&addr).await;
    let manager_for_task = Arc::clone(&manager);
    let registration = tokio::spawn(async move {
        manager_for_task
            .update_connection_with_uid(
                ConnectionParams {
                    meter_uid: 0,
                    username: "admission-timeout-user",
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
            .await
    });

    // Let the task reach the blocked cleanup reservation, then fire the deadline.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::time::advance(CLEANUP_ADMISSION_TIMEOUT + Duration::from_secs(1)).await;
    let registered = registration.await.expect("registration task must not panic");

    assert!(registered.display_stream.is_none(), "saturated cleanup admission must be rejected");
    assert_eq!(registered.rejection_reason(), Some(ConnectionRejectionReason::CleanupAdmissionTimeout));
}

/// A joined shared body already holds the terminal cleanup permit. Registration must
/// reuse that ownership instead of waiting for a second permit from the same full queue.
#[tokio::test]
async fn shared_admission_does_not_wait_for_second_permit_it_prevents() {
    let manager = create_test_connection_manager();
    let cleanup_tx = manager.cleanup_tx();
    let available = cleanup_tx.capacity();
    let mut permits = Vec::with_capacity(available);
    for _ in 0..available {
        permits.push(cleanup_tx.clone().try_reserve_owned().expect("cleanup permit"));
    }

    let addr: SocketAddr = "127.0.0.1:56243".parse().expect("test socket");
    let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
    let channel = StreamChannel {
        virtual_id: 415,
        title: "channel-415".intern(),
        shared: true,
        shared_joined_existing: Some(true),
        ..make_stream_info("provider_1", "channel-415").channel
    };

    manager.add_connection(&addr).await;
    let mut registered = tokio::time::timeout(
        Duration::from_millis(100),
        manager.update_connection_with_uid_using_shared_cleanup(
            ConnectionParams {
                meter_uid: 0,
                username: "shared-admission-user",
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
            SharedCleanupCapability::new(tuliprox_core::model::SharedSubscriberId::from_stream_uid(1)),
            None,
        ),
    )
    .await
    .expect("externally owned cleanup must not reserve another permit");

    assert!(registered.display_stream.is_some(), "shared registration must succeed");
    assert!(registered.into_body_cleanup().is_some(), "the body still tracks its terminal outcome");
    let _ = manager.user_manager.release_stream_request_by_uid(&addr, 1).await;
    drop(permits);
}

#[tokio::test]
async fn new_with_capacity_sizes_the_cleanup_queue() {
    let app_cfg = create_test_app_config();
    let event_manager = Arc::new(EventManager::new());
    let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
    provider_manager.set_shared_stream_manager(&shared_manager);
    let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let user_manager = Arc::new(ActiveUserManager::new(&config, &geo_ip, &event_manager));

    let manager = Arc::new(ConnectionManager::new_with_capacity(
        &user_manager,
        &provider_manager,
        &shared_manager,
        &event_manager,
        None,
        2,
    ));
    assert_eq!(manager.cleanup_tx().capacity(), 2);

    // A capacity of zero is clamped to one so the cleanup queue is always usable.
    let manager = Arc::new(ConnectionManager::new_with_capacity(
        &user_manager,
        &provider_manager,
        &shared_manager,
        &event_manager,
        None,
        0,
    ));
    assert_eq!(manager.cleanup_tx().capacity(), 1);
}
