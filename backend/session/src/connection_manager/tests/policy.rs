use super::*;

#[test]
fn socket_activity_tracker_coalesces_updates_per_socket() {
    let tracker = SocketActivityTracker::new();
    let addr_one: SocketAddr = "127.0.0.1:3234".parse().unwrap_or_else(|_| unreachable!());
    let addr_two: SocketAddr = "127.0.0.1:3235".parse().unwrap_or_else(|_| unreachable!());

    tracker.track(SocketActivityEvent::HttpActivity { addr: addr_one });
    tracker.track(SocketActivityEvent::HttpActivity { addr: addr_one });
    tracker.track(SocketActivityEvent::DirectBodyActivity { addr: addr_two });

    let pending = tracker.drain();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|event| matches!(
        event,
        SocketActivityEvent::HttpActivity { addr } if *addr == addr_one
    )));
    assert!(pending.iter().any(|event| matches!(
        event,
        SocketActivityEvent::DirectBodyActivity { addr } if *addr == addr_two
    )));
}

#[tokio::test]
async fn kick_connection_sends_kick_close_signal() {
    let manager = create_test_connection_manager();
    let mut rx = manager.get_close_connection_channel();
    let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
    let _close_rx = manager.register_close_socket(addr);

    assert!(manager.kick_connection(&addr, shared::model::VirtualId::new(1), 0).await);
    assert_eq!(rx.recv().await.ok(), Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientKicked)));
}

#[tokio::test]
async fn kick_connection_returns_false_without_receivers() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:1235".parse().unwrap_or_else(|_| unreachable!());
    assert!(!manager.kick_connection(&addr, shared::model::VirtualId::new(1), 0).await);
}

#[tokio::test]
async fn close_connection_signal_sends_generic_close_signal() {
    let manager = create_test_connection_manager();
    let mut rx = manager.get_close_connection_channel();
    let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
    let _close_rx = manager.register_close_socket(addr);

    assert!(manager.close_connection_signal(&addr));
    assert_eq!(rx.recv().await.ok(), Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientClosed)));
}

#[tokio::test]
async fn provisioning_close_connection_sends_provisioning_signal() {
    let manager = create_test_connection_manager();
    let mut rx = manager.get_close_connection_channel();
    let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
    let _close_rx = manager.register_close_socket(addr);

    assert!(
        manager
            .close_connection_with_reason_and_block(
                &addr,
                shared::model::VirtualId::new(7),
                0,
                DisconnectReason::Provisioning
            )
            .await
    );
    assert_eq!(rx.recv().await.ok(), Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::Provisioning)));
}

#[test]
fn test_client_closed_when_no_provider_end() {
    let info = make_stream_info("some_provider", "Some Channel");
    let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
    assert_eq!(reason, DisconnectReason::ClientClosed);
}

#[test]
fn test_client_kicked_disconnect_has_no_failure_stage() {
    assert_eq!(
        resolve_disconnect_failure_stage(
            &make_stream_info("some_provider", "Some Channel"),
            DisconnectReason::ClientKicked,
            &DisconnectQos::default(),
        ),
        None
    );
}

#[test]
fn test_provisioning_disconnect_has_no_failure_stage() {
    assert_eq!(
        resolve_disconnect_failure_stage(
            &make_stream_info("some_provider", "Some Channel"),
            DisconnectReason::Provisioning,
            &DisconnectQos::default(),
        ),
        None
    );
}

#[test]
fn test_provider_closed_on_eof() {
    let info = make_stream_info("some_provider", "Some Channel");
    let reason = resolve_disconnect_reason(PROVIDER_END_CLOSED, &info);
    assert_eq!(reason, DisconnectReason::ProviderClosed);
}

#[test]
fn test_provider_error_on_err() {
    let info = make_stream_info("some_provider", "Some Channel");
    let reason = resolve_disconnect_reason(PROVIDER_END_ERROR, &info);
    assert_eq!(reason, DisconnectReason::ProviderError);
}

#[test]
fn test_unknown_tuliprox_title_falls_through_to_atomic() {
    let info = make_stream_info("tuliprox", "some_unknown_video_type");
    let reason = resolve_disconnect_reason(PROVIDER_END_CLOSED, &info);
    assert_eq!(reason, DisconnectReason::ProviderClosed);
}

#[test]
fn test_session_expired_disconnect_maps_to_session_reconnect_stage() {
    assert_eq!(
        resolve_disconnect_failure_stage(
            &make_stream_info("some_provider", "Some Channel"),
            DisconnectReason::SessionExpired,
            &DisconnectQos::default(),
        ),
        Some(FailureStage::SessionReconnect)
    );
}

#[test]
fn test_provider_error_without_first_byte_maps_to_first_byte_stage() {
    assert_eq!(
        resolve_disconnect_failure_stage(
            &make_stream_info("some_provider", "Some Channel"),
            DisconnectReason::ProviderError,
            &DisconnectQos::default(),
        ),
        Some(FailureStage::FirstByte)
    );
}
