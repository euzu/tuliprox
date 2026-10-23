use super::{
    bind_refresh_request_to_app_state, mark_origin_refresh_started, refresh_and_commit, spawn_test_origin,
    test_origin_refresh_request, LiveHlsOriginEntry, OriginRefreshRequest, RetryPolicy,
};
use crate::{
    HlsCtx, HlsFreshManifestRequiredReason, HlsManifestCommitRequirement, HlsOriginAccountBinding, HlsOriginIoContext,
    HlsSessionKey,
};
use shared::model::{provider_saturation::build_group_lookup, InputType};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tuliprox_core::model::{Config, ConfigInput, PlaybackKind, SourcesConfig};
use tuliprox_session::ConnectionKind;

async fn request(url: &str) -> (OriginRefreshRequest, HlsCtx) {
    let ctx = HlsCtx::for_test(Config::default());
    let inputs = vec![Arc::new(ConfigInput {
        id: 1,
        name: Arc::from("reserved-provider"),
        input_type: InputType::Xtream,
        enabled: true,
        max_connections: 1,
        url: url.into(),
        username: Some("user".into()),
        password: Some("pass".into()),
        ..Default::default()
    })];
    ctx.app_config.sources.store(Arc::new(SourcesConfig {
        group_lookup: build_group_lookup(&inputs),
        inputs,
        ..Default::default()
    }));
    ctx.active_provider.update_config(&ctx.app_config);
    let session = ctx.hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 100).await;
    let binding = HlsOriginAccountBinding::new(
        Arc::from("reserved-provider"),
        Arc::from("reserved-provider"),
        &session.read().await.proxy_session_id,
        100,
    );
    session.write().await.origin_account_binding = Some(binding);
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), &ctx);
    request.app_config = Arc::clone(&ctx.app_config);
    request.origin_entry = LiveHlsOriginEntry::parse(url).unwrap();
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };
    request.origin_io = Some(HlsOriginIoContext {
        ctx: ctx.clone(),
        client_addr: "127.0.0.1:1234".parse().unwrap(),
        allow_grace: false,
        priority: 0,
        connection_kind: ConnectionKind::Normal,
        reservation_ttl_secs: 60,
        preacquired_provider_handle: None,
        started_generation: None,
        prepared_account: None,
        playback_kind: PlaybackKind::LiveHls,
        account_runtime: None,
    });
    (request, ctx)
}

fn reserve(ctx: &HlsCtx) {
    ctx.active_provider.refresh_provider_reservation(&Arc::from("reserved-provider"), "foreign-playback", 60);
    assert!(ctx.active_provider.confirm_playback_activity("foreign-playback").is_some());
    assert!(ctx.active_provider.is_provider_reserved_for_other_session(&Arc::from("reserved-provider"), None));
}

#[tokio::test(start_paused = true)]
async fn persistent_real_reservation_exhausts_five_without_http_or_progress() {
    let server = spawn_test_origin(Arc::new(|_| panic!("reservation must prevent HTTP"))).await;
    let (mut request, ctx) = request(&format!("{}/index.m3u8", server.base_url)).await;
    reserve(&ctx);
    request.retry_policy = RetryPolicy::default();
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let attempt = request.attempt.clone().unwrap();
    let session = Arc::clone(&request.session);
    let started = tokio::time::Instant::now();
    refresh_and_commit(request, 100).await;
    assert_eq!(attempt.logical_attempts(), 5);
    assert!(attempt.failed());
    assert!(started.elapsed() <= Duration::from_secs(2));
    assert!(server.requests.lock().await.is_empty());
    let session = session.read().await;
    assert_eq!(session.origin_control.last_origin_response_at_ms, None);
    assert_eq!(session.origin_control.last_media_progress_at_ms, None);
    assert_eq!(session.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert!(!session.origin_refresh.in_flight);
    assert!(ctx.active_provider.is_provider_reserved_for_other_session(&Arc::from("reserved-provider"), None));
}

#[tokio::test]
async fn local_denials_and_http_share_five_attempts() {
    for (denials, failures, success) in [(2, 2, true), (4, 1, false)] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let server = spawn_test_origin(Arc::new(move |_| {
            let status = if seen.fetch_add(1, Ordering::AcqRel) < failures { 503 } else { 200 };
            (status, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n100.ts\n#EXTINF:4,\n101.ts\n#EXTINF:4,\n102.ts\n".into())
        })).await;
        let (mut request, ctx) = request(&format!("{}/index.m3u8", server.base_url)).await;
        reserve(&ctx);
        request.retry_policy = RetryPolicy { delays_ms: [0, 20, 30, 40, 50], jitter_max_ms: 0 };
        assert!(mark_origin_refresh_started(&mut request, 100).await);
        let attempt = request.attempt.clone().unwrap();
        let release = async {
            while attempt.logical_attempts() < denials {
                tokio::task::yield_now().await;
            }
            ctx.active_provider.clear_provider_reservation("foreign-playback");
        };
        tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(refresh_and_commit(request, 100), release) })
            .await
            .unwrap();
        assert_eq!(attempt.logical_attempts(), 5);
        assert_eq!(attempt.failed(), !success);
        assert_eq!(calls.load(Ordering::Acquire), 5 - denials);
        assert!(attempt.is_closed());
    }
}

