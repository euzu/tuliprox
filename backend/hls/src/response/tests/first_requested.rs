//! Startup follows the player's first full GET, including a headers-only successor.
use super::*;
use crate::sync_ext::MutexExt;

const BODY: &[u8] = b"prefix-tail";

struct FirstRequestFixture {
    cached: CachedSegmentFixture,
    revisions: std::collections::BTreeMap<u64, crate::SegmentRevisionGuard>,
}

impl FirstRequestFixture {
    async fn new(mode: shared::model::HlsStartupMode, with_prefix: bool, with_replay: bool) -> std::io::Result<Self> {
        let cached = CachedSegmentFixture::new(BODY.to_vec()).await;
        let head = publish_revision_fixture(&cached, true).await?;
        let mut revisions = std::collections::BTreeMap::from([(12, head)]);
        {
            let mut session = cached.session.write().await;
            session.startup.as_mut().expect("startup").config.mode = mode;
            let store = Arc::clone(&session.startup.as_ref().expect("startup").store);
            let budget = Arc::clone(&session.startup.as_ref().expect("startup").budget);
            for seq in [13, 14] {
                let mut entry = session.segments[&12].clone();
                entry.proxy_seq = seq;
                entry.origin_key.host_local_sequence = seq;
                entry.origin_key.host_local_index = u32::try_from(seq).expect("fixture sequence");
                entry.cache_key = SegmentCacheKey::new(cached.proxy_session_id.clone(), seq, "ts");
                entry.media_start_ms = seq * 4000;
                entry.status = SegmentCacheStatus::Fetching {
                    priority: crate::SegmentFetchPriority::Demand,
                    started_at_ms: current_time_millis(),
                };
                session.segments.insert(seq, entry);
                let revision = store.create(cached.proxy_session_id.clone(), seq, crate::SegmentRevisionKind::Raw)?;
                revision.revision().bind_origin(&session);
                if with_replay {
                    let mut replay = crate::progressive_startup::ProgressiveReplay::new(&budget)
                        .ok_or_else(|| std::io::Error::other("replay slot"))?;
                    if with_prefix {
                        replay.push(b"prefix")?;
                        revision.revision().prefix_available.store(6, Ordering::Release);
                    }
                    *revision.revision().replay.lock_unpoisoned() = Some(replay);
                }
                session.startup.as_mut().expect("startup").revisions.insert(seq, revision.clone());
                revisions.insert(seq, revision);
            }
        }
        let mut leases = cached.manager.access_leases().write().await;
        let mut snapshot = leases
            .response_snapshot(&cached.lease_id, &cached.proxy_session_id, current_time_millis())
            .expect("lease")
            .last_manifest_snapshot
            .expect("snapshot");
        let mut segments = snapshot.visible_segments.to_vec();
        for seq in [13, 14] {
            let mut segment = segments[0].clone();
            segment.proxy_seq = seq;
            segment.uri = format!("{seq}.ts").into();
            segments.push(segment);
        }
        snapshot.visible_segments = Arc::from(segments);
        snapshot.last_proxy_seq = 14;
        snapshot.playlist_duration_ms = 12_000;
        snapshot.last_visible_media_end_ms = 12_000;
        snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
            window_plan: None,
            mode,
            retained_start_seq: 12,
            revisions: revisions.clone(),
        }));
        let now = current_time_millis();
        let guard =
            leases.prepare_manifest_publication(&cached.lease_id, &cached.proxy_session_id, now).expect("guard");
        assert!(leases
            .commit_manifest_publication(&cached.lease_id, &cached.proxy_session_id, guard, snapshot, now)
            .is_committed());
        drop(leases);
        Ok(Self { cached, revisions })
    }

    async fn serve(&self, seq: u64, range: Option<&str>) -> HlsResourceServeOutcome {
        self.serve_method(seq, range, axum::http::Method::GET).await
    }

    async fn serve_method(&self, seq: u64, range: Option<&str>, method: axum::http::Method) -> HlsResourceServeOutcome {
        let context = HlsCacheResponseContext {
            hls_access_lease_id: self.cached.lease_id.clone(),
            log_identity: self.cached.context.log_identity.clone(),
            cache_duration_seconds: self.cached.context.cache_duration_seconds,
            metrics: Arc::clone(&self.cached.context.metrics),
            segment_repair: Arc::clone(&self.cached.context.segment_repair),
            qos_meter: Arc::clone(&self.cached.context.qos_meter),
            media_activity_marker: self.cached.context.media_activity_marker.clone(),
            request_method: method,
            now_ms: current_time_millis(),
        };
        serve_hls_segment_cache_outcome(
            Arc::clone(&self.cached.segment_cache),
            Arc::clone(&self.cached.session),
            HlsSegmentFile { proxy_seq: seq, extension: "ts".into() },
            range.map(header),
            &context,
        )
        .await
    }

    fn prefix(&self, seq: u64) -> std::io::Result<()> {
        let revision = self.revisions[&seq].revision();
        revision.replay.lock_unpoisoned().as_mut().expect("replay").push(b"prefix")?;
        revision.prefix_available.store(6, Ordering::Release);
        revision.prefix_changed();
        Ok(())
    }

    async fn complete(&self, seq: u64) -> std::io::Result<()> {
        let cache_key = self.cached.session.read().await.segments[&seq].cache_key.clone();
        let metadata = self.cached.segment_cache.write_bytes_and_commit(&cache_key, BODY).await?;
        assert!(self.revisions[&seq].revision().complete(metadata));
        self.revisions[&seq].revision().replay.lock_unpoisoned().take();
        self.cached.session.write().await.segments.get_mut(&seq).expect("entry").status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(BODY.len()).expect("body length"),
            ready_at_ms: current_time_millis(),
        };
        Ok(())
    }
}

