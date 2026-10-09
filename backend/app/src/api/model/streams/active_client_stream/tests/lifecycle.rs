use super::{
    create_active_client_stream, create_test_app_state, create_test_fingerprint, create_test_stream_channel,
    create_test_user, should_use_direct_body_idle_timeout, ActiveClientStreamParams, BufferedStream,
};
use crate::{
    api::model::{
        AppState, BoxedProviderStream, ProviderHandle, StreamDetails, StreamError, DIRECT_BODY_IDLE_TIMEOUT_SECS,
    },
    model::GracePeriodOptions,
};
use axum::{body::Body, http::HeaderMap};
use bytes::Bytes;
use futures::{pin_mut, StreamExt};
use http_body_util::BodyExt;
use shared::{
    model::{PlaylistItemType, StreamChannel, UserConnectionPermission, XtreamCluster},
    utils::Internable,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{oneshot, Notify};
use tokio_util::sync::CancellationToken;

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_video_stream_channel(
    virtual_id: u32,
    url: &str,
) -> StreamChannel {
    let mut channel = create_test_stream_channel(virtual_id, url);
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    channel.group = "Movies".intern();
    channel.url = url.into();
    channel
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_series_stream_channel(
    virtual_id: u32,
    url: &str,
) -> StreamChannel {
    let mut channel = create_test_stream_channel(virtual_id, url);
    channel.item_type = PlaylistItemType::Series;
    channel.cluster = XtreamCluster::Series;
    channel.group = "Series".intern();
    channel.url = url.into();
    channel
}

#[derive(Clone, Default)]
pub(in crate::api::model::streams::active_client_stream::tests) struct DropTracker(
    pub(in crate::api::model::streams::active_client_stream::tests) Arc<AtomicBool>,
);

impl DropTracker {
    pub(in crate::api::model::streams::active_client_stream::tests) fn is_dropped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub(in crate::api::model::streams::active_client_stream::tests) struct DropTrackedProviderStream {
    pub(in crate::api::model::streams::active_client_stream::tests) inner: BoxedProviderStream,
    pub(in crate::api::model::streams::active_client_stream::tests) tracker: DropTracker,
}

impl futures::Stream for DropTrackedProviderStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for DropTrackedProviderStream {
    fn drop(&mut self) { self.tracker.0.store(true, Ordering::Release); }
}

pub(in crate::api::model::streams::active_client_stream::tests) fn track_provider_stream(
    stream: BoxedProviderStream,
) -> (BoxedProviderStream, DropTracker) {
    let tracker = DropTracker::default();
    let tracked_stream = DropTrackedProviderStream { inner: stream, tracker: tracker.clone() }.boxed();
    (tracked_stream, tracker)
}

pub(in crate::api::model::streams::active_client_stream::tests) struct TestDirectStreamParams<'a> {
    pub(in crate::api::model::streams::active_client_stream::tests) app_state: &'a Arc<AppState>,
    pub(in crate::api::model::streams::active_client_stream::tests) username: &'a str,
    pub(in crate::api::model::streams::active_client_stream::tests) max_connections: u32,
    pub(in crate::api::model::streams::active_client_stream::tests) addr: SocketAddr,
    pub(in crate::api::model::streams::active_client_stream::tests) stream_channel: StreamChannel,
    pub(in crate::api::model::streams::active_client_stream::tests) provider_stream: BoxedProviderStream,
    pub(in crate::api::model::streams::active_client_stream::tests) provider_handle: Option<ProviderHandle>,
}

pub(in crate::api::model::streams::active_client_stream::tests) struct TestDirectStream {
    pub(in crate::api::model::streams::active_client_stream::tests) stream: BoxedProviderStream,
    pub(in crate::api::model::streams::active_client_stream::tests) uid: u32,
}

pub(in crate::api::model::streams::active_client_stream::tests) async fn create_test_active_direct_stream(
    params: TestDirectStreamParams<'_>,
) -> TestDirectStream {
    let TestDirectStreamParams {
        app_state,
        username,
        max_connections,
        addr,
        stream_channel,
        provider_stream,
        provider_handle,
    } = params;
    let mut user = create_test_user(username);
    user.max_connections = max_connections;
    let fingerprint = create_test_fingerprint(addr);
    let mut stream_details = StreamDetails::from_stream(provider_stream, GracePeriodOptions::default());
    if provider_handle.is_some() {
        stream_details.provider_name = Some("provider_1".intern());
    }
    stream_details.provider_handle = provider_handle.map(|handle| {
        tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.connection_manager.provider_manager), handle)
    });
    let virtual_id = stream_channel.virtual_id;

    let stream = create_active_client_stream(ActiveClientStreamParams {
        stream_details,
        app_state,
        user: &user,
        connection_permission: UserConnectionPermission::Allowed,
        connection_kind: crate::api::model::ConnectionKind::Normal,
        fingerprint: &fingerprint,
        stream_channel,
        socket_bound: false,
        session_token: None,
        req_headers: &HeaderMap::default(),
        meter_uid: 0,
        meter_stream: false,
    })
    .await
    .expect("direct test stream admission should succeed");
    let uid = app_state
        .active_users
        .active_streams()
        .await
        .into_iter()
        .find(|active| active.username == username && active.addr == addr && active.channel.virtual_id == virtual_id)
        .map(|active| active.uid)
        .expect("direct test stream should be registered");

    TestDirectStream { stream, uid }
}