#[tokio::test(start_paused = true)]
async fn local_acquire_deadline_is_not_restarted_by_denials() {
    let server = spawn_test_origin(Arc::new(|_| panic!("deadline must prevent HTTP"))).await;
    let (mut request, ctx) = request(&format!("{}/index.m3u8", server.base_url)).await;
    reserve(&ctx);
    request.origin_manifest_timeout_ms = 120;
    request.retry_policy = RetryPolicy { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 0 };
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let attempt = request.attempt.clone().unwrap();
    let started = tokio::time::Instant::now();
    refresh_and_commit(request, 100).await;
    assert!(attempt.failed());
    assert_eq!(attempt.logical_attempts(), 2);
    assert_eq!(started.elapsed(), Duration::from_millis(120));
    assert!(server.requests.lock().await.is_empty());
}

#[test]
fn standard_retry_policy_including_maximum_jitter_stays_within_two_seconds() {
    let policy = RetryPolicy::default();
    let backoff: u64 = (1..policy.attempt_count())
        .map(|index| policy.delay_for_attempt_ms(index, policy.jitter_max_ms).unwrap())
        .sum();
    assert_eq!(backoff, 2000);
}

#[tokio::test]
async fn cancellation_during_local_backoff_releases_work_and_cleanup_permits() {
    let (mut request, ctx) = request("http://127.0.0.1:9/index.m3u8").await;
    reserve(&ctx);
    request.retry_policy = RetryPolicy { delays_ms: [0, 500, 500, 500, 500], jitter_max_ms: 0 };
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let session = Arc::clone(&request.session);
    let attempt = request.attempt.clone().unwrap();
    let permits = ctx.connection_manager.control_cleanup_tx().capacity();
    let task = tokio::spawn(refresh_and_commit(request, 100));
    while attempt.logical_attempts() == 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while session.read().await.origin_refresh.in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let state = session.read().await;
    assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert!(state.origin_account_io_lease.as_ref().is_none_or(|lease| !lease.is_active_or_acquiring()));
    assert_eq!(ctx.connection_manager.control_cleanup_tx().capacity(), permits);
    assert_eq!(ctx.active_provider.active_connections().unwrap_or_default().values().sum::<usize>(), 0);
    assert!(attempt.failed());
}

#[tokio::test]
async fn binding_switch_during_local_backoff_preserves_successor_and_shared_attempt() {
    let server = spawn_test_origin(Arc::new(|_| panic!("obsolete entry must never be fetched"))).await;
    let (mut request, ctx) = request(&format!("{}/old.m3u8", server.base_url)).await;
    reserve(&ctx);
    request.retry_policy = RetryPolicy { delays_ms: [0, 50, 50, 50, 50], jitter_max_ms: 0 };
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let session = Arc::clone(&request.session);
    let attempt = request.attempt.clone().unwrap();
    let task = tokio::spawn(refresh_and_commit(request, 100));
    while attempt.logical_attempts() == 0 {
        tokio::task::yield_now().await;
    }
    let successor_identity = Arc::new(());
    {
        let mut state = session.write().await;
        let binding = state.origin_account_binding.clone().unwrap();
        state.handoff_origin_account(HlsOriginAccountBinding::rebound(
            binding.input_name,
            binding.account_name,
            binding.session_owner,
            binding.generation + 1,
            101,
        ));
        state.origin_refresh.mark_started(101);
        state.origin_refresh.refresh_identity = Some(Arc::clone(&successor_identity));
        state.origin_refresh.attempt = Some(Arc::clone(&attempt));
    }
    task.await.unwrap();
    let state = session.read().await;
    assert!(state.origin_refresh.in_flight);
    assert!(Arc::ptr_eq(state.origin_refresh.refresh_identity.as_ref().unwrap(), &successor_identity));
    assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert!(!attempt.is_closed(), "old completion cannot close the successor's attempt");
    assert_eq!(attempt.logical_attempts(), 1);
    assert!(server.requests.lock().await.is_empty());
}