#[tokio::test]
async fn first_successor_streams_before_eof_in_both_raw_startup_modes() -> std::io::Result<()> {
    for mode in [shared::model::HlsStartupMode::Progressive, shared::model::HlsStartupMode::Optimistic] {
        let fixture = FirstRequestFixture::new(mode, true, true).await?;
        let response = ready_response(fixture.serve(13, None).await);
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        let mut body = response.into_body().into_data_stream();
        assert_eq!(
            body.next().await.transpose().map_err(std::io::Error::other)?.as_deref(),
            Some(b"prefix".as_slice())
        );
        assert!(fixture.revisions[&13].revision().was_exposed());
        assert!(!fixture.revisions[&12].revision().was_exposed());
        assert!(!fixture.revisions[&13].revision().is_complete());
        let subsequent = fixture.serve(14, None);
        tokio::pin!(subsequent);
        assert!(futures::poll!(subsequent.as_mut()).is_pending());
        fixture.complete(14).await?;
        let subsequent = ready_response(subsequent.await);
        assert!(subsequent.headers().contains_key(header::CONTENT_LENGTH));
        assert_eq!(subsequent.into_body().collect().await.map_err(std::io::Error::other)?.to_bytes().as_ref(), BODY);
        fixture.complete(13).await?;
        assert_eq!(body.next().await.transpose().map_err(std::io::Error::other)?.as_deref(), Some(b"-tail".as_slice()));
        assert!(body.next().await.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn headers_only_first_get_keeps_the_claim_until_its_own_prefix_arrives() -> std::io::Result<()> {
    let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, false, true).await?;
    let first = fixture.serve(13, Some("bytes=0-"));
    let later = fixture.serve(14, None);
    tokio::pin!(first, later);
    assert!(futures::poll!(first.as_mut()).is_pending());
    assert!(futures::poll!(later.as_mut()).is_pending());
    fixture.prefix(14)?;
    assert!(futures::poll!(later.as_mut()).is_pending(), "a later GET still waits for EOF");
    fixture.prefix(13)?;
    let response = ready_response(first.await);
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    assert_eq!(body.next().await.transpose().map_err(std::io::Error::other)?.as_deref(), Some(b"prefix".as_slice()));
    assert!(futures::poll!(later.as_mut()).is_pending());
    fixture.complete(14).await?;
    assert!(ready_response(later.await).headers().contains_key(header::CONTENT_LENGTH));
    Ok(())
}

#[tokio::test]
async fn headers_only_first_get_times_out_temporarily_and_preserves_the_lease() -> std::io::Result<()> {
    let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, false, true).await?;
    fixture.cached.session.write().await.startup.as_mut().expect("startup").first_data_timeout_ms = 25;
    let failure = match fixture.serve(13, None).await {
        HlsResourceServeOutcome::Failure(failure) => failure,
        HlsResourceServeOutcome::Ready(_) => panic!("the origin has not supplied a prefix"),
    };
    let response = crate::response::cache::hls_resource_failure_default_response(failure);
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    assert!(fixture
        .cached
        .manager
        .access_lease_response_snapshot(
            &fixture.cached.lease_id,
            &fixture.cached.proxy_session_id,
            current_time_millis()
        )
        .await
        .is_some());
    fixture.complete(13).await?;
    let retry = ready_response(fixture.serve(13, None).await);
    assert!(retry.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(retry.into_body().collect().await.map_err(std::io::Error::other)?.to_bytes().as_ref(), BODY);
    Ok(())
}

#[tokio::test]
async fn head_metadata_probe_does_not_consume_the_first_get_opportunity() -> std::io::Result<()> {
    let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, true, true).await?;
    let head = fixture.serve_method(12, None, axum::http::Method::HEAD);
    tokio::pin!(head);
    assert!(futures::poll!(head.as_mut()).is_pending(), "HEAD waits for complete metadata");
    let first_get = ready_response(fixture.serve(13, None).await);
    assert!(!first_get.headers().contains_key(header::CONTENT_LENGTH));
    assert!(fixture.revisions[&13].revision().was_exposed());
    assert!(!fixture.revisions[&12].revision().was_exposed());
    fixture.complete(12).await?;
    assert!(ready_response(head.await).headers().contains_key(header::CONTENT_LENGTH));
    Ok(())
}

#[tokio::test]
async fn first_get_without_replay_or_after_unexposed_replay_exhaustion_waits_for_complete_bytes() -> std::io::Result<()>
{
    for replay in [false, true] {
        let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, false, replay).await?;
        let first = fixture.serve(13, None);
        tokio::pin!(first);
        assert!(futures::poll!(first.as_mut()).is_pending());
        if replay {
            fixture.revisions[&13].revision().replay_exhausted();
            assert!(futures::poll!(first.as_mut()).is_pending());
        }
        fixture.complete(13).await?;
        let response = ready_response(first.await);
        assert!(response.headers().contains_key(header::CONTENT_LENGTH));
        assert_eq!(response.into_body().collect().await.map_err(std::io::Error::other)?.to_bytes().as_ref(), BODY);
        assert!(!fixture.revisions[&13].revision().was_exposed());
    }
    Ok(())
}

