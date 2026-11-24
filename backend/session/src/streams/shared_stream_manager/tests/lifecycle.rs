use super::*;

#[tokio::test]
async fn send_client_chunk_returns_when_cancelled_while_queue_is_full() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, _rx) = mpsc::channel::<BudgetedChunk>(1);
    let byte_budget = Arc::new(Semaphore::new(1024));
    let queued_permit = byte_budget.clone().acquire_many_owned(6).await?;
    tx.send(BudgetedChunk { bytes: Bytes::from_static(b"queued"), _permit: queued_permit }).await?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = timeout(
        Duration::from_secs(1),
        send_client_chunk(
            &tx,
            Bytes::from_static(b"blocked"),
            &cancel,
            Instant::now() + Duration::from_secs(1),
            &byte_budget,
        ),
    )
    .await?;
    assert_eq!(outcome, SendOutcome::Cancelled);
    Ok(())
}

#[tokio::test]
async fn dropping_unpolled_shared_response_releases_its_subscription() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41005".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/unpolled.ts";
    register_user(&users, addr, id.stream_uid(), "first", url).await?;
    let allocation = ManagedProviderHandle::new(
        Arc::clone(&providers),
        providers
            .acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal)
            .ok_or("provider allocation missing")?,
    );
    let pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr)
        .await
        .map_err(|_| "shared cleanup admission failed")?;
    let response = SharedStreamManager::register_shared_stream(
        super::super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        },
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &addr,
        id,
        Vec::new(),
        8,
        Some(allocation),
        pending_cleanup,
        0,
        ConnectionKind::Normal,
        "first",
        "first_token",
    )
    .await
    .ok_or("subscription missing")?;
    drop(response);
    timeout(Duration::from_secs(2), async {
        while !users.active_streams().await.is_empty() || providers.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(manager.shared_streams.read().await.key_by_subscriber.is_empty());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn saturated_cleanup_queue_rejects_shared_subscriber_before_registration(
) -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let (_, _, _, connections) = create_test_connection_manager(&app_cfg, &events);
    let cleanup_tx = connections.cleanup_tx();
    let available = cleanup_tx.capacity();
    let mut permits = Vec::with_capacity(available);
    for _ in 0..available {
        permits.push(cleanup_tx.clone().try_reserve_owned()?);
    }

    let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    let addr = "127.0.0.1:41008".parse()?;
    let subscriber_id = SharedSubscriberId::from_stream_uid(88);
    let subscribe_connections = Arc::clone(&connections);
    let subscribe = tokio::spawn(async move {
        SharedStreamManager::reserve_subscriber_cleanup(&subscribe_connections, subscriber_id, addr).await
    });

    tokio::task::yield_now().await;
    tokio::time::advance(SHARED_CLEANUP_ADMISSION_TIMEOUT + Duration::from_millis(1)).await;
    assert!(subscribe.await?.is_err(), "saturated cleanup admission must reject the subscriber");
    assert!(state.subscribers.read().await.is_empty(), "rejected admission must not register a subscriber");
    assert!(state.lock_task_handles().is_empty(), "rejected admission must not spawn a forwarding task");

    drop(permits);
    Ok(())
}

#[tokio::test]
async fn duplicate_subscriber_release_preserves_other_channel_on_same_socket() -> Result<(), Box<dyn std::error::Error>>
{
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let (_, _, manager, _) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41006".parse()?;
    let first = SharedSubscriberId::from_stream_uid(1);
    let second = SharedSubscriberId::from_stream_uid(2);
    let first_url: Arc<str> = Arc::from("https://example.invalid/first.ts");
    let second_url: Arc<str> = Arc::from("https://example.invalid/second.ts");
    let state_a = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    let state_b = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    let surviving_token = CancellationToken::new();
    state_a.register_subscriber(first, &addr, CancellationToken::new()).await;
    state_b.register_subscriber(second, &addr, surviving_token.clone()).await;
    {
        let mut register = manager.shared_streams.write().await;
        register.by_key.insert(Arc::clone(&first_url), state_a);
        register.by_key.insert(Arc::clone(&second_url), state_b);
        register.key_by_subscriber.insert(first, Arc::clone(&first_url));
        register.key_by_subscriber.insert(second, Arc::clone(&second_url));
    }
    manager.release_subscriber(first).await;
    manager.release_subscriber(first).await;
    assert!(manager.get_shared_state(&first_url).await.is_none());
    assert!(manager.get_shared_state(&second_url).await.is_some());
    assert!(!surviving_token.is_cancelled());
    manager.release_connection(&addr, true).await;
    assert!(surviving_token.is_cancelled());
    Ok(())
}

#[tokio::test]
async fn test_duplicate_release_connection_is_idempotent_with_single_subscriber() {
    let app_cfg = create_test_app_config();
    let event_manager = Arc::new(EventManager::new());
    let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));

    let stream_url = "https://example.invalid/live/single.ts";
    let addr_1: SocketAddr = "127.0.0.1:42001".parse().unwrap_or_else(|_| unreachable!());
    let id = SharedSubscriberId::from_stream_uid(1);
    let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));

    {
        let mut reg = shared_manager.shared_streams.write().await;
        reg.by_key.insert(Arc::from(stream_url), Arc::clone(&state));
        reg.key_by_subscriber.insert(id, Arc::from(stream_url));
    }

    state.register_subscriber(id, &addr_1, CancellationToken::new()).await;

    shared_manager.release_connection(&addr_1, false).await;
    {
        let reg = shared_manager.shared_streams.read().await;
        assert!(!reg.by_key.contains_key(stream_url));
        assert!(!reg.key_by_subscriber.contains_key(&id));
    }
    {
        let subs = state.subscribers.read().await;
        assert!(subs.is_empty());
    }

    shared_manager.release_connection(&addr_1, false).await;
    {
        let reg = shared_manager.shared_streams.read().await;
        assert!(!reg.by_key.contains_key(stream_url));
        assert!(!reg.key_by_subscriber.contains_key(&id));
    }
    {
        let subs = state.subscribers.read().await;
        assert!(subs.is_empty());
    }
}