#[tokio::test]
async fn late_old_http_error_cannot_fail_shared_successor_attempt_or_retain_allocation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/old.m3u8", listener.local_addr().unwrap());
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let origin = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0_u8; 4096];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        received_tx.send(()).unwrap();
        release_rx.await.unwrap();
        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let (mut old, ctx) = request(&url).await;
    assert!(mark_origin_refresh_started(&mut old, 100).await);
    let session = Arc::clone(&old.session);
    let attempt = old.attempt.clone().unwrap();
    let permits = ctx.connection_manager.control_cleanup_tx().capacity();
    let mut successor = old.clone();
    successor.origin_io.as_mut().unwrap().started_generation = None;
    successor.origin_io.as_mut().unwrap().prepared_account = None;
    let task = tokio::spawn(refresh_and_commit(old, 100));
    received_rx.await.unwrap();
    {
        let mut state = session.write().await;
        state.invalidate_queued_origin_work();
        state.origin_refresh = super::OriginRefreshState::default();
    }
    assert!(mark_origin_refresh_started(&mut successor, 101).await);
    let identity = successor.refresh_identity.clone().unwrap();
    release_tx.send(()).unwrap();
    task.await.unwrap();
    origin.await.unwrap();
    assert!(!attempt.failed(), "late 404 belongs to the obsolete binding");
    assert!(!attempt.is_closed());
    {
        let state = session.read().await;
        assert!(state.origin_refresh.in_flight);
        assert!(Arc::ptr_eq(state.origin_refresh.refresh_identity.as_ref().unwrap(), &identity));
        assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 1);
        assert!(state.origin_account_io_lease.is_none());
    }
    session.write().await.invalidate_queued_origin_work();
    refresh_and_commit(successor, 101).await;
    assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert_eq!(ctx.connection_manager.control_cleanup_tx().capacity(), permits);
    assert_eq!(ctx.active_provider.active_connections().unwrap_or_default().values().sum::<usize>(), 0);
}