#[tokio::test]
async fn completed_first_get_consumes_startup_but_a_partial_range_does_not() -> std::io::Result<()> {
    for partial in [false, true] {
        let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, true, true).await?;
        fixture.complete(12).await?;
        let first = ready_response(fixture.serve(12, if partial { Some("bytes=2-5") } else { None }).await);
        assert_eq!(first.status(), if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK });
        first.into_body().collect().await.map_err(std::io::Error::other)?;
        let next = fixture.serve(13, None);
        tokio::pin!(next);
        if partial {
            let response = ready_response(next.await);
            assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        } else {
            assert!(futures::poll!(next.as_mut()).is_pending());
            fixture.complete(13).await?;
            assert!(ready_response(next.await).headers().contains_key(header::CONTENT_LENGTH));
        }
    }
    Ok(())
}

#[tokio::test]
async fn missing_uri_does_not_consume_startup_and_cancelled_first_get_does() -> std::io::Result<()> {
    let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, false, true).await?;
    assert!(matches!(
        fixture.serve(99, None).await,
        HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Missing)
    ));
    {
        let first = fixture.serve(13, None);
        tokio::pin!(first);
        assert!(futures::poll!(first.as_mut()).is_pending());
    }
    fixture.prefix(14)?;
    let next = fixture.serve(14, None);
    tokio::pin!(next);
    assert!(futures::poll!(next.as_mut()).is_pending());
    fixture.complete(14).await?;
    assert!(ready_response(next.await).headers().contains_key(header::CONTENT_LENGTH));
    Ok(())
}

#[tokio::test]
async fn first_successor_body_failure_is_terminal_for_the_exposed_revision() -> std::io::Result<()> {
    let fixture = FirstRequestFixture::new(shared::model::HlsStartupMode::Optimistic, true, true).await?;
    let response = ready_response(fixture.serve(13, None).await);
    let mut body = response.into_body().into_data_stream();
    assert_eq!(body.next().await.transpose().map_err(std::io::Error::other)?.as_deref(), Some(b"prefix".as_slice()));
    fixture.revisions[&13].revision().fail();
    assert!(body.next().await.is_some_and(|chunk| chunk.is_err()));
    assert!(body.next().await.is_none());
    assert!(matches!(
        fixture.serve(13, None).await,
        HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Missing)
    ));
    assert!(fixture
        .cached
        .manager
        .access_lease_response_snapshot(
            &fixture.cached.lease_id,
            &fixture.cached.proxy_session_id,
            current_time_millis()
        )
        .await
        .is_none());
    Ok(())
}