#[tokio::test]
async fn shared_origin_abort_before_user_registration_releases_physical_and_logical_request(
) -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, users, _manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41010".parse()?;
    let owner = "shared-owner-1";
    let handle = providers
        .acquire_connection_with_lease_for_session(
            &"provider_1".intern(),
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::active_provider_manager::PlaybackLeaseRef::new(
                owner,
                tuliprox_core::model::PlaybackKind::LiveTs,
            )),
        )
        .ok_or("provider allocation missing")?;
    let request_id = handle.playback_request_id.ok_or("provider request identity missing")?;
    let managed = ManagedProviderHandle::new(Arc::clone(&providers), handle);
    let subscriber_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let mut pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, subscriber_id, addr)
        .await
        .map_err(|err| format!("shared cleanup admission failed: {err}"))?;
    pending_cleanup.set_provider_request_identity(owner, request_id);
    assert_eq!(providers.get_provider_connections_count(), 1);
    assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).starting, 1);

    drop(managed);
    drop(pending_cleanup);

    timeout(Duration::from_secs(2), async {
        while providers.provider_lease_usage(&"provider_1".intern()).total() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(providers.get_provider_connections_count(), 0);
    assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).total(), 0);
    assert_eq!(users.active_streams().await.len(), 0);
    Ok(())
}

#[tokio::test]
async fn shared_origin_registration_rejected_releases_preacquired_handle() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41011".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/rejected_preacquired.ts";
    let owner = "rejected-preacquired-owner";
    let handle = providers
        .acquire_connection_with_lease_for_session(
            &"provider_1".intern(),
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::active_provider_manager::PlaybackLeaseRef::new(
                owner,
                tuliprox_core::model::PlaybackKind::LiveTs,
            )),
        )
        .ok_or("provider allocation missing")?;
    let request_id = handle.playback_request_id.ok_or("provider request identity missing")?;
    let managed_handle = ManagedProviderHandle::new(Arc::clone(&providers), handle);
    assert_eq!(providers.get_provider_connections_count(), 1);

    let dummy_state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    manager.shared_streams.write().await.by_key.insert(Arc::from(url), dummy_state);

    let mut pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
    pending_cleanup.set_provider_request_identity(owner, request_id);
    let res = SharedStreamManager::register_shared_stream(
        super::super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        },
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &addr,
        id,
        Vec::new(),
        8,
        Some(managed_handle),
        pending_cleanup,
        0,
        ConnectionKind::Normal,
        "first",
        "first_token",
    )
    .await;

    assert!(res.is_none());
    timeout(Duration::from_secs(2), async {
        while providers.get_provider_connections_count() != 0
            || providers.provider_lease_usage(&"provider_1".intern()).total() != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(providers.get_provider_connections_count(), 0);
    assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).total(), 0);
    Ok(())
}

