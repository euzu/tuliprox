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
    assert!(users.active_streams().await.is_empty());
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
    )
    .await
    .ok_or("register failed")?;

    assert_eq!(providers.get_provider_connections_count(), 1);
    assert_eq!(users.active_streams().await.len(), 1);

    connections.shutdown().await;

    assert_eq!(providers.get_provider_connections_count(), 0);
    assert!(users.active_streams().await.is_empty());
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
            SharedStreamManager::subscribe_shared_stream(ctx, url, &addr, id, 0, ConnectionKind::Normal).await
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
