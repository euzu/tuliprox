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

#[expect(clippy::too_many_arguments, reason = "Test fixture seeds user session with playback id.")]
async fn seed_playback_session(
    manager: &ConnectionManager,
    username: &str,
    max_connections: u32,
    session_token: &str,
    virtual_id: u32,
    stream_url: &str,
    addr: &SocketAddr,
    socket_bound: bool,
    playback_id: Option<&str>,
) -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.max_connections = max_connections;
    manager
        .user_manager
        .create_user_session_with_playback_id(
            CreateUserSessionParams {
                user: &user,
                session_token,
                virtual_id,
                provider: "provider_1",
                stream_url,
                addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::ConnectionKind::Normal),
                socket_bound,
            },
            playback_id,
        )
        .await;
    user
}

fn create_test_connection_manager_with_max_provider(
    max_provider_connections: u16,
) -> (Arc<AppConfig>, Arc<ConnectionManager>) {
    let base_cfg = create_test_app_config();
    let app_cfg = Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config {
            user_access_control: true,
            ..(*base_cfg.config.load_full()).clone()
        })),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig {
            inputs: vec![Arc::new(ConfigInput {
                id: 1,
                name: "provider_1".intern(),
                input_type: InputType::Xtream,
                url: "http://provider-1.example".to_string(),
                enabled: true,
                max_connections: max_provider_connections,
                ..ConfigInput::default()
            })],
            ..SourcesConfig::default()
        })),
        ..base_cfg
    });
    let event_manager = Arc::new(EventManager::new());
    let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
    provider_manager.set_shared_stream_manager(&shared_manager);
    let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let user_manager = Arc::new(ActiveUserManager::new(&config, &geo_ip, &event_manager));
    user_manager.set_shared_stream_manager(&shared_manager);
    shared_manager.set_user_manager(&user_manager);
    let manager =
        Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, &event_manager, None));
    (app_cfg, manager)
}

#[expect(clippy::too_many_arguments, reason = "Test fixture registers active stream row with optional shared cleanup.")]
async fn register_test_stream(
    manager: &ConnectionManager,
    username: &str,
    max_connections: u32,
    addr: &SocketAddr,
    channel: &StreamChannel,
    session_token: &str,
    shared_cap: Option<SharedCleanupCapability>,
    explicit_uid: Option<u32>,
) {
    let fp = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), *addr);
    manager.add_connection(addr).await;
    let params = ConnectionParams {
        meter_uid: 0,
        username,
        max_connections,
        soft_connections: 0,
        connection_kind: crate::ConnectionKind::Normal,
        priority: 0,
        soft_priority: 0,
        fingerprint: &fp,
        provider: "provider_1".intern(),
        stream_channel: channel,
        user_agent: std::borrow::Cow::Borrowed("player/1.0"),
        session_token: Some(session_token),
    };
    let mut reg = if let Some(cap) = shared_cap {
        manager
            .update_connection_with_uid_using_shared_cleanup(params, ConnectionHistoryMode::EmitConnect, cap, None)
            .await
    } else {
        manager
            .update_connection_with_uid_and_session_registration(
                params,
                ConnectionHistoryMode::EmitConnect,
                explicit_uid.unwrap_or(1),
                None,
                None,
            )
            .await
    };
    assert!(reg.display_stream.is_some());
    reg.commit();
}