#[tokio::test]
async fn shared_origin_abort_waiting_for_registry_lock_releases_handle() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41012".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/abort_waiting.ts";
    let owner = "abort-waiting-owner";

    let lock_guard = manager.shared_streams.write().await;

    let handle = providers
        .acquire_connection_with_lease_for_session(
            &"provider_1".intern(),
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::active_provider_manager::PlaybackLeaseRef::new(
                owner,
                tuliprox_core::model::PlaybackKind::LiveTs,
            )),
        )
        .ok_or("provider allocation missing")?;
    let request_id = handle.playback_request_id.ok_or("provider request identity missing")?;
    let managed_handle = ManagedProviderHandle::new(Arc::clone(&providers), handle);
    assert_eq!(providers.get_provider_connections_count(), 1);

    let mut pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
    pending_cleanup.set_provider_request_identity(owner, request_id);

    let register_task = tokio::spawn({
        let app_cfg = Arc::clone(&app_cfg);
        let manager = Arc::clone(&manager);
        let providers = Arc::clone(&providers);
        let connections = Arc::clone(&connections);
        async move {
            let ctx = super::super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            };
            SharedStreamManager::register_shared_stream(
                ctx,
                url,
                futures::stream::pending::<Result<Bytes, std::io::Error>>(),
                &addr,
                id,
                Vec::new(),
                8,
                Some(managed_handle),
                pending_cleanup,
                0,
                ConnectionKind::Normal,
                "first",
                "first_token",
            )
            .await
        }
    });

    tokio::task::yield_now().await;

    register_task.abort();
    let _ = register_task.await;

    drop(lock_guard);

    timeout(Duration::from_secs(2), async {
        while providers.get_provider_connections_count() != 0
            || providers.provider_lease_usage(&"provider_1".intern()).total() != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(providers.get_provider_connections_count(), 0);
    assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).total(), 0);
    Ok(())
}

#[tokio::test]
async fn shutdown_with_unpolled_shared_body_leaves_no_requests_or_subscribers() -> Result<(), Box<dyn std::error::Error>>
{
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41016".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/unpolled_shutdown.ts";
    register_user(&users, addr, id.stream_uid(), "shutdown_user", url).await?;

    let handle = providers
        .acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal)
        .ok_or("provider allocation missing")?;
    let managed_handle = ManagedProviderHandle::new(Arc::clone(&providers), handle);
    let pending = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
    let (unpolled_body, _, _) = SharedStreamManager::register_shared_stream(
        super::super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        },
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &addr,
        id,
        Vec::new(),
        8,
        Some(managed_handle),
        pending,
        0,
        ConnectionKind::Normal,
        "first",
        "first_token",
    )
    .await
    .ok_or("register failed")?;

    assert_eq!(providers.get_provider_connections_count(), 1);
    assert_eq!(users.active_streams().await.len(), 1);

    connections.shutdown().await;

    assert_eq!(providers.get_provider_connections_count(), 0);
    assert_eq!(users.active_streams().await.len(), 0);
    assert!(manager.get_shared_state(url).await.is_none());

    drop(unpolled_body);
    Ok(())
}

#[tokio::test]
async fn shared_state_removed_between_preflight_and_commit_leaves_no_claim() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41018".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/removed_between.ts";

    let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    manager.shared_streams.write().await.by_key.insert(Arc::from(url), state);

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    if let Ok(mut lock) = manager.test_preflight_barrier.lock() {
        *lock = Some(Arc::clone(&barrier));
    }

    let initial_capacity = connections.cleanup_tx().capacity();

    let sub_task = tokio::spawn({
        let app_cfg = Arc::clone(&app_cfg);
        let manager = Arc::clone(&manager);
        let providers = Arc::clone(&providers);
        let connections = Arc::clone(&connections);
        async move {
            let ctx = super::super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            };
            SharedStreamManager::subscribe_shared_stream(
                ctx,
                url,
                &addr,
                id,
                0,
                ConnectionKind::Normal,
                "first",
                "first_token",
                None,
            )
            .await
        }
    });

    barrier.wait().await;

    {
        let mut reg = manager.shared_streams.write().await;
        reg.by_key.remove(url);
    }

    let res = sub_task.await??;
    assert!(res.is_none());
    assert_eq!(connections.cleanup_tx().capacity(), initial_capacity);
    assert!(manager.get_shared_state(url).await.is_none());
    Ok(())
}

