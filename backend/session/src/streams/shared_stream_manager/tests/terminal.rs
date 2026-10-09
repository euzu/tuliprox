use super::*;

#[tokio::test]
async fn test_preempted_shared_subscriber_switches_to_low_priority_fallback() {
    let app_cfg = create_test_app_config();
    let event_manager = Arc::new(EventManager::new());
    let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let geoip = Arc::new(ArcSwapOption::default());
    let user_manager = Arc::new(ActiveUserManager::new(&Config::default(), &geoip, &event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
    let connection_manager =
        Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, &event_manager, None));

    let addr: SocketAddr = "127.0.0.1:43001".parse().unwrap_or_else(|_| unreachable!());
    let mut ts_packet = vec![0_u8; 188];
    ts_packet[0] = 0x47;

    let low_priority_fallback = tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer::new(ts_packet);
    let state =
        Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, Some(low_priority_fallback)));

    let subscriber_id = SharedSubscriberId::from_stream_uid(1);
    let Ok(pending_cleanup) =
        SharedStreamManager::reserve_subscriber_cleanup(&connection_manager, subscriber_id, addr).await
    else {
        panic!("shared subscriber admission failed");
    };
    let (mut stream, _provider, _capability) = state
        .subscribe(
            &addr,
            subscriber_id,
            connection_manager,
            pending_cleanup,
            super::super::PendingJoinGuard::new(&state),
        )
        .await;

    state.preempted_token.cancel();
    drop(state);

    let first = timeout(Duration::from_secs(2), stream.next()).await;
    let Ok(maybe_chunk) = first else { panic!("timed out waiting for fallback chunk") };
    let chunk = match maybe_chunk {
        Some(Ok(bytes)) => bytes,
        Some(Err(err)) => panic!("fallback stream returned error: {err}"),
        None => panic!("fallback stream ended unexpectedly"),
    };
    assert!(!chunk.is_empty(), "fallback chunk must contain MPEG-TS bytes");
}
