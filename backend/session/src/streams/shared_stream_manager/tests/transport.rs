use super::*;

#[tokio::test]
async fn send_client_chunk_respects_progress_deadline_while_queue_is_full() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, _rx) = mpsc::channel::<BudgetedChunk>(1);
    let byte_budget = Arc::new(Semaphore::new(1024));
    let queued_permit = byte_budget.clone().acquire_many_owned(6).await?;
    tx.send(BudgetedChunk { bytes: Bytes::from_static(b"queued"), _permit: queued_permit }).await?;
    let cancel = CancellationToken::new();
    let deadline = Instant::now() + Duration::from_millis(50);
    let outcome = timeout(
        Duration::from_secs(1),
        send_client_chunk(&tx, Bytes::from_static(b"blocked"), &cancel, deadline, &byte_budget),
    )
    .await?;
    assert_eq!(outcome, SendOutcome::TimedOut);
    Ok(())
}

#[tokio::test]
async fn send_client_chunk_respects_byte_budget() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, _rx) = mpsc::channel::<BudgetedChunk>(16);
    // Byte budget of 4 bytes cannot accommodate a 6-byte chunk, so the acquire must
    // time out rather than exceed the budget.
    let byte_budget = Arc::new(Semaphore::new(4));
    let cancel = CancellationToken::new();
    let deadline = Instant::now() + Duration::from_millis(50);
    let outcome = timeout(
        Duration::from_secs(1),
        send_client_chunk(&tx, Bytes::from_static(b"123456"), &cancel, deadline, &byte_budget),
    )
    .await?;
    assert_eq!(outcome, SendOutcome::TimedOut);
    Ok(())
}

#[tokio::test]
async fn shared_timeout_ends_only_its_subscriber() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let (_, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41007".parse()?;
    let first = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let second = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url: Arc<str> = Arc::from("https://example.invalid/timeout.ts");
    register_user(&users, addr, first.stream_uid(), "same-user", &url).await?;
    register_user(&users, addr, second.stream_uid(), "same-user", &url).await?;
    let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    let surviving_token = CancellationToken::new();
    state.register_subscriber(first, &addr, CancellationToken::new()).await;
    state.register_subscriber(second, &addr, surviving_token.clone()).await;
    {
        let mut register = manager.shared_streams.write().await;
        register.by_key.insert(Arc::clone(&url), state);
        register.key_by_subscriber.insert(first, Arc::clone(&url));
        register.key_by_subscriber.insert(second, Arc::clone(&url));
    }
    let permit = connections.cleanup_tx().reserve_owned().await.ok();
    let mut response = super::super::ReceiverStreamWrapper {
        stream: futures::stream::pending::<Bytes>(),
        start: None,
        subscriber_id: first,
        addr,
        permit,
        request_id: None,
        owner: None,
        deadline: Some(Box::pin(tokio::time::sleep(Duration::ZERO))),
        released: false,
    };
    assert!(timeout(Duration::from_secs(2), response.next()).await?.is_none());
    timeout(Duration::from_secs(2), async {
        while users.active_streams().await.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(users.active_streams().await.first().map(|stream| stream.uid), Some(second.stream_uid()));
    assert!(!surviving_token.is_cancelled());
    assert!(manager.get_shared_state(&url).await.is_some());
    Ok(())
}

#[test]
fn test_burst_buffer_eviction_is_chunk_bounded_for_small_packets() {
    let max_chunks = 3;
    let mut buffer = BurstBuffer::new(MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES * max_chunks);
    for _ in 0..max_chunks.saturating_add(1) {
        buffer.push(Bytes::from_static(b"x"));
    }

    let mut chunks = Vec::new();
    let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

    assert_eq!(buffer.buffer.len(), max_chunks);
    assert_eq!(buffer.current_bytes, max_chunks);
    assert_eq!(read.skipped, 1);
    assert_eq!(chunks.len(), max_chunks);
}

#[test]
fn test_burst_buffer_keeps_oversized_packet_as_single_latest_chunk() {
    let mut buffer = BurstBuffer::new(4);
    buffer.push(Bytes::from_static(b"12345678"));

    let mut chunks = Vec::new();
    let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

    assert_eq!(buffer.buffer.len(), 1);
    assert_eq!(buffer.current_bytes, 8);
    assert_eq!(read.next_sequence, 1);
    assert_eq!(chunks.len(), 1);
}
