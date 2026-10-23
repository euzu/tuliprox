use super::*;
use crate::{cache::StartupWritePhase, SegmentCacheStatus};
use std::{fmt::Write as _, sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;

#[tokio::test]
async fn startup_failure_and_cancellation_close_each_commit_transition() -> std::io::Result<()> {
    for phase in [
        StartupWritePhase::BeforeNormalCommit,
        StartupWritePhase::AfterNormalCommit,
        StartupWritePhase::AfterPublication,
    ] {
        for cancel in [false, true] {
            for head in [1, 3] {
                startup_transition_round(phase, cancel, head).await?;
            }
        }
    }
    Ok(())
}

async fn startup_transition_round(phase: StartupWritePhase, cancel: bool, head: u64) -> std::io::Result<()> {
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    if head == 3 {
        let mut body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n".to_string();
        for seq in 1..=6 {
            writeln!(body, "#EXTINF:4,\n{}/{seq}.ts", server.base_url).unwrap();
        }
        let mut session = context.session.write().await;
        session.apply_origin_manifest(&normal_manifest(&body)).unwrap();
        session.live_window_strip.value = 1;
    }
    install_startup(&context, &worker, shared::model::HlsStartupMode::FirstReady).await;
    clear_scheduled_prefetch(&context, &policy).await;
    context.session.write().await.queue_segment_fetch_candidate(head, SegmentFetchPriority::Demand, 10);
    let snapshot = worker.next_fetch_snapshot(&context, 10, &policy).await.unwrap();
    let (entered, resume) = context.segment_cache.pause_startup_write(phase);
    let task_context = context.clone();
    let task_snapshot = snapshot.clone();
    let task_policy = policy.clone();
    let task = tokio::spawn(async move {
        super::super::fetch_segment_into_cache(&task_context, &task_snapshot, &task_policy).await
    });
    tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
    let revision = context.session.read().await.startup.as_ref().unwrap().revisions[&head].clone();
    if cancel {
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        drop(resume);
    } else {
        drop(resume); // A closed checkpoint injects a local I/O failure.
        let result = task.await.unwrap();
        assert!(result.is_err());
        let mut session = context.session.write().await;
        worker.apply_segment_fetch_result(&mut session, &snapshot, &policy, result, 11);
    }
    let id = snapshot.cache_key.proxy_session_id();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if context.segment_cache.remaining_revision_bytes(id) == 0
                && context.session.read().await.active_segment_fetches == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded cleanup completes without GC or foreign mutations");
    assert!(context.segment_cache.metadata(&snapshot.cache_key).await?.is_none());
    assert_eq!(revision.revision().is_complete(), phase == StartupWritePhase::AfterPublication);
    let bytes = context.segment_cache.capacity_usage(id).await?.session_bytes;
    assert_eq!(bytes, if phase == StartupWritePhase::AfterPublication { 10 } else { 0 });
    assert_eq!(context.segment_cache.revision_disk_bytes(), context.segment_cache.scan_revision_disk_bytes().await?);
    assert_eq!(temp_cache_files(directory.path()).len(), 0);
    if phase == StartupWritePhase::AfterPublication {
        for _ in 0..2 {
            let metadata = revision.revision().wait_complete(tokio::time::Instant::now()).await?;
            assert_eq!(tokio::fs::read(metadata.path).await?, format!("body:/{head}.ts").as_bytes());
        }
    }
    Ok(())
}

#[tokio::test]
async fn capacity_retry_requires_the_complete_peak_and_only_reservation_release() -> std::io::Result<()> {
    use super::super::scheduler::{wait_for_capacity_retry_admission, CapacityRetryAdmission};
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    install_startup(&context, &worker, shared::model::HlsStartupMode::FirstReady).await;
    clear_scheduled_prefetch(&context, &policy).await;
    context.session.write().await.queue_segment_fetch_candidate(1, SegmentFetchPriority::Demand, 10);
    let snapshot = worker.next_fetch_snapshot(&context, 10, &policy).await.unwrap();
    context.segment_cache.update_cache_limits(100, 100);
    let held = context.segment_cache.reserve_revision_peak(&snapshot.cache_key, 70).await?;
    let small = context.segment_cache.reserve_revision_peak(&snapshot.cache_key, 10).await?;
    let sufficient = context.segment_cache.reserve_revision_peak(&snapshot.cache_key, 10).await?;
    let error = context.segment_cache.reserve_revision_peak(&snapshot.cache_key, 30).await.err().unwrap();
    let revision = crate::cache::hls_cache_capacity_from_io(&error).unwrap().revision().clone();
    context.session.write().await.segments.get_mut(&1).unwrap().status =
        SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Demand, deferred_at_ms: 11 };
    let waiting = wait_for_capacity_retry_admission(&context, &snapshot, revision, 30);
    tokio::pin!(waiting);
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    drop(small);
    assert!(futures::poll!(waiting.as_mut()).is_pending(), "20 free bytes cannot admit a fresh 30-byte peak");
    assert!(server.requests.lock().await.is_empty());
    drop(sufficient);
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap(), CapacityRetryAdmission::Ready);
    assert!(server.requests.lock().await.is_empty());
    drop(held);
    Ok(())
}

