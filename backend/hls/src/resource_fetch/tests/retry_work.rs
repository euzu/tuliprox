use super::{fetch_clients, fetch_target, read_request, test_log_identity};
use crate::{
    origin_attempt::prepare_media_origin_attempt, HlsOriginResourceFetchError as Error, HlsOriginRetryWork,
    HlsResourceFetchKind, HlsResourceFetchSource, HlsSession, HlsSessionKey, SegmentFetchPolicy,
};
use futures::FutureExt;
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::Notify};

#[tokio::test]
async fn cancellation_during_preparation_transfer_and_backoff_releases_only_its_operation() {
    for phase in 0..3 {
        run_lifecycle(phase, HlsResourceFetchKind::Segment, HlsResourceFetchSource::Normal).await;
    }
}

#[tokio::test]
async fn a_hanging_commit_cannot_outlive_its_fixed_budget_after_policy_and_duration_changes() {
    run_lifecycle(3, HlsResourceFetchKind::Segment, HlsResourceFetchSource::Normal).await;
}

#[tokio::test]
async fn map_and_key_operations_share_the_same_backoff_and_deadline_contract() {
    for kind in [HlsResourceFetchKind::Map, HlsResourceFetchKind::Key] {
        for source in [HlsResourceFetchSource::Normal, HlsResourceFetchSource::Transient] {
            run_lifecycle(2, kind, source).await;
            run_lifecycle(3, kind, source).await;
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep each lifecycle phase, real provider cleanup and fixed-clock assertions together."
)]
async fn run_lifecycle(phase: u8, kind: HlsResourceFetchKind, source: HlsResourceFetchSource) {
    let session =
        Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "retry-work"), b"secret", 0)));
    let policy = SegmentFetchPolicy {
        timeouts: crate::HlsOriginTimeoutPolicy::fixed(10_000, 10_000),
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (io, work) = admitted_work(&session, &policy, kind, source).await;
    let manager = Arc::clone(&io.ctx.active_provider);
    let binding = session.read().await.origin_account_binding.clone().unwrap();
    let tag = binding.provider_binding_tag.unwrap();
    let count = Arc::clone(&session.read().await.activity.active_origin_work_count);
    let generation = session.read().await.activity.origin_work_generation;
    let deadline = work.deadline();
    let barrier = session.write().await.pause_next_origin_retry_for_test();
    let entered = Arc::new(Notify::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_ready = Arc::clone(&entered);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        if phase == 1 {
            server_ready.notify_one();
            std::future::pending::<()>().await;
        } else {
            socket
                .write_all(if phase == 2 {
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                })
                .await
                .unwrap();
        }
    });
    let mut clients = fetch_clients(false);
    clients.session = Some(Arc::clone(&session));
    clients.retry_work = Some(work);
    let prepare_session = Arc::clone(&session);
    let cleanup_session = Arc::clone(&session);
    let prepare_ready = Arc::clone(&entered);
    let commit_ready = Arc::clone(&entered);
    let task_policy = policy.clone();
    let task = tokio::spawn(async move {
        let mut target = fetch_target(format!("http://{address}/resource.bin"));
        target.kind = kind;
        target.source = source;
        super::super::run_hls_origin_resource_retry_loop_with_attempt_prepare(
            target,
            clients,
            &task_policy,
            &test_log_identity(),
            move |attempt| {
                let session = Arc::clone(&prepare_session);
                let io = io.clone();
                let ready = Arc::clone(&prepare_ready);
                async move {
                    if phase == 0 {
                        ready.notify_one();
                        std::future::pending::<()>().await;
                    }
                    assert_eq!(attempt.logical_deadline(), Some(deadline));
                    prepare_media_origin_attempt(
                        Some(&io),
                        &session,
                        generation,
                        Duration::from_millis(attempt.timeout.effective_ms),
                    )
                    .await
                    .map_err(Error::ProviderUnavailable)
                }
                .boxed()
            },
            move |guard| {
                let session = Arc::clone(&cleanup_session);
                async move {
                    guard.finish(&session).await;
                }
                .boxed()
            },
            move |_response, attempt, _deadline, guard| {
                let ready = Arc::clone(&commit_ready);
                async move {
                    assert_eq!(attempt.logical_deadline(), Some(deadline));
                    ready.notify_one();
                    let _guard = guard;
                    std::future::pending::<Result<(), Error>>().await
                }
                .boxed()
            },
        )
        .await
    });
    if phase == 2 {
        tokio::time::timeout(Duration::from_secs(2), barrier.retry_ready.notified()).await.unwrap();
    } else {
        tokio::time::timeout(Duration::from_secs(2), entered.notified()).await.unwrap();
    }
    assert!(count.load(Ordering::Acquire) > 0);
    if phase == 3 {
        session.write().await.target_duration = Some(999);
        let mut updated = policy.timeouts.config();
        updated.segment = shared::model::HlsSegmentTimeout::Fixed(shared::model::Millis::new(1_000_000));
        policy.timeouts.update(updated);
        tokio::time::pause();
        tokio::time::advance(deadline.saturating_duration_since(tokio::time::Instant::now())).await;
        tokio::time::resume();
        assert!(matches!(task.await.unwrap(), Err(Error::Timeout)));
    } else {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(8)).await;
        tokio::time::resume();
        assert_eq!(
            manager.binding_tag_for_owner(&binding.session_owner),
            Some(tag),
            "normal expiry cannot cancel the work"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(count.load(Ordering::Acquire), 0, "phase {phase}: every counter is released exactly once");
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.get_provider_connections_count() > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("I/O handle cancellation completes through the cleanup lane");
    assert!(
        manager.binding_tag_for_owner(&binding.session_owner).is_none(),
        "no operation right retains the expired slot"
    );
    server.abort();
}