pub(in crate::api::model::streams::active_client_stream::tests) fn acquire_test_provider_handle(
    app_state: &Arc<AppState>,
    addr: SocketAddr,
) -> ProviderHandle {
    app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &"provider_1".intern(),
            &addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("direct test stream should acquire the provider slot")
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::api::model::streams::active_client_stream::tests) struct TestLifecycleSnapshot {
    pub(in crate::api::model::streams::active_client_stream::tests) active_counts: (usize, usize),
    pub(in crate::api::model::streams::active_client_stream::tests) stream_uids: Vec<u32>,
    pub(in crate::api::model::streams::active_client_stream::tests) provider_connections: usize,
}

pub(in crate::api::model::streams::active_client_stream::tests) async fn lifecycle_snapshot(
    app_state: &Arc<AppState>,
) -> TestLifecycleSnapshot {
    let active_counts = app_state.active_users.active_users_and_connections().await;
    let mut stream_uids =
        app_state.active_users.active_streams().await.into_iter().map(|stream| stream.uid).collect::<Vec<_>>();
    stream_uids.sort_unstable();
    TestLifecycleSnapshot {
        active_counts,
        stream_uids,
        provider_connections: app_state.active_provider.get_provider_connections_count(),
    }
}

pub(in crate::api::model::streams::active_client_stream::tests) struct ExpectedLifecycle<'a> {
    pub(in crate::api::model::streams::active_client_stream::tests) description: &'static str,
    pub(in crate::api::model::streams::active_client_stream::tests) active_counts: (usize, usize),
    pub(in crate::api::model::streams::active_client_stream::tests) stream_uids: &'a [u32],
    pub(in crate::api::model::streams::active_client_stream::tests) provider_connections: usize,
    pub(in crate::api::model::streams::active_client_stream::tests) dropped_streams: &'a [&'a DropTracker],
}

pub(in crate::api::model::streams::active_client_stream::tests) async fn wait_for_lifecycle(
    app_state: &Arc<AppState>,
    expected: ExpectedLifecycle<'_>,
) {
    let mut expected_stream_uids = expected.stream_uids.to_vec();
    expected_stream_uids.sort_unstable();
    let expected_snapshot = TestLifecycleSnapshot {
        active_counts: expected.active_counts,
        stream_uids: expected_stream_uids,
        provider_connections: expected.provider_connections,
    };
    let completed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = lifecycle_snapshot(app_state).await;
            if snapshot == expected_snapshot && expected.dropped_streams.iter().all(|tracker| tracker.is_dropped()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

    if completed.is_err() {
        let snapshot = lifecycle_snapshot(app_state).await;
        let dropped = expected.dropped_streams.iter().map(|tracker| tracker.is_dropped()).collect::<Vec<_>>();
        panic!(
            "{} did not converge: expected={expected_snapshot:?}, actual={snapshot:?}, dropped={dropped:?}",
            expected.description
        );
    }
}

#[test]
fn test_direct_body_idle_timeout_only_applies_to_direct_vod_series_streams() {
    let video = create_test_video_stream_channel(1, "http://provider-1.example/movie/1.mkv");
    assert!(should_use_direct_body_idle_timeout(&video));

    let mut series = create_test_video_stream_channel(2, "http://provider-1.example/series/2.mkv");
    series.item_type = PlaylistItemType::Series;
    series.cluster = XtreamCluster::Series;
    assert!(should_use_direct_body_idle_timeout(&series));

    let live = create_test_stream_channel(3, "http://provider-1.example/live/3.ts");
    assert!(!should_use_direct_body_idle_timeout(&live));

    let shared_video = {
        let mut channel = video.clone();
        channel.shared = true;
        channel
    };
    assert!(!should_use_direct_body_idle_timeout(&shared_video));
}

#[tokio::test]
async fn direct_series_normal_eof_releases_full_lifecycle() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55031".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let (provider_stream, tracker) =
        track_provider_stream(futures::stream::once(async { Ok(Bytes::from_static(b"series-eof")) }).boxed());
    let direct = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-eof-user",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(1, "http://provider-1.example/series/1.mkv"),
        provider_stream,
        provider_handle: Some(provider_handle),
    })
    .await;
    let active_uids = [direct.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "registered Series EOF stream",
            active_counts: (1, 1),
            stream_uids: &active_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;
    assert!(!tracker.is_dropped());

    let body = Body::from_stream(direct.stream);
    let collected = body.collect().await.expect("normal provider EOF should complete the response body");
    assert_eq!(collected.to_bytes(), Bytes::from_static(b"series-eof"));

    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "normal Series EOF cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&tracker],
        },
    )
    .await;
}