#[tokio::test]
async fn thousand_live_windows_keep_one_manager_two_leases_and_bounded_revision_disk() -> std::io::Result<()> {
    tokio::time::timeout(Duration::from_secs(60), live_window_soak())
        .await
        .expect("1000-window soak makes bounded progress")
}

#[allow(clippy::too_many_lines)]
async fn live_window_soak() -> std::io::Result<()> {
    use crate::{
        HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsProxyManager, HlsSessionKey,
    };
    use shared::model::{ByteSize, HlsCacheConfigDto, HlsStartupMode};
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir()?;
    let mut dto = HlsCacheConfigDto {
        cache_path: Some(directory.path().to_string_lossy().into_owned()),
        cache_duration: shared::model::Secs::new(1),
        cache_bytes: ByteSize::new("512 B"),
        cache_bytes_per_session: ByteSize::new("256 B"),
        max_concurrent_segment_fetches_per_session: 1,
        ..Default::default()
    };
    dto.startup.mode = HlsStartupMode::FirstReady;
    let manager = HlsProxyManager::with_hls_cache_config(&tuliprox_core::model::HlsCacheConfig::from(&dto));
    let now = current_time_millis();
    let session_key = HlsSessionKey::new(1, "capacity-soak");
    let (session, _) = manager.get_or_create_session_with_outcome(session_key.clone(), b"secret", now).await;
    let id = session.read().await.proxy_session_id.clone();
    let lease_ids = [HlsAccessLeaseId("soak-a".into()), HlsAccessLeaseId("soak-b".into())];
    for lease_id in &lease_ids {
        let mut lease = HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("test", &lease_id.0),
            id.clone(),
            "test".into(),
            "test-session".into(),
            1,
            "capacity-soak".into(),
            1,
            now,
            1_000_000,
        );
        lease.state = HlsAccessLeaseState::Activated;
        lease.active_until_ms = Some(now + 1_000_000);
        assert!(manager.access_leases().write().await.prepare_access_lease(lease));
    }
    let (_, mut context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    context.session = Arc::clone(&session);
    context.segment_cache = Arc::clone(manager.segment_cache());
    context.segment_repair = Arc::clone(manager.segment_repair());
    let worker = Arc::clone(manager.segment_worker_pool());
    let policy = worker.policy();
    let mut downloads = 0usize;
    let mut injected_failures = 0usize;
    let mut reclaimed = 0u64;
    for head in 0u64..1000 {
        let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
        for seq in head..head + 6 {
            writeln!(body, "#EXTINF:4,\n{}/{seq}.ts", server.base_url).unwrap();
        }
        let keys = {
            let mut guard = session.write().await;
            guard.last_client_access_at_ms = now + head;
            guard.apply_origin_manifest(&normal_manifest(&body)).unwrap();
            guard
                .segments
                .values()
                .filter(|entry| {
                    entry.origin_fetch_ref.is_some() && !matches!(entry.status, SegmentCacheStatus::Ready { .. })
                })
                .map(|entry| entry.cache_key.clone())
                .collect::<Vec<_>>()
        };
        for key in keys {
            let inject = head > 0 && head.is_multiple_of(100);
            if inject {
                let (entered, resume) = context.segment_cache.pause_startup_write(StartupWritePhase::AfterNormalCommit);
                let fill = soak_fetch(&worker, &context, &policy, key.proxy_seq(), now + head);
                tokio::pin!(fill);
                tokio::select! {
                    reached = entered => reached.unwrap(),
                    result = &mut fill => panic!("fill bypassed injected checkpoint: {}", result.is_ok()),
                }
                drop(resume);
                assert!(fill.await.is_err());
                injected_failures += 1;
                session.write().await.segments.get_mut(&key.proxy_seq()).unwrap().status =
                    SegmentCacheStatus::Discovered;
            }
            soak_fetch(&worker, &context, &policy, key.proxy_seq(), now + head).await?;
            downloads += 1;
            let segment = crate::HlsSegmentFile { proxy_seq: key.proxy_seq(), extension: "ts".into() };
            for _ in 0..2 {
                assert_eq!(
                    worker.demand_fetch_and_wait(context.clone(), &segment, now + head).await,
                    super::super::SegmentDemandFetchOutcome::Ready
                );
            }
        }
        let snapshot = {
            let mut guard = session.write().await;
            guard.render_and_store_manifest(now + head).unwrap();
            let rendered = guard.last_rendered_manifest.as_ref().unwrap();
            crate::manifest_snapshot::derive_hls_lease_manifest_snapshot(
                &crate::manifest_snapshot::HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
                    session: &guard,
                    committed_body: &rendered.body,
                    materialized_body: &rendered.body,
                    stripped_tail_segments: 0,
                },
                now + head,
            )
            .unwrap()
            .unwrap()
        };
        for lease_id in &lease_ids {
            let expected = manager.prepare_access_lease_manifest_publication(lease_id, &id, now + head).await.unwrap();
            assert!(manager
                .commit_access_lease_manifest_publication(lease_id, &id, expected, snapshot.clone(), now + head)
                .await
                .is_committed());
        }
        drop(snapshot);
        let report = manager.run_garbage_collection_once(now + head + 60_000).await?;
        reclaimed += report.revision_bytes_reclaimed;
        let usage = context.segment_cache.capacity_usage(&id).await?;
        assert!(usage.session_bytes <= 256 && usage.global_bytes <= 512);
        assert_eq!(context.segment_cache.remaining_revision_bytes(&id), 0);
        assert_eq!(
            context.segment_cache.revision_disk_bytes(),
            context.segment_cache.scan_revision_disk_bytes().await?
        );
        if head.is_multiple_of(100) {
            eprintln!("live-window={head} downloads={downloads} revision_gc_bytes={reclaimed} injected_failures={injected_failures}");
        }
    }
    for lease in &lease_ids {
        manager.access_leases().write().await.remove_access_lease(lease);
    }
    for _ in 0..16 {
        if manager.run_garbage_collection_once(now + 61_000).await?.revision_deletions_planned == 0 {
            break;
        }
    }
    let guard = session.read().await;
    let startup = guard.startup.as_ref().unwrap();
    assert!(startup.store.retire_unpinned()?.is_empty());
    let retained = startup
        .revisions
        .values()
        .filter_map(|owner| match owner.revision().state() {
            crate::SegmentRevisionState::Complete(metadata) => Some(metadata.size),
            crate::SegmentRevisionState::Pending | crate::SegmentRevisionState::Failed => None,
        })
        .sum::<u64>();
    assert_eq!(context.segment_cache.scan_revision_disk_bytes().await?, retained);
    assert_eq!(context.segment_cache.remaining_revision_bytes(&id), 0);
    assert!(downloads >= 1000 && injected_failures >= 9 && reclaimed > 0);
    drop(guard);
    let (same_session, _) = manager.get_or_create_session_with_outcome(session_key, b"secret", now + 1001).await;
    assert!(Arc::ptr_eq(&session, &same_session));
    assert_eq!(server.requests.lock().await.len(), downloads + injected_failures);
    eprintln!(
        "live-window=1000 downloads={downloads} revision_gc_bytes={reclaimed} injected_failures={injected_failures}"
    );
    Ok(())
}

