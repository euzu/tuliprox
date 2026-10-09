use super::*;

#[tokio::test]
async fn subscribers_on_same_socket_do_not_replace_each_other() -> Result<(), Box<dyn std::error::Error>> {
    let state = SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None);
    let addr = "127.0.0.1:41003".parse()?;
    let first = CancellationToken::new();
    let second = CancellationToken::new();
    state.register_subscriber(SharedSubscriberId::from_stream_uid(1), &addr, first.clone()).await;
    state.register_subscriber(SharedSubscriberId::from_stream_uid(2), &addr, second.clone()).await;
    assert_eq!(state.subscribers.read().await.len(), 2);
    assert!(!first.is_cancelled());
    assert!(!second.is_cancelled());
    Ok(())
}

#[tokio::test]
async fn old_origin_cannot_unregister_replacement() {
    let app_cfg = create_test_app_config();
    let events = Arc::new(EventManager::new());
    let (_, _, manager, _) = create_test_connection_manager(&app_cfg, &events);
    let url = "https://example.invalid/live/replaced.ts";
    let old = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    let current = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    manager.shared_streams.write().await.by_key.insert(Arc::from(url), Arc::clone(&current));
    manager.unregister(url, &old).await;
    assert!(manager.get_shared_state(url).await.is_some_and(|state| Arc::ptr_eq(&state, &current)));
    assert!(!current.stop_token.is_cancelled());
}

#[test]
fn test_burst_buffer_eviction_is_byte_bounded() {
    let mut buffer = BurstBuffer::new(10);
    buffer.push(Bytes::from_static(b"12345"));
    buffer.push(Bytes::from_static(b"67890"));
    buffer.push(Bytes::from_static(b"abcde"));

    let mut chunks = Vec::new();
    let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

    assert_eq!(buffer.current_bytes, 10);
    assert_eq!(read.skipped, 1);
    assert_eq!(read.next_sequence, 3);
    assert_eq!(chunks.len(), 2);
}

#[test]
fn test_burst_buffer_reads_clone_bytes_without_copying_payload() {
    let mut buffer = BurstBuffer::new(1024);
    let chunk = Bytes::from(vec![1_u8, 2, 3, 4]);
    let ptr = chunk.as_ptr();
    buffer.push(chunk);

    let mut chunks = Vec::new();
    let _read = buffer.read_from_into(0, &mut chunks, usize::MAX);
    let Some(read_chunk) = chunks.first() else {
        panic!("expected one buffered chunk");
    };

    assert_eq!(read_chunk.as_ptr(), ptr);
}

#[test]
fn test_burst_buffer_live_batch_is_chunk_bounded() {
    let mut buffer = BurstBuffer::new(4096);
    for _ in 0..10 {
        buffer.push(Bytes::from_static(b"x"));
    }

    let mut chunks = Vec::new();
    let first = buffer.read_from_into(0, &mut chunks, 4);
    assert_eq!(chunks.len(), 4);
    assert_eq!(first.next_sequence, 4);
    assert_eq!(first.skipped, 0);

    let second = buffer.read_from_into(first.next_sequence, &mut chunks, 4);
    assert_eq!(chunks.len(), 4);
    assert_eq!(second.next_sequence, 8);

    let third = buffer.read_from_into(second.next_sequence, &mut chunks, 4);
    assert_eq!(chunks.len(), 2);
    assert_eq!(third.next_sequence, 10);
}

#[tokio::test]
async fn broadcast_does_not_emit_shared_stream_health_events() {
    let app_cfg = create_test_app_config();
    let event_manager = Arc::new(EventManager::new());
    let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));
    let mut events = event_manager.get_event_channel();
    let stream_url = "https://user:pass@example.invalid/live/health.ts";
    let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));
    {
        let mut reg = shared_manager.shared_streams.write().await;
        reg.by_key.insert(Arc::from(stream_url), Arc::clone(&state));
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    state.broadcast(stream_url, ReceiverStream::new(rx), Arc::clone(&shared_manager));

    assert!(
        timeout(Duration::from_millis(100), events.recv()).await.is_err(),
        "broadcast startup must not emit runtime events"
    );

    // Pushing data must also not emit a per-chunk health event.
    tx.send(Ok(Bytes::from_static(b"payload-1"))).await.unwrap_or_else(|_| panic!("send chunk should succeed"));
    assert!(
        timeout(Duration::from_millis(100), events.recv()).await.is_err(),
        "pushing a chunk must not emit runtime events"
    );

    drop(tx);
    let ended = timeout(Duration::from_secs(2), async {
        loop {
            if shared_manager.get_shared_state(stream_url).await.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(ended.is_ok(), "broadcast must exit after source end");
    assert!(
        timeout(Duration::from_millis(100), events.recv()).await.is_err(),
        "broadcast shutdown must not emit runtime events"
    );
}

#[tokio::test]
async fn pending_join_keeps_origin_alive_when_last_subscriber_leaves() -> Result<(), Box<dyn std::error::Error>> {
    let app_cfg = Arc::new(create_test_app_config());
    let events = Arc::new(EventManager::new());
    let (_providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
    let addr = "127.0.0.1:41019".parse()?;
    let first_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
    let url = "https://example.invalid/live/pending_join.ts";

    let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
    state.register_subscriber(first_id, &addr, CancellationToken::new()).await;
    {
        let mut reg = manager.shared_streams.write().await;
        reg.by_key.insert(Arc::from(url), Arc::clone(&state));
        reg.key_by_subscriber.insert(first_id, Arc::from(url));
    }

    // Simulate a join that committed its registry entry but has not yet registered
    // its subscriber in the origin.
    let pending_join = super::super::PendingJoinGuard::new(&state);
    assert!(state.has_pending_joins());

    // The last (and only) subscriber leaves; the origin must not be torn down because
    // the pending join is about to land on it.
    manager.release_subscriber(first_id).await;
    assert!(manager.get_shared_state(url).await.is_some(), "origin must survive a pending join");

    // The join completes; the origin has no subscribers and no pending joins.
    pending_join.commit();
    assert!(!state.has_pending_joins());
    assert!(state.subscribers.read().await.is_empty(), "origin has no subscribers after release");
    Ok(())
}