#[tokio::test]
async fn replace_playback_id_replaces_existing_user_stream_and_frees_provider_slot() {
    let manager = create_test_connection_manager();
    let input = "provider_1".intern();
    let addr1: SocketAddr = "127.0.0.1:56301".parse().expect("addr1");
    let addr2: SocketAddr = "127.0.0.1:56302".parse().expect("addr2");
    let channel = StreamChannel {
        virtual_id: 501,
        title: "channel-501".intern(),
        ..make_stream_info("provider_1", "channel-501").channel
    };

    let mut events = manager.event_manager.get_event_channel();
    let close_rx = manager.register_close_socket(addr1);
    let tok1 = "tok-pb-1";
    let user = seed_playback_session(
        &manager,
        "playback-user",
        1,
        tok1,
        501,
        "http://provider-1.example/live/501.ts",
        &addr1,
        true,
        Some("player-device-a"),
    )
    .await;

    let provider_handle1 = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(&input, &addr1, false, 0, crate::ConnectionKind::Normal, Some(tok1))
        .expect("initial provider slot at max_connections=1");
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);

    register_test_stream(&manager, &user.username, 1, &addr1, &channel, tok1, None, Some(101)).await;
    assert_eq!(manager.user_manager.user_connections(&user.username).await, 1);

    let mgr_for_close = Arc::clone(&manager);
    let close_task = tokio::spawn(async move {
        let reason = close_rx.await.expect("close signal delivered");
        assert_eq!(reason, DisconnectReason::ClientKicked);
        mgr_for_close.release_provider_deferred(&addr1).await;
        mgr_for_close.unregister_close_socket(&addr1);
    });

    let admission = manager
        .replace_playback_id_and_check_admission(
            &user.username,
            1,
            0,
            &addr2,
            "player-device-a",
            true,
            Some("tok-pb-2"),
            true,
        )
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(manager.user_manager.user_connections(&user.username).await, 0);
    assert!(manager.user_manager.is_session_ended(tok1).await);

    close_task.await.expect("close task");
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 0);

    let provider_handle2 = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(
            &input,
            &addr2,
            false,
            0,
            crate::ConnectionKind::Normal,
            Some("tok-pb-2"),
        )
        .expect("replacement provider slot must be free");
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);
    manager.release_provider_handle(Some(provider_handle1));
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);
    manager.release_provider_handle(Some(provider_handle2));
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 0);

    let mut saw_stopped = false;
    while let Ok(ev) = events.try_recv() {
        if matches!(
            ev,
            shared::model::EventMessage::StreamEnded {
                uid: 101,
                reason: shared::model::StreamEndedReason::Stopped,
                ..
            }
        ) {
            saw_stopped = true;
        }
    }
    assert!(saw_stopped, "replacement must emit StreamEnded with Stopped like DELETE /me/streams/{{uid}}");
}

#[tokio::test]
async fn replace_playback_id_does_not_affect_other_users_and_preserves_shared_producer() {
    let (app_cfg, manager) = create_test_connection_manager_with_max_provider(2);
    let input = "provider_1".intern();
    let stream_url = "http://provider-1.example/live/shared.ts";
    let first_addr: SocketAddr = "127.0.0.1:56311".parse().expect("first_addr");
    let next_addr: SocketAddr = "127.0.0.1:56312".parse().expect("next_addr");
    let peer_addr: SocketAddr = "127.0.0.1:56313".parse().expect("peer_addr");

    let alice_sub = tuliprox_core::model::SharedSubscriberId::from_stream_uid(201);
    let bob_sub = tuliprox_core::model::SharedSubscriberId::from_stream_uid(202);
    let origin_handle = manager
        .provider_manager
        .acquire_connection(&input, &first_addr, 0, crate::ConnectionKind::Normal)
        .expect("origin");
    let pending_a =
        SharedStreamManager::reserve_subscriber_cleanup(&manager, alice_sub, first_addr).await.expect("cap a");
    let shared_ctx = crate::SharedStreamCtx {
        app_config: &app_cfg,
        shared_stream_manager: &manager.shared_stream_manager,
        active_provider: &manager.provider_manager,
        connection_manager: &manager,
    };
    let shared_upstream = futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
    assert!(SharedStreamManager::register_shared_stream(
        shared_ctx,
        stream_url,
        shared_upstream,
        &first_addr,
        alice_sub,
        Vec::new(),
        1,
        Some(crate::ManagedProviderHandle::new(Arc::clone(&manager.provider_manager), origin_handle)),
        pending_a,
        0,
        crate::ConnectionKind::Normal,
        "alice",
        "tok-a1",
    )
    .await
    .is_some());

    let peer_join = SharedStreamManager::subscribe_shared_stream(
        shared_ctx,
        stream_url,
        &peer_addr,
        bob_sub,
        0,
        crate::ConnectionKind::Normal,
        "bob",
        "tok-b1",
        None,
    )
    .await
    .expect("subscribe bob ok")
    .expect("subscribe bob joined");
    let _peer_cleanup = peer_join.2;

    let shared_channel = StreamChannel {
        virtual_id: 502,
        title: "shared-502".intern(),
        shared: true,
        shared_joined_existing: Some(false),
        ..make_stream_info("provider_1", "shared-502").channel
    };
    let mut bob_channel = shared_channel.clone();
    bob_channel.shared_joined_existing = Some(true);

    seed_playback_session(&manager, "alice", 1, "tok-a1", 502, stream_url, &first_addr, true, Some("same-id")).await;
    seed_playback_session(&manager, "bob", 1, "tok-b1", 502, stream_url, &peer_addr, true, Some("same-id")).await;
    let alice_cap = Some(SharedCleanupCapability::new(alice_sub));
    let bob_cap = Some(SharedCleanupCapability::new(bob_sub));
    register_test_stream(&manager, "alice", 1, &first_addr, &shared_channel, "tok-a1", alice_cap, None).await;
    register_test_stream(&manager, "bob", 1, &peer_addr, &bob_channel, "tok-b1", bob_cap, None).await;

    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);
    assert!(manager.shared_stream_manager.get_shared_state(stream_url).await.is_some());

    let admission = manager
        .replace_playback_id_and_check_admission("alice", 1, 0, &next_addr, "same-id", true, Some("tok-a2"), true)
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(manager.user_manager.user_connections("alice").await, 0);
    assert_eq!(manager.user_manager.user_connections("bob").await, 1);
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);
    assert!(
        manager.shared_stream_manager.get_shared_state(stream_url).await.is_some(),
        "shared producer must remain active while other subscriber is listening"
    );

    manager.shared_stream_manager.release_subscriber(bob_sub).await;
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 0);
}