async fn soak_fetch(
    worker: &Arc<crate::HlsSegmentWorkerPool>,
    context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
    seq: u64,
    now: u64,
) -> std::io::Result<()> {
    assert!(context.session.write().await.queue_segment_fetch_candidate(seq, SegmentFetchPriority::Demand, now));
    let snapshot = worker.next_fetch_snapshot(context, now, policy).await.unwrap();
    let result = super::super::fetch_segment_into_cache(context, &snapshot, policy).await;
    let success = result.is_ok();
    let mut session = context.session.write().await;
    worker.apply_segment_fetch_result(&mut session, &snapshot, policy, result, now);
    if success {
        Ok(())
    } else {
        Err(std::io::Error::other("injected or unexpected fill failure"))
    }
}

#[tokio::test]
async fn processed_peak_release_resumes_a_real_deferred_download_without_gc() -> std::io::Result<()> {
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy::default();
    let (worker, context, segment) = fetch_context(&server, &directory, &policy).await;
    install_startup(&context, &worker, shared::model::HlsStartupMode::FirstReady).await;
    clear_scheduled_prefetch(&context, &policy).await;
    context.segment_cache.update_cache_limits(100, 100);
    let key = context.session.read().await.segments[&1].cache_key.clone();
    let held = context.segment_cache.reserve_revision_peak(&key, 80).await?;
    let demand = worker.demand_fetch_and_wait(context.clone(), &segment, current_time_millis());
    tokio::pin!(demand);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                outcome = &mut demand => panic!("premature demand completion: {outcome:?}"),
                () = tokio::task::yield_now() => {},
            }
            if matches!(context.session.read().await.segments[&1].status, SegmentCacheStatus::CapacityDeferred { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(server.requests.lock().await.len(), 1);
    drop(held);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), demand).await.unwrap(),
        super::super::SegmentDemandFetchOutcome::Ready
    );
    assert_eq!(server.requests.lock().await.len(), 2);
    assert_eq!(context.segment_cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    assert_eq!(tokio::fs::read(context.segment_cache.object_path(&key)).await?, b"body:/1.ts");
    Ok(())
}

