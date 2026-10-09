use super::{
    cache::HlsPlaybackCursorTracking, finite_hls_media_response, reader::transient_body_object_kind,
    serve_cache_object, serve_hls_segment_cache_outcome, spawn_bounded_media_completion, ActiveReaderStream,
    CacheBodyLogContext, CacheObject, CacheObjectLogContext, CacheObjectServeContext, HlsCacheResponseContext,
    HlsMediaActivityMarker, HlsResourceServeFailure, HlsResourceServeOutcome, PREPARED_MEDIA_CHUNK_SIZE,
};
use crate::{
    media_reserve::HlsLeasePlaybackCursor, CacheAccessState, HlsAccessLease, HlsAccessLeaseId, HlsLogIdentity,
    HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager, HlsSessionHandle,
    HlsSessionKey, OriginSegmentKey, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
    TransientResourceKind,
};
use tuliprox_core::{model::HlsSegmentRepairConfig, utils::current_time_millis};
use tuliprox_session::StreamMeterHandle;

fn test_log_identity() -> HlsLogIdentity { HlsLogIdentity::for_test("content-session", "proxy-session") }

use arc_swap::ArcSwapOption;
use axum::http::{header, HeaderValue, StatusCode};
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::BodyExt;
use shared::model::HlsSegmentRepairMode;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::{sync::Semaphore, time::advance};

fn header(value: &str) -> HeaderValue { HeaderValue::from_str(value).expect("valid header") }

fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
    Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
        max_level: HlsSegmentRepairMode::Off,
        apply_to_first_segments: 1,
        max_parallel_repairs: 1,
        ..Default::default()
    }))
}

async fn live_media_marker(
    manager: &Arc<HlsProxyManager>,
    session: &HlsSessionHandle,
    lease_id: HlsAccessLeaseId,
) -> HlsMediaActivityMarker {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease = HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("test-user", "test-fingerprint"),
        proxy_session_id.clone(),
        "test-user".to_string(),
        "test-session".to_string(),
        1,
        "stream".to_string(),
        1,
        current_time_millis(),
        60_000,
    );
    let lease_identity = lease.media_identity().expect("live lease identity");
    manager.prepare_access_lease(lease).await;
    HlsMediaActivityMarker::new(Arc::clone(manager), Arc::clone(session), proxy_session_id, lease_id, lease_identity)
}

struct CachedSegmentFixture {
    _temp_dir: tempfile::TempDir,
    manager: Arc<HlsProxyManager>,
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    segment_file: HlsSegmentFile,
    access: Arc<CacheAccessState>,
    context: HlsCacheResponseContext,
    meter: Arc<StreamMeterHandle>,
    full_size: u64,
}