#[test]
fn pending_meter_rollback_does_not_remove_adopted_entry() {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
    let manager = Arc::new(SharedStreamManager::new(providers));
    let url = "https://example.invalid/live/meter-replacement.ts";

    // A reserved the meter; B then commits a shared origin and adopts it.
    let (_, stale) = manager.reserve_meter_uid(url, || 51);
    manager.adopt_meter_uid(url, 999);

    // A aborts: its pending rollback must not remove the adopted entry.
    drop(stale);
    assert_eq!(manager.meter_count(), 1);
    assert_eq!(manager.lock_meter_uids().get(url).map(|entry| entry.uid), Some(51));
    assert_eq!(manager.lock_meter_uids().get(url).and_then(|entry| entry.owner), Some(999));
}

#[tokio::test]
async fn shutdown_clears_shared_meter_registrations() {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
    let manager = Arc::new(SharedStreamManager::new(providers));

    let (_, pending) = manager.reserve_meter_uid("https://example.invalid/live/meter-shutdown.ts", || 61);
    pending.expect("new registration").commit();
    assert_eq!(manager.meter_count(), 1);

    manager.shutdown().await;
    assert_eq!(manager.meter_count(), 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn free_join_count_tracks_all_lifecycle_and_removal_paths() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config_with_conns(10));
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let url = "https://example.invalid/live/free-join-lifecycle.ts";

    assert_eq!(manager.user_free_shared_joins("free_user").await, 0);

    let setup_stream = |owner_port: u16, stream_url: &'static str| {
        let app_cfg = Arc::clone(&app_cfg);
        let providers = Arc::clone(&providers);
        let manager = Arc::clone(&manager);
        let connections = Arc::clone(&connections);
        let users = Arc::clone(&users);
        async move {
            let addr: SocketAddr = format!("127.0.0.1:{owner_port}").parse().unwrap();
            let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
            register_user(&users, addr, id.stream_uid(), "owner_user", stream_url).await.unwrap();
            let allocation = ManagedProviderHandle::new(
                Arc::clone(&providers),
                providers.acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal).unwrap(),
            );
            let pending_cleanup =
                SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await.unwrap();
            SharedStreamManager::register_shared_stream(
                super::super::SharedStreamCtx {
                    app_config: &app_cfg,
                    shared_stream_manager: &manager,
                    active_provider: &providers,
                    connection_manager: &connections,
                },
                stream_url,
                futures::stream::pending::<Result<Bytes, std::io::Error>>(),
                &addr,
                id,
                Vec::new(),
                8,
                Some(allocation),
                pending_cleanup,
                0,
                ConnectionKind::Normal,
                "owner_user",
                "owner_token",
            )
            .await
            .unwrap()
        }
    };

    let subscribe_free = |sub_port: u16, stream_url: &'static str| {
        let app_cfg = Arc::clone(&app_cfg);
        let providers = Arc::clone(&providers);
        let manager = Arc::clone(&manager);
        let connections = Arc::clone(&connections);
        let users = Arc::clone(&users);
        async move {
            let sub_addr: SocketAddr = format!("127.0.0.1:{sub_port}").parse().unwrap();
            let sub_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
            register_user(&users, sub_addr, sub_id.stream_uid(), "free_user", stream_url).await.unwrap();
            let result = SharedStreamManager::subscribe_shared_stream(
                super::super::SharedStreamCtx {
                    app_config: &app_cfg,
                    shared_stream_manager: &manager,
                    active_provider: &providers,
                    connection_manager: &connections,
                },
                stream_url,
                &sub_addr,
                sub_id,
                0,
                ConnectionKind::Normal,
                "free_user",
                "free_token",
                Some(10),
            )
            .await
            .unwrap()
            .unwrap();
            (sub_id, sub_addr, result)
        }
    };

    // 1. Initial register and free join
    let _owner_res1 = setup_stream(42001, url).await;
    let (sub_id1, sub_addr1, _sub_res1) = subscribe_free(42002, url).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 1, "free subscriber registered");

    // 2. Conversion (free -> paid -> free)
    manager.set_subscriber_free_join(sub_id1, false).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 0, "conversion to paid decrements count");
    manager.set_subscriber_free_join(sub_id1, true).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 1, "conversion to free increments count");

    // 3. Removal via subscriber disconnect (release_connection)
    manager.release_connection(&sub_addr1, false).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 0, "subscriber disconnect decrements count");

    // 4. Removal via stream unregister
    let (_sub_id2, _sub_addr2, _sub_res2) = subscribe_free(42003, url).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 1);
    let state = manager.get_shared_state(url).await.unwrap();
    manager.unregister(url, &state).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 0, "unregister decrements count");

    // 5. Removal via teardown_preempted_stream
    let url_preempt = "https://example.invalid/live/free-join-preempt.ts";
    let _owner_res2 = setup_stream(42004, url_preempt).await;
    let (_sub_id3, _sub_addr3, _sub_res3) = subscribe_free(42005, url_preempt).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 1);
    let state = manager.get_shared_state(url_preempt).await.unwrap();
    let alloc_id = state.provider_allocation_id().unwrap();
    manager.teardown_preempted_stream(url_preempt, alloc_id).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 0, "teardown decrements count");

    // 6. Removal via shutdown
    let url_shutdown = "https://example.invalid/live/free-join-shutdown.ts";
    let _owner_res3 = setup_stream(42006, url_shutdown).await;
    let (_sub_id4, _sub_addr4, _sub_res4) = subscribe_free(42007, url_shutdown).await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 1);
    manager.shutdown().await;
    assert_eq!(manager.user_free_shared_joins("free_user").await, 0, "shutdown clears count");

    Ok(())
}