async fn resolve_hls_admission(
    adm: &crate::AdmissionCtx,
    user: &ProxyUserCredentials,
    fp: &tuliprox_core::model::Fingerprint,
    session: Option<&crate::UserSession>,
    token: &str,
    activate: bool,
    playback_id: Option<&str>,
) -> (crate::ConnectionAdmission, crate::admission::PlaybackRequestClass) {
    let (res, _, class) = crate::admission::resolve_playback_request_admission_with_playback_id(
        adm,
        user,
        fp,
        session,
        token,
        activate,
        crate::admission::EvictionReentryGuard::Session(token),
        false,
        false,
        playback_id,
    )
    .await;
    (res, class)
}

#[tokio::test]
async fn hls_followup_preserves_session_while_new_token_replaces_pre_segment_session_at_limit() {
    let (app_cfg, manager) = create_test_connection_manager_with_max_provider(1);
    let adm = crate::AdmissionCtx {
        app_config: app_cfg,
        active_users: Arc::clone(&manager.user_manager),
        connection_manager: Arc::clone(&manager),
    };

    let addr1: SocketAddr = "127.0.0.1:56321".parse().expect("addr1");
    let addr2: SocketAddr = "127.0.0.1:56322".parse().expect("addr2");
    let fp1 = tuliprox_core::model::Fingerprint::new("fp-hls".to_string(), addr1.ip().to_string(), addr1);
    let fp2 = tuliprox_core::model::Fingerprint::new("fp-hls".to_string(), addr2.ip().to_string(), addr2);

    let channel = StreamChannel {
        virtual_id: 601,
        title: "hls-601".intern(),
        ..make_stream_info("provider_1", "hls-601").channel
    };
    let tok1 = "tok-hls-1";
    let user = seed_playback_session(
        &manager,
        "hls-playback-user",
        1,
        tok1,
        601,
        "http://provider-1.example/live/601.m3u8",
        &addr1,
        false,
        Some("apple-playback-id"),
    )
    .await;
    register_test_stream(&manager, &user.username, 1, &addr1, &channel, tok1, None, Some(301)).await;
    assert_eq!(manager.user_manager.user_connections(&user.username).await, 1);

    let session1 = manager.user_manager.get_and_update_user_session(&user.username, tok1).await.expect("session1 live");
    let (followup_adm, followup_class) =
        resolve_hls_admission(&adm, &user, &fp1, Some(&session1), tok1, true, Some("apple-playback-id")).await;
    assert_eq!(followup_class, crate::admission::PlaybackRequestClass::FollowUp);
    assert_eq!(followup_adm.permission(), UserConnectionPermission::Allowed);
    assert!(!manager.user_manager.is_session_ended(tok1).await);

    for rejected_id in [None, Some("different-device-id")] {
        let (adm_res, _) = resolve_hls_admission(&adm, &user, &fp2, None, "tok-hls-other", false, rejected_id).await;
        assert_eq!(adm_res.permission(), UserConnectionPermission::Exhausted);
    }
    assert!(!manager.user_manager.is_session_ended(tok1).await);

    let (replace_adm, replace_class) =
        resolve_hls_admission(&adm, &user, &fp2, None, "tok-hls-2", false, Some("apple-playback-id")).await;
    assert_eq!(replace_class, crate::admission::PlaybackRequestClass::Activate);
    assert_eq!(replace_adm.permission(), UserConnectionPermission::Allowed);
    assert!(manager.user_manager.is_session_ended(tok1).await, "replaced HLS session must be marked ended");
    assert_eq!(manager.user_manager.user_connections(&user.username).await, 0);

    seed_playback_session(
        &manager,
        &user.username,
        1,
        "tok-hls-preseg",
        601,
        "http://provider-1.example/live/601.m3u8",
        &addr2,
        false,
        Some("apple-playback-id"),
    )
    .await;
    let (preseg_replace_adm, _) =
        resolve_hls_admission(&adm, &user, &fp1, None, "tok-hls-3", false, Some("apple-playback-id")).await;
    assert_eq!(preseg_replace_adm.permission(), UserConnectionPermission::Allowed);
    assert!(manager.user_manager.is_session_ended("tok-hls-preseg").await);
}