async fn admitted_work(
    session: &crate::HlsSessionHandle,
    policy: &SegmentFetchPolicy,
    kind: HlsResourceFetchKind,
    source: HlsResourceFetchSource,
) -> (crate::HlsOriginIoContext, Arc<HlsOriginRetryWork>) {
    let (io, _) = crate::origin_attempt::tests::bound_origin_io(session).await;
    let manager = &io.ctx.active_provider;
    let binding = session.read().await.origin_account_binding.clone().unwrap();
    let handle = manager
        .acquire_exact_connection_with_lease_for_session(
            &binding.account_name,
            &io.client_addr,
            false,
            0,
            tuliprox_session::ConnectionKind::Normal,
            Some(tuliprox_session::PlaybackLeaseRef::new(
                &binding.session_owner,
                tuliprox_core::model::PlaybackKind::LiveHls,
            )),
        )
        .unwrap();
    let tag = manager.reserve_hls_start(&binding.session_owner, &handle, Duration::from_secs(7)).unwrap();
    manager.release_handle(&handle);
    session.write().await.origin_account_binding.as_mut().unwrap().provider_binding_tag = Some(tag);
    let work = HlsOriginRetryWork::for_resource(session, Some(manager), policy, kind, source).await.unwrap();
    (io, work)
}

#[tokio::test(start_paused = true)]
async fn retained_work_expires_without_polling_despite_new_viewers_and_manifest_activity() {
    let session =
        Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "retained-work"), b"secret", 0)));
    let policy = SegmentFetchPolicy {
        timeouts: crate::HlsOriginTimeoutPolicy::fixed(10_000, 10_000),
        ..SegmentFetchPolicy::default()
    };
    let (io, work) =
        admitted_work(&session, &policy, HlsResourceFetchKind::Segment, HlsResourceFetchSource::Normal).await;
    let deadline = work.deadline();
    let proxy = session.read().await.proxy_session_id.clone();
    for index in 0..3 {
        let now = tuliprox_core::utils::current_time_millis();
        io.ctx
            .hls_proxy
            .prepare_access_lease(crate::HlsAccessLease::pending(
                crate::HlsAccessLeaseId(format!("viewer-{index}")),
                crate::HlsPlaybackFamilyKey::new("viewer", format!("client-{index}")),
                proxy.clone(),
                "viewer".into(),
                "token".into(),
                1,
                "retained-work".into(),
                1,
                now,
                1_000_000,
            ))
            .await;
        session.write().await.mark_authorized_manifest_access(now);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(work.deadline(), deadline);
    }
    session.write().await.target_duration = Some(999);
    let mut updated = policy.timeouts.config();
    updated.segment = shared::model::HlsSegmentTimeout::Fixed(shared::model::Millis::new(1_000_000));
    policy.timeouts.update(updated);
    tokio::time::advance(deadline.saturating_duration_since(tokio::time::Instant::now())).await;
    tokio::task::yield_now().await;
    let state = session.read().await;
    assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert!(io
        .ctx
        .active_provider
        .binding_tag_for_owner(&state.origin_account_binding.as_ref().unwrap().session_owner)
        .is_none());
    assert!(state.activity.last_delivered_media_at_ms.is_none());
    drop(state);
    drop(work);
    assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
}
