use super::{
    assert_missing_custom_video_terminates, create_active_client_stream, create_deferred_provider_grace_details,
    create_deferred_provider_open_future, create_test_app_state, create_test_connection_manager,
    create_test_fingerprint, create_test_stream_channel, create_test_user, custom_video_test_state, ActiveClientStream,
    ActiveClientStreamParams, ActiveClientStreamState, CustomVideoBuffers, DeferredProviderOpenOutcome,
    DeferredProviderOpenState, DirectBodyIdleTimeout, StreamMode, TimedStreamContext,
};
use crate::{
    api::model::{
        connection_manager::PROVIDER_END_NOT_SET, EventManager, ProviderContentRepresentationMode, StreamDetails,
        StreamError,
    },
    auth::Fingerprint,
    model::GracePeriodOptions,
};
use axum::http::HeaderMap;
use bytes::Bytes;
use futures::{pin_mut, StreamExt};
use shared::{
    model::{PlaylistItemType, UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{
    sync::{atomic::AtomicU8, Arc},
    time::Duration,
};

#[tokio::test]
async fn custom_response_modes_preserve_ts_fallback_and_terminate_hls_resources(
) -> Result<(), Box<dyn std::error::Error>> {
    use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;
    use tuliprox_session::stream_options::StreamResponseMode;

    let mut packet = [0xff_u8; 188];
    packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    let fallback = TransportStreamBuffer::new(packet.repeat(8));
    for mode in [
        StreamMode::UserExhausted,
        StreamMode::ProviderExhausted,
        StreamMode::ChannelUnavailable,
        StreamMode::Provisioning,
        StreamMode::LowPriorityPreempted,
    ] {
        for response_mode in [StreamResponseMode::Stream, StreamResponseMode::HlsResource] {
            let mut state = custom_video_test_state(mode, true);
            state.response_mode = response_mode;
            state.custom_video = CustomVideoBuffers {
                user_exhausted: Some(fallback.clone()),
                provider_exhausted: Some(fallback.clone()),
                unavailable: Some(fallback.clone()),
                provisioning: Some(fallback.clone()),
                low_priority_preempted: Some(fallback.clone()),
            };
            let mut stream = ActiveClientStream { state };
            let chunk = tokio::time::timeout(Duration::from_secs(1), stream.next()).await?;
            if response_mode == StreamResponseMode::Stream {
                let bytes = chunk.ok_or("TS fallback missing")??;
                assert_eq!(bytes.first(), Some(&0x47), "{mode:?}");
            } else {
                assert!(chunk.is_none(), "HLS must terminate without TS bytes in {mode:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn test_stream_mode_byte_values_match_the_implicit_discriminants() {
    // Discriminants are implicit (declaration order) except the 255 sentinel, so
    // this pins the wire mapping against accidental reordering.
    assert_eq!(StreamMode::Inner as u8, 0);
    assert_eq!(StreamMode::UserExhausted as u8, 1);
    assert_eq!(StreamMode::ProviderExhausted as u8, 2);
    assert_eq!(StreamMode::ChannelUnavailable as u8, 3);
    assert_eq!(StreamMode::Provisioning as u8, 4);
    assert_eq!(StreamMode::LowPriorityPreempted as u8, 5);
    assert_eq!(StreamMode::ReentrySuppressed as u8, 6);
    assert_eq!(StreamMode::GracePending as u8, 255);
}

#[test]
fn test_stream_mode_try_from_rejects_unknown_values() {
    assert_eq!(StreamMode::try_from(0), Ok(StreamMode::Inner));
    assert_eq!(StreamMode::try_from(6), Ok(StreamMode::ReentrySuppressed));
    assert_eq!(StreamMode::try_from(255), Ok(StreamMode::GracePending));
    // Unknown values must not be silently mapped onto GracePending.
    assert_eq!(StreamMode::try_from(7), Err(7));
    assert_eq!(StreamMode::try_from(200), Err(200));
}

#[tokio::test]
async fn test_user_exhausted_without_custom_video_terminates_immediately() {
    assert_missing_custom_video_terminates(StreamMode::UserExhausted, false).await;
}

#[tokio::test]
async fn test_provider_exhausted_without_custom_video_terminates_immediately() {
    assert_missing_custom_video_terminates(StreamMode::ProviderExhausted, false).await;
}

#[tokio::test]
async fn test_channel_unavailable_without_custom_video_terminates_immediately() {
    assert_missing_custom_video_terminates(StreamMode::ChannelUnavailable, false).await;
}

#[tokio::test]
async fn deferred_provider_open_forces_identity_when_original_mode_preserves_origin() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let addr = "127.0.0.1:55015".parse().unwrap();
    let provider_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(&provider_name, &addr, true, 0, crate::api::model::ConnectionKind::Normal)
        .expect("deferred provider allocation");
    let stream_details = create_deferred_provider_grace_details(
        &provider_name,
        tuliprox_session::ManagedProviderHandle::new(
            Arc::clone(&app_state.connection_manager.provider_manager),
            provider_handle.clone(),
        ),
    );
    assert_eq!(stream_details.content_representation, ProviderContentRepresentationMode::PreserveOrigin);
    let fingerprint = create_test_fingerprint(addr);
    let mut stream_channel = create_test_stream_channel(1, "http://provider-1.example/live/1");
    stream_channel.item_type = PlaylistItemType::Catchup;

    let deferred = create_deferred_provider_open_future(
        &app_state,
        &stream_details,
        &fingerprint,
        &stream_channel,
        &HeaderMap::new(),
    )
    .expect("deferred provider open context");

    let DeferredProviderOpenState::Pending(context) = deferred else { panic!("new deferred open must start pending") };
    assert_eq!(
        context.provider_stream_factory_options.content_representation(),
        ProviderContentRepresentationMode::Identity
    );
    assert!(!context.provider_stream_factory_options.response_head_is_available());
    drop(context);
    app_state.connection_manager.release_provider_handle(Some(provider_handle));
}

#[tokio::test(start_paused = true)]
async fn test_active_client_stream_deferred_provider_open_applies_sleep_timer_timeout() {
    let app_state = create_test_app_state();
    let connection_manager = create_test_connection_manager();
    let addr = "127.0.0.1:55020".parse().unwrap_or_else(|_| unreachable!());
    let state = ActiveClientStreamState {
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        inner: None,
        send_custom_stream_flag: Some(Arc::new(AtomicU8::new(StreamMode::Inner as u8))),
        provider_handle: None,
        deferred_provider_open: Some(DeferredProviderOpenState::Opening(Box::pin(async {
            DeferredProviderOpenOutcome::Stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed())
        }))),
        timed_stream_context: Some(TimedStreamContext { app_state, duration_secs: 1, virtual_id: VirtualId::new(1) }),
        preempt_cancelled: None,
        grace_task_handle: None,
        provisioning_stop_signal: None,
        provisionable: false,
        custom_video: CustomVideoBuffers {
            user_exhausted: None,
            provider_exhausted: None,
            unavailable: None,
            provisioning: None,
            low_priority_preempted: None,
        },
        meter: None,
        event_manager: Arc::new(EventManager::new()),
        waker: None,
        connection_manager,
        fingerprint: Arc::new(Fingerprint::new("fp-timeout".to_string(), "127.0.0.1".to_string(), addr)),
        stream_uid: None,
        provider_stopped: false,
        user_stream_released: true,
        provider_handle_released: true,
        custom_video_timeout_secs: 0,
        custom_video_timeout_mode: None,
        custom_video_timeout_sleep: None,
        direct_body_idle_timeout: DirectBodyIdleTimeout::disabled(),
        provider_end_reason: AtomicU8::new(PROVIDER_END_NOT_SET),
        provider_error_class: None,
        provider_http_status: None,
        provider_reconnect_count: AtomicU8::new(0),
        lease_owner: None,
        media_started: None,
        lease_confirmed: false,
        lease_request_id: None,
        request_cleanup: None,
    };
    let stream = ActiveClientStream { state };
    pin_mut!(stream);

    assert!(
        matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
        "deferred-open success should first install the wrapped upstream stream and park pending"
    );

    tokio::time::advance(Duration::from_secs(2)).await;

    let result = tokio::time::timeout(Duration::from_millis(1), stream.next()).await;
    assert!(result.is_ok(), "deferred-open stream should stop once the configured sleep timer expires");
    match result {
        Ok(joined) => {
            assert!(joined.is_none(), "sleep timer should terminate the deferred-open stream without yielding bytes");
        }
        Err(_) => unreachable!("timeout already checked"),
    }
}

#[tokio::test(start_paused = true)]
async fn test_active_client_stream_immediate_provider_stream_emits_meter_batches() {
    let app_state = create_test_app_state();
    let mut meter_events = app_state.event_manager.get_meter_channel();
    app_state.event_manager.stream_meter_subscriber_connected();

    let addr = "127.0.0.1:55030".parse().unwrap_or_else(|_| unreachable!());
    let test_user = create_test_user("meter-user");
    let test_fingerprint = create_test_fingerprint(addr);
    let provider_stream = futures::stream::iter(vec![Ok::<Bytes, StreamError>(Bytes::from_static(&[0_u8; 3072]))])
        .chain(futures::stream::pending())
        .boxed();
    let stream_details = StreamDetails::from_stream(provider_stream, GracePeriodOptions::default());

    let stream = create_active_client_stream(ActiveClientStreamParams {
        stream_details,
        app_state: &app_state,
        user: &test_user,
        connection_permission: UserConnectionPermission::Allowed,
        connection_kind: crate::api::model::ConnectionKind::Normal,
        fingerprint: &test_fingerprint,
        stream_channel: create_test_stream_channel(1, "http://provider-1.example/live/1"),
        socket_bound: true,
        session_token: None,
        req_headers: &HeaderMap::default(),
        meter_uid: 55,
        meter_stream: true,
    })
    .await
    .expect("metered test stream admission should succeed");
    pin_mut!(stream);

    let first_chunk = stream.next().await;
    assert!(
        matches!(first_chunk, Some(Ok(ref bytes)) if bytes.len() == 3072),
        "immediate provider stream should yield the metered payload chunk"
    );

    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;

    let entries = tokio::time::timeout(Duration::from_millis(1), meter_events.recv())
        .await
        .expect("immediate provider stream should publish a meter batch after bytes are sent")
        .expect("meter channel should stay open while the stream is active");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].meter_uid, 55);
    assert_eq!(entries[0].uids, vec![1]);
    assert_eq!(entries[0].rate_kbps, 1);
    assert_eq!(entries[0].total_kb, 3);
}