async fn setup_mux_stream_slot(
    manager: &Arc<ConnectionManager>,
    input: &Arc<str>,
    shared_addr: &SocketAddr,
    vid: u32,
    title: &str,
    tok: &str,
    playback_id: &str,
) -> (crate::ManagedProviderHandle, tokio_util::sync::CancellationToken, tokio_util::sync::CancellationToken) {
    let channel =
        StreamChannel { virtual_id: vid, title: title.intern(), ..make_stream_info("provider_1", title).channel };
    let url = format!("http://provider-1.example/live/{vid}.ts");
    seed_playback_session(manager, "mux-user", 2, tok, vid, &url, shared_addr, true, Some(playback_id)).await;
    let raw_handle = manager
        .provider_manager
        .acquire_connection_with_grace_for_session(
            input,
            shared_addr,
            false,
            0,
            crate::ConnectionKind::Normal,
            Some(tok),
        )
        .expect("mux provider slot");
    let cancel = raw_handle.cancel_token.clone().expect("cancel");
    let completion = raw_handle.completion_token.clone().expect("completion");
    let slot = crate::ManagedProviderHandle::new(Arc::clone(&manager.provider_manager), raw_handle);
    register_test_stream(manager, "mux-user", 2, shared_addr, &channel, tok, None, Some(vid)).await;
    (slot, cancel, completion)
}

#[tokio::test]
async fn replace_playback_id_multiplexed_socket_releases_only_replaced_provider_slot() {
    let (_app_cfg, manager) = create_test_connection_manager_with_max_provider(2);
    let input = "provider_1".intern();
    let shared_addr: SocketAddr = "127.0.0.1:56331".parse().expect("shared_addr");
    let next_addr: SocketAddr = "127.0.0.1:56332".parse().expect("next_addr");

    let (slot1, cancel1, completion1) =
        setup_mux_stream_slot(&manager, &input, &shared_addr, 701, "channel-701", "tok-mux-1", "mux-id-1").await;
    let (slot2, cancel2, _completion2) =
        setup_mux_stream_slot(&manager, &input, &shared_addr, 702, "channel-702", "tok-mux-2", "mux-id-2").await;

    // Simulate ActiveClientStream holding slot1 and dropping it upon cancellation.
    let cancel1_wait = cancel1.clone();
    let stream1_task = tokio::spawn(async move {
        cancel1_wait.cancelled().await;
        completion1.cancel();
        drop(slot1);
    });
    assert_eq!(manager.provider_manager.get_provider_connections_count(), 2);

    let admission = manager
        .replace_playback_id_and_check_admission(
            "mux-user",
            2,
            0,
            &next_addr,
            "mux-id-1",
            true,
            Some("tok-mux-3"),
            true,
        )
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert!(cancel1.is_cancelled(), "replaced stream's provider handle must be cancelled");
    assert!(!cancel2.is_cancelled(), "other stream on multiplexed socket must stay active");
    tokio::time::timeout(Duration::from_secs(1), stream1_task)
        .await
        .expect("stream1 task finishes")
        .expect("stream1 task ok");
    assert_eq!(
        manager.provider_manager.get_provider_connections_count(),
        1,
        "provider slot of replaced multiplexed stream must be released"
    );

    let raw_handle3 = manager.provider_manager.acquire_connection_with_grace_for_session(
        &input,
        &next_addr,
        false,
        0,
        crate::ConnectionKind::Normal,
        Some("tok-mux-3"),
    );
    assert!(raw_handle3.is_some(), "replacement request must be able to acquire freed provider slot");
    manager.release_provider_handle(raw_handle3);
    drop(slot2);
}