impl CachedSegmentFixture {
    async fn new(bytes: Vec<u8>) -> Self {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let segment_cache = Arc::clone(manager.segment_cache());
        let session = manager.get_or_create_session(HlsSessionKey::new(1, "range-stream"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let proxy_seq = 12;
        let cache_key = SegmentCacheKey::new(proxy_session_id.clone(), proxy_seq, "ts");
        segment_cache.write_bytes_and_commit(&cache_key, &bytes).await.expect("segment cache commit");
        let full_size = u64::try_from(bytes.len()).expect("test segment size");
        let access = Arc::new(CacheAccessState::new());
        session.write().await.segments.insert(
            proxy_seq,
            SegmentEntry {
                origin_key: OriginSegmentKey {
                    origin_epoch: 1,
                    effective_host_id: 1,
                    host_local_sequence: proxy_seq,
                    host_local_index: u32::try_from(proxy_seq).expect("test proxy sequence"),
                },
                proxy_seq,
                duration_ms: 4_000,
                proxy_file_ext: "ts".to_string(),
                content_type: "video/mp2t".to_string(),
                cache_key,
                discontinuity_before: false,
                program_date_time: None,
                daterange_tags_before: Vec::new(),
                origin_byte_range: None,
                map_ref: None,
                encryption: None,
                origin_fetch_ref: None,
                status: SegmentCacheStatus::Ready { content_length: full_size, ready_at_ms: 1_000 },
                last_rendered_at_ms: Some(1_000),
                access: Arc::clone(&access),
            },
        );
        let lease_id = HlsAccessLeaseId("range-lease".to_string());
        let marker = live_media_marker(&manager, &session, lease_id.clone()).await;
        let meter = Arc::new(StreamMeterHandle::new(7, Weak::new()));
        let log_identity = {
            let session = session.read().await;
            HlsLogIdentity::from_session(&session)
        };
        let context = HlsCacheResponseContext::new(
            lease_id.clone(),
            log_identity,
            300,
            Arc::clone(manager.metrics()),
            Arc::clone(manager.segment_repair()),
            Some(Arc::clone(&meter)),
            Some(marker),
            current_time_millis(),
        );
        Self {
            _temp_dir: temp_dir,
            manager,
            segment_cache,
            session,
            proxy_session_id,
            lease_id,
            segment_file: HlsSegmentFile { proxy_seq, extension: "ts".to_string() },
            access,
            context,
            meter,
            full_size,
        }
    }

    async fn serve(&self, range: Option<&str>) -> HlsResourceServeOutcome {
        serve_hls_segment_cache_outcome(
            Arc::clone(&self.segment_cache),
            Arc::clone(&self.session),
            self.segment_file.clone(),
            range.map(header),
            &self.context,
        )
        .await
    }

    async fn cursor(&self) -> HlsLeasePlaybackCursor {
        self.manager
            .access_lease_response_snapshot(&self.lease_id, &self.proxy_session_id, current_time_millis())
            .await
            .expect("range test lease")
            .playback_cursor
    }

    async fn wait_for_completion(&self) -> HlsLeasePlaybackCursor {
        for _ in 0..64 {
            let cursor = self.cursor().await;
            if cursor.highest_contiguous_completed_proxy_seq == Some(self.segment_file.proxy_seq) {
                return cursor;
            }
            tokio::task::yield_now().await;
        }
        panic!("segment completion task did not commit")
    }
}

fn ready_response(outcome: HlsResourceServeOutcome) -> axum::response::Response {
    match outcome {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(failure) => panic!("expected ready response, got {failure:?}"),
    }
}

async fn publish_revision_fixture(
    fixture: &CachedSegmentFixture,
    raw: bool,
) -> std::io::Result<crate::SegmentRevisionGuard> {
    let config = tuliprox_core::model::HlsStartupConfig {
        mode: if raw { shared::model::HlsStartupMode::Progressive } else { shared::model::HlsStartupMode::FirstReady },
        ..Default::default()
    };
    let store = Arc::new(crate::SegmentRevisionStore::default());
    let budget = crate::ProgressiveBudgetManager::new(config.clone());
    let revision = store.create(
        fixture.proxy_session_id.clone(),
        fixture.segment_file.proxy_seq,
        if raw { crate::SegmentRevisionKind::Raw } else { crate::SegmentRevisionKind::Processed },
    )?;
    if raw {
        let mut replay =
            crate::progressive_startup::ProgressiveReplay::new(&budget).ok_or_else(|| std::io::Error::other("slot"))?;
        replay.push(b"prefix")?;
        *revision.revision().replay.lock().map_err(|_| std::io::Error::other("replay"))? = Some(replay);
        revision.revision().prefix_available.store(6, Ordering::Release);
    } else {
        let entry = fixture
            .session
            .read()
            .await
            .segments
            .get(&fixture.segment_file.proxy_seq)
            .cloned()
            .ok_or_else(|| std::io::Error::other("segment"))?;
        let pin =
            fixture.segment_cache.pin_revision_file(&fixture.segment_cache.object_path(&revision.revision().key))?;
        *revision.revision().file_pin.lock().map_err(|_| std::io::Error::other("pin"))? = Some(pin);
        let metadata = fixture
            .segment_cache
            .publish_processed_revision(
                &revision.revision().key,
                &fixture.segment_cache.object_path(&entry.cache_key),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await?;
        revision.revision().complete(metadata);
    }
    let revisions = std::collections::BTreeMap::from([(fixture.segment_file.proxy_seq, revision.clone())]);
    fixture.session.write().await.startup = Some(crate::HlsSessionStartup {
        config: config.clone(),
        first_data_timeout_ms: 1000,
        worker: Weak::new(),
        access_leases: Arc::clone(fixture.manager.access_leases()),
        revisions: revisions.clone(),
        policy_fixed: true,
        store,
        budget,
    });
    let snapshot = crate::HlsLeaseManifestSnapshot {
        startup_revisions: Some(Arc::new(crate::HlsManifestRevisions {
            mode: config.mode,
            retained_start_seq: fixture.segment_file.proxy_seq,
            revisions,
        })),
        delivery_mode: crate::HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: crate::HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: current_time_millis(),
        first_proxy_seq: fixture.segment_file.proxy_seq,
        last_proxy_seq: fixture.segment_file.proxy_seq,
        visible_segments: Arc::from([crate::HlsLeaseManifestSegment {
            proxy_seq: fixture.segment_file.proxy_seq,
            duration_ms: 4000,
            uri: "12.ts".into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: 4000,
        playlist_duration_ms: 4000,
        last_visible_media_end_ms: 4000,
        active_map: None,
        active_encryption: None,
        container: crate::HlsMediaContainer::MpegTs,
    };
    let mut leases = fixture.manager.access_leases().write().await;
    let now = current_time_millis();
    let guard = leases
        .prepare_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, now)
        .ok_or_else(|| std::io::Error::other("publication guard"))?;
    assert!(leases
        .commit_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, guard, snapshot, now)
        .is_committed());
    Ok(revision)
}

mod admission;
mod http;
mod lifecycle;
mod policy;
mod recovery;
mod terminal;
