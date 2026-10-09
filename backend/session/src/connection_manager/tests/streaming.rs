use super::*;

#[tokio::test]
async fn enqueue_with_backpressure_bounds_overflow_buffer() {
    let (tx, mut rx) = mpsc::channel(1);
    let sender = BackpressureSender::new(tx.clone(), "test", 1);
    assert!(tx.send(1_u8).await.is_ok());

    sender.enqueue(2_u8);
    for _ in 0..50 {
        let ready = {
            let state = super::super::lock_backpressure_state(sender.state.as_ref());
            state.overflow.is_empty() && state.draining
        };
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    sender.enqueue(3_u8);
    sender.enqueue(4_u8);

    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(2));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(3));
    let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn close_connection_with_reason_returns_false_when_only_broadcast_receiver_exists() {
    let manager = create_test_connection_manager();
    let mut broadcast_rx = manager.get_close_connection_channel();
    let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());

    assert!(
        !manager.close_connection_with_reason(&addr, DisconnectReason::ClientKicked),
        "must return false when only broadcast receiver exists without registered target socket"
    );
    assert_eq!(
        broadcast_rx.recv().await.ok(),
        Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientKicked))
    );
}

#[tokio::test]
async fn close_connection_with_reason_returns_false_for_unrelated_addr_even_with_broadcast_receivers() {
    let manager = create_test_connection_manager();
    let _broadcast_rx = manager.get_close_connection_channel();
    let active_addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
    let unrelated_addr: SocketAddr = "127.0.0.1:5678".parse().unwrap_or_else(|_| unreachable!());

    let _close_rx = manager.register_close_socket(active_addr);

    assert!(
        !manager.close_connection_with_reason(&unrelated_addr, DisconnectReason::ClientKicked),
        "unrelated address must not report success just because another socket is registered"
    );
    assert!(
        manager.close_connection_with_reason(&active_addr, DisconnectReason::ClientKicked),
        "target address registered must report success"
    );
}

#[test]
fn test_provider_error_disconnect_maps_to_streaming_failure_stage() {
    assert_eq!(
        resolve_disconnect_failure_stage(
            &make_stream_info("some_provider", "Some Channel"),
            DisconnectReason::ProviderError,
            &DisconnectQos { first_byte_latency_ms: Some(150), ..Default::default() },
        ),
        Some(FailureStage::Streaming)
    );
}

#[test]
fn test_shared_provider_error_without_first_byte_stays_streaming_stage() {
    let mut info = make_stream_info("some_provider", "Some Channel");
    info.channel.shared = true;
    assert_eq!(
        resolve_disconnect_failure_stage(&info, DisconnectReason::ProviderError, &DisconnectQos::default(),),
        Some(FailureStage::Streaming)
    );
}