#[tokio::test]
async fn playback_id_admission_ignores_user_limits_when_user_access_control_disabled() {
    let (app_cfg, manager) = create_test_connection_manager_with_max_provider(4);
    app_cfg.config.store(Arc::new(Config { user_access_control: false, ..(*app_cfg.config.load_full()).clone() }));
    let adm = crate::AdmissionCtx {
        app_config: app_cfg,
        active_users: Arc::clone(&manager.user_manager),
        connection_manager: Arc::clone(&manager),
    };

    let addr1: SocketAddr = "127.0.0.1:56341".parse().expect("addr1");
    let addr2: SocketAddr = "127.0.0.1:56342".parse().expect("addr2");
    let fp2 = tuliprox_core::model::Fingerprint::new("fp-2".to_string(), addr2.ip().to_string(), addr2);
    let channel = StreamChannel {
        virtual_id: 711,
        title: "channel-711".intern(),
        ..make_stream_info("provider_1", "channel-711").channel
    };
    let mut user = ProxyUserCredentials::default();
    user.username = "uac-off-user".to_string();
    user.max_connections = 1;

    register_test_stream(&manager, &user.username, 1, &addr1, &channel, "tok-no-id", None, Some(711)).await;
    assert_eq!(manager.user_manager.user_connections(&user.username).await, 1);

    let (admission, _, _) = crate::admission::resolve_playback_request_admission_with_playback_id(
        &adm,
        &user,
        &fp2,
        None,
        "tok-dev-b",
        false,
        crate::admission::EvictionReentryGuard::Session("tok-dev-b"),
        false,
        false,
        Some("dev-b"),
    )
    .await;
    assert_eq!(
        admission.permission(),
        UserConnectionPermission::Allowed,
        "user.max_connections must not reject playback-id admission when user_access_control is false"
    );
}

#[tokio::test]
async fn replaced_placeholder_cannot_register_stream_afterwards() {
    let manager = create_test_connection_manager();
    let addr1: SocketAddr = "127.0.0.1:56351".parse().expect("addr1");
    let addr2: SocketAddr = "127.0.0.1:56352".parse().expect("addr2");
    let fp1 = tuliprox_core::model::Fingerprint::new("fp-1".to_string(), addr1.ip().to_string(), addr1);
    let channel = StreamChannel {
        virtual_id: 712,
        title: "channel-712".intern(),
        ..make_stream_info("provider_1", "channel-712").channel
    };
    let mut user = ProxyUserCredentials::default();
    user.username = "stale-placeholder-user".to_string();
    user.max_connections = 1;

    let (_, identity1) = manager
        .user_manager
        .ensure_user_session_placeholder_with_identity(
            CreateUserSessionParams {
                user: &user,
                session_token: "tok-stale-1",
                virtual_id: 712,
                provider: "provider_1",
                stream_url: "http://provider_1/live/1",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::ConnectionKind::Normal),
                socket_bound: false,
            },
            Some("same-playback-id"),
            true,
        )
        .await;

    let (admission2, _placeholder2) = manager
        .replace_playback_id_and_check_admission_with_placeholder(
            &user.username,
            user.max_connections,
            user.soft_connections,
            &addr2,
            "same-playback-id",
            true,
            Some("tok-stale-2"),
            false,
            Some(CreateUserSessionParams {
                user: &user,
                session_token: "tok-stale-2",
                virtual_id: 712,
                provider: "provider_1",
                stream_url: "http://provider_1/live/1",
                addr: &addr2,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::ConnectionKind::Normal),
                socket_bound: false,
            }),
        )
        .await;
    assert_eq!(admission2.permission(), UserConnectionPermission::Allowed);

    manager.add_connection(&addr1).await;
    let make_params = || ConnectionParams {
        meter_uid: 0,
        username: &user.username,
        max_connections: user.max_connections,
        soft_connections: user.soft_connections,
        connection_kind: crate::ConnectionKind::Normal,
        priority: 0,
        soft_priority: 0,
        fingerprint: &fp1,
        provider: "provider_1".intern(),
        stream_channel: &channel,
        user_agent: std::borrow::Cow::Borrowed("player/1.0"),
        session_token: Some("tok-stale-1"),
    };

    let with_reg = manager
        .update_connection_with_uid_and_session_registration(
            make_params(),
            ConnectionHistoryMode::EmitConnect,
            1,
            None,
            Some(&crate::PlaybackSessionRegistration {
                identity: identity1,
                enforce_limits: true,
                grace_admitted: false,
            }),
        )
        .await;
    assert!(with_reg.display_stream.is_none(), "fenced registration of replaced placeholder must fail");
    assert_eq!(with_reg.rejection_reason(), Some(ConnectionRejectionReason::RegistrationFailed));

    let without_reg = manager
        .update_connection_with_uid_and_session_registration(
            make_params(),
            ConnectionHistoryMode::EmitConnect,
            2,
            None,
            None,
        )
        .await;
    assert!(without_reg.display_stream.is_none(), "unfenced registration of ended token must also fail");
    assert_eq!(without_reg.rejection_reason(), Some(ConnectionRejectionReason::RegistrationFailed));
    assert_eq!(manager.user_manager.user_connections(&user.username).await, 0);
}