#[tokio::test]
async fn direct_series_upstream_error_after_registration_releases_full_lifecycle() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55032".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let provider_items = vec![
        Ok(Bytes::from_static(b"before-error")),
        Err(StreamError::Stream("controlled upstream failure".to_string())),
    ];
    let (provider_stream, tracker) = track_provider_stream(futures::stream::iter(provider_items).boxed());
    let direct = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-error-user",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(2, "http://provider-1.example/series/2.mkv"),
        provider_stream,
        provider_handle: Some(provider_handle),
    })
    .await;
    let active_uids = [direct.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "registered Series error stream",
            active_counts: (1, 1),
            stream_uids: &active_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;

    let body = Body::from_stream(direct.stream);
    let collected = body
        .collect()
        .await
        .expect("ActiveClientStream should convert the controlled upstream error into stream termination");
    assert_eq!(collected.to_bytes(), Bytes::from_static(b"before-error"));

    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "Series upstream error cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&tracker],
        },
    )
    .await;
}

#[tokio::test]
async fn closed_buffered_consumer_releases_direct_series_full_lifecycle() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55039".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let (gate_tx, gate_rx) = oneshot::channel();
    let gated_provider = futures::stream::once(async move {
        gate_rx.await.expect("test should release the upstream chunk");
        Ok(Bytes::from_static(b"after-consumer-drop"))
    })
    .chain(futures::stream::pending())
    .boxed();
    let (tracked_provider, tracker) = track_provider_stream(gated_provider);
    let producer_cancel = CancellationToken::new();
    let buffered_provider =
        BufferedStream::new(tracked_provider, 1, 0, producer_cancel.clone(), "controlled-test-stream").boxed();
    let direct = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-closed-consumer-user",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(10, "http://provider-1.example/series/10.mkv"),
        provider_stream: buffered_provider,
        provider_handle: Some(provider_handle),
    })
    .await;
    let active_uids = [direct.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "registered buffered Series stream",
            active_counts: (1, 1),
            stream_uids: &active_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;

    drop(Body::from_stream(direct.stream));
    gate_tx.send(()).expect("buffered producer should still own the gated upstream");

    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "closed buffered Series consumer cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&tracker],
        },
    )
    .await;
    assert!(producer_cancel.is_cancelled(), "closed consumer must cancel the buffered producer");
}

#[tokio::test]
async fn aborting_task_polling_direct_series_body_releases_full_lifecycle() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55033".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let (provider_stream, tracker) =
        track_provider_stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed());
    let direct = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-abort-user",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(3, "http://provider-1.example/series/3.mkv"),
        provider_stream,
        provider_handle: Some(provider_handle),
    })
    .await;
    let active_uids = [direct.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "registered Series task-abort stream",
            active_counts: (1, 1),
            stream_uids: &active_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;

    let started = Arc::new(Notify::new());
    let started_in_task = Arc::clone(&started);
    let task = tokio::spawn(async move {
        let mut body = Body::from_stream(direct.stream);
        started_in_task.notify_one();
        let _ = body.frame().await;
    });
    tokio::time::timeout(Duration::from_secs(1), started.notified()).await.expect("body polling task should start");
    task.abort();
    let join_error = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("aborted body polling task should finish")
        .expect_err("pending body polling task should be cancelled");
    assert!(join_error.is_cancelled());

    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "aborted Series body task cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&tracker],
        },
    )
    .await;
}

