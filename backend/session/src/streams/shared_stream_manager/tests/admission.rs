use super::*;

#[tokio::test]
async fn shared_clients_on_same_socket_keep_independent_streams_and_capacity() -> Result<(), Box<dyn std::error::Error>>
{
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41004".parse()?;
    let first_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let second_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/shared.ts";
    register_user(&users, addr, first_id.stream_uid(), "first", url).await?;
    let allocation = ManagedProviderHandle::new(
        Arc::clone(&providers),
        providers
            .acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal)
            .ok_or("provider allocation missing")?,
    );
    let ctx = super::super::SharedStreamCtx {
        app_config: &app_cfg,
        shared_stream_manager: &manager,
        active_provider: &providers,
        connection_manager: &connections,
    };
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, first_id, addr)
        .await
        .map_err(|_| "shared cleanup admission failed")?;
    let (mut first, _, _) = SharedStreamManager::register_shared_stream(
        ctx,
        url,
        ReceiverStream::new(rx),
        &addr,
        first_id,
        Vec::new(),
        8,
        Some(allocation),
        pending_cleanup,
        0,
        ConnectionKind::Normal,
    )
    .await
    .ok_or("first subscription missing")?;
    let (mut second, _, _) =
        SharedStreamManager::subscribe_shared_stream(ctx, url, &addr, second_id, 0, ConnectionKind::Normal)
            .await
            .map_err(|_| "second shared cleanup admission failed")?
            .ok_or("second subscription missing")?;
    register_user(&users, addr, second_id.stream_uid(), "second", url).await?;

    tx.send(Ok(Bytes::from_static(b"first chunk"))).await?;
    assert_eq!(
        timeout(Duration::from_secs(2), first.next()).await?.ok_or("first ended")??,
        Bytes::from_static(b"first chunk")
    );
    assert_eq!(
        timeout(Duration::from_secs(2), second.next()).await?.ok_or("second ended")??,
        Bytes::from_static(b"first chunk")
    );
    assert_eq!(users.active_streams().await.len(), 2);
    assert_eq!(providers.get_provider_connections_count(), 1);

    drop(first);
    timeout(Duration::from_secs(2), async {
        while users.active_streams().await.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    // A delayed duplicate cleanup cannot remove the surviving subscriber.
    connections.send_cleanup(crate::CleanupEvent::ReleaseSharedSubscriber {
        addr,
        subscriber_id: first_id,
        request_id: None,
        owner: None,
    });
    tx.send(Ok(Bytes::from_static(b"second chunk"))).await?;
    assert_eq!(
        timeout(Duration::from_secs(2), second.next()).await?.ok_or("survivor ended")??,
        Bytes::from_static(b"second chunk")
    );
    assert_eq!(providers.get_provider_connections_count(), 1);
    assert_eq!(users.active_streams().await.first().map(|stream| stream.uid), Some(second_id.stream_uid()));

    drop(second);
    timeout(Duration::from_secs(2), async {
        while !users.active_streams().await.is_empty() || providers.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(manager.get_shared_state(url).await.is_none());
    Ok(())
}

#[test]
fn test_shared_state_channel_capacity_does_not_scale_with_burst_buffer_bytes() {
    let min_burst_buffer_size = 12 * 1024 * 1024;
    let state = SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, min_burst_buffer_size, None);

    assert_eq!(state.buf_size, CHANNEL_SIZE);
}

#[test]
fn test_burst_buffer_chunk_bound_preserves_ts_sized_byte_capacity() {
    let max_chunks = 4;
    let mut buffer = BurstBuffer::new(MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES * max_chunks);
    for _ in 0..max_chunks.saturating_add(1) {
        buffer.push(Bytes::from(vec![0_u8; MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES]));
    }

    let mut chunks = Vec::new();
    let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

    assert_eq!(buffer.buffer.len(), max_chunks);
    assert_eq!(buffer.current_bytes, MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES * max_chunks);
    assert_eq!(read.skipped, 1);
    assert_eq!(chunks.len(), max_chunks);
}

#[tokio::test(start_paused = true)]
async fn shared_subscription_admission_has_deadline_and_no_unprotected_fallback(
) -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let (_, _, _, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr: SocketAddr = "127.0.0.1:43002".parse()?;
    let id = SharedSubscriberId::from_stream_uid(1);

    // Saturate the cleanup queue so the admission await blocks; the shared admission
    // deadline must reject the subscriber instead of hanging or falling back.
    let pending = || crate::CleanupEvent::Defer(Box::pin(std::future::pending::<()>()));
    connections.send_cleanup(pending());
    for _ in 0..4096 {
        connections.send_cleanup(pending());
    }

    let admission = tokio::spawn({
        let connections = Arc::clone(&connections);
        async move { SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await }
    });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::time::advance(super::super::SHARED_CLEANUP_ADMISSION_TIMEOUT + Duration::from_secs(1)).await;
    let result = admission.await?;
    assert!(result.is_err(), "saturated shared subscription admission must be rejected, not fall back");
    Ok(())
}

#[tokio::test]
async fn concurrent_shared_origin_creation_releases_only_losing_allocation() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config_with_conns(2));
    let events = Arc::new(EventManager::new());
    let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr_1 = "127.0.0.1:41013".parse()?;
    let addr_2 = "127.0.0.1:41014".parse()?;
    let id_1 = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let id_2 = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/concurrent_origin.ts";

    let handle_1 = providers
        .acquire_connection(&"provider_1".intern(), &addr_1, 0, ConnectionKind::Normal)
        .ok_or("first provider allocation missing")?;
    let managed_1 = ManagedProviderHandle::new(Arc::clone(&providers), handle_1);
    let pending_1 = SharedStreamManager::reserve_subscriber_cleanup(&connections, id_1, addr_1).await?;
    let (first_stream, _, _) = SharedStreamManager::register_shared_stream(
        super::super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        },
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &addr_1,
        id_1,
        Vec::new(),
        8,
        Some(managed_1),
        pending_1,
        0,
        ConnectionKind::Normal,
    )
    .await
    .ok_or("first origin registration failed")?;

    assert_eq!(providers.get_provider_connections_count(), 1);

    let handle_2 = providers
        .acquire_connection(&"provider_1".intern(), &addr_2, 0, ConnectionKind::Normal)
        .ok_or("second provider allocation missing")?;
    let managed_2 = ManagedProviderHandle::new(Arc::clone(&providers), handle_2);
    assert_eq!(providers.get_provider_connections_count(), 2);

    let pending_2 = SharedStreamManager::reserve_subscriber_cleanup(&connections, id_2, addr_2).await?;
    let (second_stream, _, _) = SharedStreamManager::register_shared_stream(
        super::super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        },
        url,
        futures::stream::pending::<Result<Bytes, std::io::Error>>(),
        &addr_2,
        id_2,
        Vec::new(),
        8,
        Some(managed_2),
        pending_2,
        0,
        ConnectionKind::Normal,
    )
    .await
    .ok_or("second subscriber join failed")?;

    assert_eq!(providers.get_provider_connections_count(), 1);

    drop(first_stream);
    drop(second_stream);
    timeout(Duration::from_secs(2), async {
        while providers.get_provider_connections_count() != 0 || !users.active_streams().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(providers.get_provider_connections_count(), 0);
    Ok(())
}

#[tokio::test]
async fn shutdown_rejects_waiting_admission_and_drains_owned_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (_providers, _users, _manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41015".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let initial_capacity = connections.cleanup_tx().capacity();
    let held_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
    let admission = connections.begin_admission().await.ok_or("admission unexpectedly closed")?;

    let shutdown = tokio::spawn({
        let connections = Arc::clone(&connections);
        async move { connections.shutdown().await }
    });
    timeout(Duration::from_secs(2), async {
        while !connections.is_shutting_down() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(connections.is_shutting_down());

    let res = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await;
    assert!(
        matches!(res, Err(ConnectionRejectionReason::CleanupReceiverClosed)),
        "waiting admission must be rejected after shutdown"
    );
    assert!(!shutdown.is_finished(), "shutdown must wait for an admitted registration");

    drop(held_cleanup);
    drop(admission);
    timeout(Duration::from_secs(2), shutdown).await??;
    assert_eq!(connections.cleanup_tx().capacity(), initial_capacity);
    Ok(())
}

#[tokio::test]
async fn shared_cold_miss_does_not_consume_cleanup_capacity_or_emit_cleanup() -> Result<(), Box<dyn std::error::Error>>
{
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41017".parse()?;
    let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let initial_capacity = connections.cleanup_tx().capacity();

    let ctx = super::super::SharedStreamCtx {
        app_config: &app_cfg,
        shared_stream_manager: &manager,
        active_provider: &providers,
        connection_manager: &connections,
    };

    let res = SharedStreamManager::subscribe_shared_stream(
        ctx,
        "https://example.invalid/live/non_existent.ts",
        &addr,
        id,
        0,
        ConnectionKind::Normal,
    )
    .await?;

    assert!(res.is_none());
    assert_eq!(connections.cleanup_tx().capacity(), initial_capacity, "cold miss must not consume cleanup capacity");
    Ok(())
}