#[tokio::test]
async fn unknown_and_growing_bodies_abort_before_writing_unreserved_bytes() -> std::io::Result<()> {
    use futures::StreamExt;
    for mode in [shared::model::HlsStartupMode::FirstReady, shared::model::HlsStartupMode::Progressive] {
        for expected in [None, Some(8)] {
            let server = spawn_segment_server(0).await;
            let directory = tempfile::tempdir()?;
            let policy = SegmentFetchPolicy::default();
            let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
            install_startup(&context, &worker, mode).await;
            clear_scheduled_prefetch(&context, &policy).await;
            context.session.write().await.startup.as_mut().unwrap().config.max_progressive_bytes_per_segment =
                shared::model::Bytes::new(8);
            context.segment_cache.update_cache_limits(45, 45);
            context.session.write().await.queue_segment_fetch_candidate(1, SegmentFetchPriority::Demand, 10);
            let snapshot = worker.next_fetch_snapshot(&context, 10, &policy).await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let prepared = super::super::prepare_startup_fill(&context, &snapshot, expected, deadline)
                .await
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            let mut transaction = super::super::startup_write::StartupWrite::new(
                &context,
                &snapshot,
                Arc::clone(&prepared.reservation),
                prepared.revision.clone(),
            );
            transaction.track_fill(&prepared.observed);
            let observed = Arc::clone(&prepared.observed);
            let stream = futures::stream::iter([b"12345678".as_slice(), b"extra".as_slice()]).then(move |chunk| {
                let observed = Arc::clone(&observed);
                async move {
                    observed.lock().await.observe(chunk).await?;
                    Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(chunk))
                }
            });
            let reader = Box::pin(tokio_util::io::StreamReader::new(stream.boxed()));
            let result = context
                .segment_cache
                .stage_temp_with_budget(&snapshot.cache_key, reader, deadline, Some(Arc::clone(&prepared.reservation)))
                .await;
            assert!(result.is_err());
            // The first chunk was covered; the rejected second chunk never
            // reached either the normal staging file or the raw spool.
            if prepared.raw {
                assert_eq!(tokio::fs::metadata(&prepared.observed.lock().await.path).await?.len(), 8);
            }
            transaction.abort().await;
            drop(transaction);
            drop(prepared);
            assert_eq!(
                context.segment_cache.capacity_usage(snapshot.cache_key.proxy_session_id()).await?.session_bytes,
                0
            );
            assert_eq!(context.segment_cache.remaining_revision_bytes(snapshot.cache_key.proxy_session_id()), 0);
            assert_eq!(temp_cache_files(directory.path()).len(), 0);
        }
    }
    Ok(())
}