#[tokio::test]
async fn teardown_between_reserve_and_subscriber_insert_leaves_count_zero() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config_with_conns(10));
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let url = "https://example.invalid/live/teardown-window.ts";
    let owner_addr = "127.0.0.1:42101".parse()?;
    let owner_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    register_user(&users, owner_addr, owner_id.stream_uid(), "owner", url).await?;
    let allocation = ManagedProviderHandle::new(
        Arc::clone(&providers),
        providers.acquire_connection(&"provider_1".intern(), &owner_addr, 0, ConnectionKind::Normal).unwrap(),
    );
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&connections, owner_id, owner_addr).await.unwrap();
    let ctx = super::super::SharedStreamCtx {
        app_config: &app_cfg,
        shared_stream_manager: &manager,
        active_provider: &providers,
        connection_manager: &connections,
    };
    let _ = SharedStreamManager::register_shared_stream(
        ctx,
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &owner_addr,
        owner_id,
        Vec::new(),
        8,
        Some(allocation),
        pending_cleanup,
        0,
        ConnectionKind::Normal,
        "owner",
        "owner_token",
    )
    .await
    .unwrap();

    let sub_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    // Reserve free join in the register directly under write lock (simulating window before register_subscriber_entry)
    {
        let mut reg = manager.shared_streams.write().await;
        assert!(reg.try_reserve_free_join(sub_id, "free_user", 1));
        reg.key_by_subscriber.insert(sub_id, Arc::from(url));
    }
    assert_eq!(manager.user_free_shared_joins("free_user").await, 1);

    // Teardown occurs in this window (e.g. unregister the stream)
    let state = manager.get_shared_state(url).await.unwrap();
    manager.unregister(url, &state).await;

    // Count must be decremented to 0 even though subscriber was not in state.subscribers
    assert_eq!(manager.user_free_shared_joins("free_user").await, 0);
    Ok(())
}

#[tokio::test]
async fn settlement_does_not_block_cleanup_worker() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config_with_conns(10));
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    manager.set_user_manager(&users);
    let url = "https://example.invalid/live/settle-block.ts";

    let owner_addr = "127.0.0.1:42201".parse()?;
    let owner_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    register_user(&users, owner_addr, owner_id.stream_uid(), "owner", url).await?;
    let allocation = ManagedProviderHandle::new(
        Arc::clone(&providers),
        providers.acquire_connection(&"provider_1".intern(), &owner_addr, 0, ConnectionKind::Normal).unwrap(),
    );
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&connections, owner_id, owner_addr).await.unwrap();
    let ctx = super::super::SharedStreamCtx {
        app_config: &app_cfg,
        shared_stream_manager: &manager,
        active_provider: &providers,
        connection_manager: &connections,
    };
    let _ = SharedStreamManager::register_shared_stream(
        ctx,
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &owner_addr,
        owner_id,
        Vec::new(),
        8,
        Some(allocation),
        pending_cleanup,
        0,
        ConnectionKind::Normal,
        "owner",
        "owner_token",
    )
    .await
    .unwrap();

    let sub_addr = "127.0.0.1:42202".parse()?;
    let sub_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    register_user(&users, sub_addr, sub_id.stream_uid(), "free_user", url).await?;
    let _ = SharedStreamManager::subscribe_shared_stream(
        ctx,
        url,
        &sub_addr,
        sub_id,
        0,
        ConnectionKind::Normal,
        "free_user",
        "free_token",
        Some(1),
    )
    .await
    .unwrap()
    .unwrap();

    // Acquire playback transition on free_user session, holding the gate
    let _held_gate = users.acquire_playback_transition("free_user", "free_token").await;

    // Releasing the owner initiates settlement. Because settlement is spawned detached via tokio::spawn,
    // release_subscriber must return immediately rather than waiting for _held_gate.
    let start = Instant::now();
    timeout(Duration::from_millis(150), manager.release_subscriber(owner_id))
        .await
        .expect("release_subscriber must not block waiting for playback transition");
    assert!(start.elapsed() < Duration::from_millis(100));

    Ok(())
}