#[tokio::test]
async fn abort_before_first_poll_releases_work_even_while_session_is_locked() {
    let (mut request, _) = request("http://127.0.0.1:9/index.m3u8").await;
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let session = Arc::clone(&request.session);
    let state = session.write().await;
    let task = tokio::spawn(refresh_and_commit(request, 100));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    drop(state);
    tokio::time::timeout(Duration::from_secs(1), async {
        while session.read().await.origin_refresh.in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_refresh_requests_share_one_budget_and_one_origin_allocation() {
    let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n100.ts\n#EXTINF:4,\n101.ts\n#EXTINF:4,\n102.ts\n".into()))).await;
    let (mut first, ctx) = request(&format!("{}/index.m3u8", server.base_url)).await;
    let mut second = first.clone();
    reserve(&ctx);
    first.retry_policy = RetryPolicy { delays_ms: [0, 50, 50, 50, 50], jitter_max_ms: 0 };
    assert!(mark_origin_refresh_started(&mut first, 100).await);
    let session = Arc::clone(&first.session);
    let attempt = first.attempt.clone().unwrap();
    let task = tokio::spawn(refresh_and_commit(first, 100));
    while attempt.logical_attempts() == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        super::mark_origin_refresh_started_with_outcome(&mut second, 100).await,
        super::HlsOriginRefreshTriggerOutcome::InFlight
    );
    assert!(Arc::ptr_eq(second.attempt.as_ref().unwrap(), &attempt));
    assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 1);
    ctx.active_provider.clear_provider_reservation("foreign-playback");
    task.await.unwrap();
    assert_eq!(attempt.logical_attempts(), 2);
    assert!(!attempt.failed());
    assert_eq!(server.requests.lock().await.len(), 1);
    assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert_eq!(ctx.active_provider.active_connections().unwrap_or_default().values().sum::<usize>(), 0);
}

#[tokio::test]
async fn reservation_diagnostics_distinguish_real_capacity_and_io_conflicts() {
    super::short_manifest::init_qualification_logger();
    for io_conflict in [false, true] {
        let (mut request, ctx) = request("http://127.0.0.1:9/live/user/pass/12345.m3u8").await;
        let old = request.session.read().await.origin_account_binding.clone().unwrap();
        let io = request.origin_io.clone().unwrap();
        let old_guard = if io_conflict {
            let guard =
                crate::origin::begin_hls_origin_account_io_bounded(&io, &request.session, &old, Duration::from_secs(1))
                    .await
                    .unwrap();
            request.session.write().await.origin_account_binding = Some(HlsOriginAccountBinding::rebound(
                old.input_name,
                old.account_name,
                old.session_owner,
                old.generation + 1,
                101,
            ));
            Some(guard)
        } else {
            reserve(&ctx);
            None
        };
        assert!(mark_origin_refresh_started(&mut request, 100).await);
        let session = Arc::clone(&request.session);
        refresh_and_commit(request, 100).await;
        assert_eq!(session.read().await.origin_control.last_origin_response_at_ms, None);
        assert_eq!(session.read().await.origin_control.last_media_progress_at_ms, None);
        if let Some(guard) = old_guard {
            crate::finish_hls_origin_account_io(&io, &session, guard, false).await;
        }
    }
    let messages = super::short_manifest::reservation_messages();
    let capacity = messages
        .iter()
        .find(|message| {
            message.contains("conflict=provider-capacity")
                && message.contains("owner=hls-cache:")
                && message.contains("active_connections=0 limit=1 idle_foreign_reserved_slots=1")
        })
        .unwrap();
    let io = messages
        .iter()
        .find(|message| {
            message.contains("conflict=io-binding") && message.contains("active_io_count=1 acquiring=false")
        })
        .unwrap();
    assert!(io.contains("requested_generation=1") && io.contains("blocking_generation=0"));
    assert!(messages.iter().any(|message| message.contains("used=5 remaining=0 completion=budget-exhausted")));
    for message in [capacity, io] {
        assert!(!message.contains("http://") && !message.contains("user/pass"));
        eprintln!("{message}");
    }
}

#[tokio::test]
async fn abort_during_http_releases_real_provider_allocation_and_cleanup_permit() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/index.m3u8", listener.local_addr().unwrap());
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let origin = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0_u8; 4096];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        received_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let (mut request, ctx) = request(&url).await;
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let session = Arc::clone(&request.session);
    let permits = ctx.connection_manager.control_cleanup_tx().capacity();
    let task = tokio::spawn(refresh_and_commit(request, 100));
    received_rx.await.unwrap();
    assert_eq!(ctx.active_provider.active_connections().unwrap_or_default().values().sum::<usize>(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !session.read().await.origin_refresh.in_flight
                && ctx.active_provider.active_connections().unwrap_or_default().values().sum::<usize>() == 0
                && ctx.connection_manager.control_cleanup_tx().capacity() == permits
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let state = session.read().await;
    assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert!(state.origin_account_io_lease.as_ref().is_none_or(|lease| !lease.is_active_or_acquiring()));
    origin.abort();
}

#[tokio::test]
async fn cancellation_after_counter_completion_still_releases_singleflight_without_double_decrement() {
    let (mut request, _) = request("http://127.0.0.1:9/index.m3u8").await;
    assert!(mark_origin_refresh_started(&mut request, 100).await);
    let session = Arc::clone(&request.session);
    assert!(super::super::finish_refresh_origin_work(&request, 101).await.generation_valid);
    request.work_owner.as_ref().unwrap().finish_counter();
    let state = session.write().await;
    let task = tokio::spawn(async move {
        let _request = request;
        std::future::pending::<()>().await;
    });
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(state.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    drop(state);
    tokio::time::timeout(Duration::from_secs(1), async {
        while session.read().await.origin_refresh.in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
}