#[tokio::test]
async fn response_builder_error_after_registration_drops_direct_series_body() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55034".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let (provider_stream, tracker) =
        track_provider_stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed());
    let direct = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-builder-error-user",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(4, "http://provider-1.example/series/4.mkv"),
        provider_stream,
        provider_handle: Some(provider_handle),
    })
    .await;
    let active_uids = [direct.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "registered Series response-builder stream",
            active_counts: (1, 1),
            stream_uids: &active_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;

    let response = axum::response::Response::builder().status(10_000_u16).body(Body::from_stream(direct.stream));
    assert!(response.is_err(), "invalid status should fail response construction");

    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "response-builder error cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&tracker],
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn test_direct_vod_body_idle_timeout_releases_active_stream() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55035".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let pending_provider =
        futures::stream::once(async { Ok(Bytes::from_static(b"vod")) }).chain(futures::stream::pending()).boxed();
    let (provider_stream, tracker) = track_provider_stream(pending_provider);
    let direct = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "vod-user",
        max_connections: 1,
        addr,
        stream_channel: create_test_video_stream_channel(5, "http://provider-1.example/movie/5.mkv"),
        provider_stream,
        provider_handle: Some(provider_handle),
    })
    .await;
    let active_uids = [direct.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "registered idle-timeout VOD stream",
            active_counts: (1, 1),
            stream_uids: &active_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;
    let stream = direct.stream;
    pin_mut!(stream);

    let first_chunk = stream.next().await;
    assert!(matches!(first_chunk, Some(Ok(ref bytes)) if bytes.as_ref() == b"vod"));
    assert!(
        matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
        "pending VOD body should wait until the direct body idle timeout elapses"
    );

    tokio::time::advance(Duration::from_secs(DIRECT_BODY_IDLE_TIMEOUT_SECS)).await;
    tokio::task::yield_now().await;
    assert!(stream.next().await.is_none(), "VOD body idle timeout should terminate the stream");
    tokio::time::resume();

    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "VOD body idle-timeout cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&tracker],
        },
    )
    .await;
}

#[tokio::test]
async fn dropping_direct_series_body_releases_original_user_after_socket_owner_changes() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55036".parse().unwrap_or_else(|_| unreachable!());
    let provider_handle = acquire_test_provider_handle(&app_state, addr);
    let (first_provider_stream, first_tracker) =
        track_provider_stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed());
    let first = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-user-a",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(6, "http://provider-1.example/series/6.mkv"),
        provider_stream: first_provider_stream,
        provider_handle: Some(provider_handle),
    })
    .await;
    let (second_provider_stream, second_tracker) =
        track_provider_stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed());
    let second = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "series-user-b",
        max_connections: 1,
        addr,
        stream_channel: create_test_series_stream_channel(7, "http://provider-1.example/series/7.mkv"),
        provider_stream: second_provider_stream,
        provider_handle: None,
    })
    .await;
    let both_uids = [first.uid, second.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "two users sharing a reused socket address",
            active_counts: (2, 2),
            stream_uids: &both_uids,
            provider_connections: 1,
            dropped_streams: &[],
        },
    )
    .await;

    let first_body = Body::from_stream(first.stream);
    let second_body = Body::from_stream(second.stream);
    drop(first_body);
    let second_uid = [second.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "original user Body drop after socket owner replacement",
            active_counts: (1, 1),
            stream_uids: &second_uid,
            provider_connections: 0,
            dropped_streams: &[&first_tracker],
        },
    )
    .await;
    assert!(!second_tracker.is_dropped());

    drop(second_body);
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "replacement user Body drop cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&first_tracker, &second_tracker],
        },
    )
    .await;
}

#[tokio::test]
async fn multiple_same_user_direct_series_connections_release_independently() {
    let app_state = create_test_app_state();
    let first_addr = "127.0.0.1:55037".parse().unwrap_or_else(|_| unreachable!());
    let second_addr = "127.0.0.1:55038".parse().unwrap_or_else(|_| unreachable!());
    let (first_provider_stream, first_tracker) =
        track_provider_stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed());
    let first = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "multi-series-user",
        max_connections: 2,
        addr: first_addr,
        stream_channel: create_test_series_stream_channel(8, "http://provider-1.example/series/8.mkv"),
        provider_stream: first_provider_stream,
        provider_handle: None,
    })
    .await;
    let (second_provider_stream, second_tracker) =
        track_provider_stream(futures::stream::pending::<Result<Bytes, StreamError>>().boxed());
    let second = create_test_active_direct_stream(TestDirectStreamParams {
        app_state: &app_state,
        username: "multi-series-user",
        max_connections: 2,
        addr: second_addr,
        stream_channel: create_test_series_stream_channel(9, "http://provider-1.example/series/9.mkv"),
        provider_stream: second_provider_stream,
        provider_handle: None,
    })
    .await;
    let both_uids = [first.uid, second.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "two connections for one Series user",
            active_counts: (1, 2),
            stream_uids: &both_uids,
            provider_connections: 0,
            dropped_streams: &[],
        },
    )
    .await;

    let first_body = Body::from_stream(first.stream);
    let second_body = Body::from_stream(second.stream);
    drop(first_body);
    let second_uid = [second.uid];
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "first same-user Series connection cleanup",
            active_counts: (1, 1),
            stream_uids: &second_uid,
            provider_connections: 0,
            dropped_streams: &[&first_tracker],
        },
    )
    .await;
    assert!(!second_tracker.is_dropped());

    drop(second_body);
    wait_for_lifecycle(
        &app_state,
        ExpectedLifecycle {
            description: "last same-user Series connection cleanup",
            active_counts: (0, 0),
            stream_uids: &[],
            provider_connections: 0,
            dropped_streams: &[&first_tracker, &second_tracker],
        },
    )
    .await;
}