#[tokio::test]
async fn test_live_free_joins_counter_and_gate() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config_with_conns(10));
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    manager.set_user_manager(&users);
    users.set_shared_stream_manager(&manager);

    let url = "http://localhost/shared_counter_test.ts";
    let owner_addr: SocketAddr = "127.0.0.1:55120".parse().unwrap();
    let owner_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    register_user(&users, owner_addr, owner_id.stream_uid(), "owner", url).await?;
    let allocation = ManagedProviderHandle::new(
        Arc::clone(&providers),
        providers.acquire_connection(&"provider_1".intern(), &owner_addr, 0, ConnectionKind::Normal).unwrap(),
    );
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&connections, owner_id, owner_addr).await.unwrap();
    let ctx = super::super::SharedStreamCtx {
        app_config: &app_cfg,
        shared_stream_manager: &manager,
        active_provider: &providers,
        connection_manager: &connections,
    };
    let _ = SharedStreamManager::register_shared_stream(
        ctx,
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &owner_addr,
        owner_id,
        Vec::new(),
        8,
        Some(allocation),
        pending_cleanup,
        0,
        ConnectionKind::Normal,
        "owner",
        "owner_token",
    )
    .await
    .unwrap();

    let stream_uid = connections.next_stream_uid();
    let subscriber_id = SharedSubscriberId::from_stream_uid(stream_uid);

    // Initial state: count == 0, has_free_joins == false, is_free_subscriber == false
    assert_eq!(manager.live_free_joins_count(), 0);
    assert!(!manager.has_free_joins());
    assert!(!users.is_free_subscriber(stream_uid).await);

    // 1. Reserve free join
    {
        let mut reg = manager.shared_streams.write().await;
        assert!(reg.try_reserve_free_join(subscriber_id, "free_user", 1));
        reg.key_by_subscriber.insert(subscriber_id, Arc::from(url));
    }
    assert_eq!(manager.live_free_joins_count(), 1);
    assert!(manager.has_free_joins());

    // Register subscriber in the shared state so is_subscriber_free finds it
    let state = manager.get_shared_state(url).await.unwrap();
    {
        let mut subs = state.subscribers.write().await;
        subs.insert(
            subscriber_id,
            crate::streams::shared_stream_manager::SharedSubscriber {
                addr: "127.0.0.1:55121".parse().unwrap(),
                cancel_token: tokio_util::sync::CancellationToken::new(),
                username: Arc::from("free_user"),
                session_token: Arc::from("tok-free"),
                free_join: true,
            },
        );
    }
    // Now is_free_subscriber returns true for the reserved subscriber, false for another uid
    assert!(users.is_free_subscriber(stream_uid).await);
    assert!(!users.is_free_subscriber(stream_uid + 999).await);

    // 2. Release free join for subscriber
    {
        let mut reg = manager.shared_streams.write().await;
        reg.remove_free_join_for_subscriber(subscriber_id);
    }
    assert_eq!(manager.live_free_joins_count(), 0);
    assert!(!manager.has_free_joins());
    assert!(!users.is_free_subscriber(stream_uid).await);

    // 3. Reserve again and teardown stream
    let teardown_sub_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    {
        let mut reg = manager.shared_streams.write().await;
        assert!(reg.try_reserve_free_join(teardown_sub_id, "free_user", 1));
        reg.key_by_subscriber.insert(teardown_sub_id, Arc::from(url));
    }
    assert_eq!(manager.live_free_joins_count(), 1);
    assert!(manager.has_free_joins());

    // Teardown stream
    manager.unregister(url, &state).await;
    assert_eq!(manager.live_free_joins_count(), 0);
    assert!(!manager.has_free_joins());

    Ok(())
}
