use super::{
    clear_scheduled_prefetch, cold_startup_round, fetch_context, install_startup, prefix_drop_rounds,
    spawn_segment_server, spawn_sequence_response_server, SegmentFetchPolicy, SegmentFetchPriority,
};
use crate::{HlsOriginResourceFetchError, SegmentCacheStatus};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn capacity_deferred_segment_requeues_only_after_capacity_revision_changes() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;
    {
        let mut session = context.session.write().await;
        for proxy_seq in [2_u64, 3] {
            session.segments.get_mut(&proxy_seq).expect("ready tail segment").status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 19 };
        }
        assert!(session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Demand, 20));
    }
    let snapshot = worker.next_fetch_snapshot(&context, 20, &policy).await.expect("fetch snapshot");
    let notifier = {
        let mut session = context.session.write().await;
        session.segment_fetch_notifiers.entry(snapshot.proxy_seq).or_default().clone()
    };
    let completion = {
        let revision = context.segment_cache.capacity_revision();
        let error = HlsOriginResourceFetchError::LocalCacheCapacity {
            required_session_bytes: 1,
            required_global_bytes: 0,
            projected_write_bytes: 1,
            revision,
        };
        let mut session = context.session.write().await;
        worker.apply_segment_fetch_result(&mut session, &snapshot, &policy, Err(error), 21)
    };
    let retry = completion.scheduled_retry.expect("capacity deferral owns a revision retry");
    assert!(completion.notifier.is_none(), "deferred work retains its existing demand notifier");
    worker.schedule_segment_retry(context.clone(), snapshot, retry);
    let first_demand = worker.demand_fetch_and_wait(context.clone(), &segment_file, 22);
    let repeated_demand = worker.demand_fetch_and_wait(context.clone(), &segment_file, 23);
    tokio::pin!(first_demand, repeated_demand);
    assert!(matches!(futures::poll!(first_demand.as_mut()), std::task::Poll::Pending));
    assert!(matches!(futures::poll!(repeated_demand.as_mut()), std::task::Poll::Pending));
    assert!(server.requests.lock().await.is_empty(), "capacity wait does not poll the origin");

    let completed = notifier.notified();
    tokio::pin!(completed);
    completed.as_mut().enable();
    // The revision may change before the spawned waiter first polls. Its
    // token comparison must make that race lossless.
    context.segment_cache.notify_capacity_protection_changed();
    let ((), first_demand_outcome, repeated_demand_outcome) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(completed.as_mut(), first_demand.as_mut(), repeated_demand.as_mut())
    })
    .await
    .expect("revision wake completes one requeued fetch");

    assert_eq!(first_demand_outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(repeated_demand_outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(server.requests.lock().await.len(), 1);
    let session = context.session.read().await;
    assert!(matches!(session.segments.get(&1).map(|segment| &segment.status), Some(SegmentCacheStatus::Ready { .. })));
    let rendered = session.last_rendered_manifest.as_ref().expect("recovered timeline renders a manifest");
    assert_eq!(rendered.first_proxy_seq, 1);
    assert_eq!(rendered.last_proxy_seq, 3);
    assert!(rendered.body.contains("000001.ts"));
}

#[tokio::test]
async fn known_oversized_startup_body_falls_back_before_replay_or_publication() -> std::io::Result<()> {
    let server = spawn_sequence_response_server(Vec::new()).await;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    install_startup(&context, &worker, shared::model::HlsStartupMode::Progressive).await;
    let snapshot = worker
        .next_fetch_snapshot(&context, 10, &policy)
        .await
        .ok_or_else(|| std::io::Error::other("fetch snapshot"))?;
    let budget = {
        let mut session = context.session.write().await;
        let startup = session.startup.as_mut().ok_or_else(|| std::io::Error::other("startup policy"))?;
        startup.config.max_progressive_bytes_per_segment = shared::model::Bytes::new(8);
        Arc::clone(&startup.budget)
    };
    let prepared = super::super::prepare_startup_fill(
        &context,
        &snapshot,
        Some(12),
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await
    .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    assert!(!prepared.raw);
    assert_eq!(prepared.revision.revision().key.kind, crate::SegmentRevisionKind::Processed);
    assert!(!prepared.revision.revision().is_publishable());
    assert_eq!(context.session.read().await.startup_mode(), shared::model::HlsStartupMode::FirstReady);
    assert_eq!(budget.usage(), (0, 0));
    assert!(server.requests.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn cold_startup_publication_and_first_data_precede_successor_eof() -> std::io::Result<()> {
    for mode in [shared::model::HlsStartupMode::FirstReady, shared::model::HlsStartupMode::Progressive] {
        cold_startup_round(mode, None).await?;
    }
    Ok(())
}

#[tokio::test]
async fn first_ready_and_progressive_keep_one_origin_fill_and_exact_bytes() -> std::io::Result<()> {
    for mode in [shared::model::HlsStartupMode::FirstReady, shared::model::HlsStartupMode::Progressive] {
        let server = spawn_segment_server(0).await;
        let directory = tempfile::tempdir()?;
        let policy = SegmentFetchPolicy::default();
        let (worker, context, segment) = fetch_context(&server, &directory, &policy).await;
        install_startup(&context, &worker, mode).await;
        clear_scheduled_prefetch(&context, &policy).await;
        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment, 10).await;
        assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
        let revision = {
            let session = context.session.read().await;
            session
                .startup
                .as_ref()
                .and_then(|startup| startup.revisions.get(&segment.proxy_seq))
                .cloned()
                .ok_or_else(|| std::io::Error::other("completed revision missing"))?
        };
        let metadata = revision.revision().wait_complete(tokio::time::Instant::now()).await?;
        assert_eq!(tokio::fs::read(metadata.path).await?, b"body:/1.ts");
        assert_eq!(server.requests.lock().await.iter().filter(|request| request.starts_with("GET /1.ts ")).count(), 1);
        if mode == shared::model::HlsStartupMode::Progressive {
            assert_eq!(revision.revision().key.kind, crate::SegmentRevisionKind::Raw);
            assert_eq!(context.session.read().await.startup.as_ref().map(|s| s.budget.usage()), Some((0, 0)));
        }
    }
    Ok(())
}

#[tokio::test]
async fn progressive_exposes_prefix_before_origin_eof_and_drop_is_terminal() -> std::io::Result<()> {
    prefix_drop_rounds().await
}
