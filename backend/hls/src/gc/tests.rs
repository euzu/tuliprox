use super::{
    super::{
        media_reserve::{
            HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
        },
        terminal_tail::{HlsEncryptionSignature, HlsMediaContainer, HlsTerminalTailGeneration},
    },
    build_rewrite_secret_fingerprint,
    deletion::{CacheObjectDeletion, SegmentCacheDeletionReason},
    selection::{oldest_global_fifo_head_candidate, remove_segment_entry},
    GarbageCollectionPolicy, GarbageCollectionReport, HlsGarbageCollector, PendingCacheObjectDeletion, ProtectedSet,
    MAX_PENDING_CACHE_DELETIONS, SWITCH_CACHE_CLEANUP_HEADROOM,
};
use crate::{
    prepare_terminal_base_evidence, transient_manifest::TransientManifestRewriter, HlsAccessLease, HlsAccessLeaseId,
    HlsAccessLeaseStore, HlsOriginResourceFetchError, HlsPlaybackFamilyKey, HlsSegmentCache, HlsSegmentEncryption,
    HlsSession, HlsSessionKey, HlsSessionStore, HlsTerminalTailProtection, MapCacheStatus, OriginMapKey, ProxyMapId,
    ProxySessionId, SegmentCacheStatus, SegmentFetchPriority, TransientObjectFetchDecision, TransientPassthroughState,
    TransientResourceId, TransientResourceKind, TransientResourceRef,
};
use std::{collections::HashSet, fmt::Write as _, sync::Arc, task::Poll, time::Duration};
use tokio::sync::{Barrier, RwLock};
use tuliprox_parser::hls::origin_manifest::{
    parse_manifest_semantics, parse_origin_media_manifest, OriginManifestParseOutcome,
};

const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

fn normal_manifest(body: &str) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
    match parse_origin_media_manifest(body, BASE_URL) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { reason } => {
            panic!("expected normal manifest: {reason:?}")
        }
    }
}

fn test_policy() -> GarbageCollectionPolicy {
    GarbageCollectionPolicy {
        cache_duration_ms: 300,
        cache_bytes_global: 10_000,
        cache_bytes_per_session: 10_000,
        session_idle_timeout_ms: 1_000,
        temp_file_retention_ms: 30_000,
        failed_segment_retention_ms: 10,
    }
}

async fn gc_with_session(temp_dir: &tempfile::TempDir) -> (Arc<HlsGarbageCollector>, crate::HlsSessionHandle) {
    let sessions = Arc::new(HlsSessionStore::new());
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    let gc = Arc::new(HlsGarbageCollector::new(
        sessions,
        Arc::clone(&cache),
        test_policy(),
        build_rewrite_secret_fingerprint(b"secret"),
    ));
    cache.install_capacity_reclaimer(&gc);
    (gc, session)
}

fn update_gc_policy(gc: &HlsGarbageCollector, update: impl FnOnce(&mut GarbageCollectionPolicy)) {
    let mut policy = gc.policy().as_ref().clone();
    update(&mut policy);
    gc.update_config(policy, gc.rewrite_secret_fingerprint());
}

fn six_segment_manifest() -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
    normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n#EXTINF:4.0,\n4.ts\n#EXTINF:4.0,\n5.ts\n#EXTINF:4.0,\n6.ts\n",
        )
}

fn apply_six_segment_manifest_for_gc(session: &mut super::HlsSession) {
    session.proxy_next_seq = Some(1);
    session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
}

fn install_finalized_transient_manifest(
    session: &mut HlsSession,
    segment_count: usize,
    resource_ttl_ms: u64,
) -> Vec<TransientResourceRef> {
    let mut body =
        String::from("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n");
    for index in 0..segment_count {
        writeln!(body, "#EXTINF:6.000,\n{index}.ts").expect("synthetic finalized manifest renders");
    }
    body.push_str("#EXT-X-ENDLIST\n");
    let rewritten =
        TransientManifestRewriter::rewrite(&body, BASE_URL, &session.proxy_session_id, b"secret", 0, resource_ttl_ms);
    let resources = rewritten.resources.clone();
    session
        .transient
        .commit_rewritten_manifest_with_semantics(
            rewritten.body,
            rewritten.resources,
            0,
            Some(u64::try_from(segment_count).expect("segment count fits u64").saturating_mul(6_000)),
            parse_manifest_semantics(&body),
        )
        .expect("synthetic finalized manifest stays within representation limits");
    resources
}

async fn populate_ready_segments(gc: &HlsGarbageCollector, session: &crate::HlsSessionHandle, ready_at_ms: u64) {
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for segment in session.segments.values_mut() {
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
        }
        session.render_and_store_manifest(ready_at_ms).expect("ready manifest should render");
    }
}

async fn cache_selected_ready_segments(
    gc: &HlsGarbageCollector,
    session: &crate::HlsSessionHandle,
    proxy_seqs: &[u64],
    body: &[u8],
) {
    let keys = {
        let session = session.read().await;
        proxy_seqs
            .iter()
            .map(|proxy_seq| (*proxy_seq, session.segments.get(proxy_seq).expect("segment").cache_key.clone()))
            .collect::<Vec<_>>()
    };
    for (_, key) in &keys {
        gc.cache.write_bytes_and_commit(key, body).await.expect("cache fixture writes");
    }
    let mut session = session.write().await;
    for (proxy_seq, _) in keys {
        session.segments.get_mut(&proxy_seq).expect("segment").status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(body.len()).unwrap_or(u64::MAX),
            ready_at_ms: proxy_seq,
        };
    }
}

async fn commit_sparse_segment(gc: &HlsGarbageCollector, key: &crate::SegmentCacheKey, size: u64) {
    let final_path = gc.cache.object_path(key);
    let parent = final_path.parent().expect("cache object parent");
    tokio::fs::create_dir_all(parent).await.expect("cache object parent creates");
    let temp_path = final_path.with_extension(format!("ts.tmp.fixture-{}", key.proxy_seq()));
    let file = tokio::fs::File::create(&temp_path).await.expect("sparse temp creates");
    file.set_len(size).await.expect("sparse temp length");
    drop(file);
    let staged = gc.cache.adopt_staged_file(temp_path, size).expect("sparse staged object adopts");
    gc.cache.commit_staged(key, staged).await.expect("sparse object commits");
}

fn activated_startup_lease(proxy_session_id: &ProxySessionId) -> HlsAccessLease {
    let snapshot = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(10),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 10,
        first_proxy_seq: 1,
        last_proxy_seq: 3,
        visible_segments: Arc::from(
            (1_u64..=3)
                .map(|proxy_seq| HlsLeaseManifestSegment {
                    proxy_seq,
                    duration_ms: 4_000,
                    uri: format!("/hls/shared/live/session/lease/{proxy_seq:06}.ts").into(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                })
                .collect::<Vec<_>>(),
        ),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 12_000,
        last_visible_media_end_ms: 12_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    };
    let mut lease = HlsAccessLease::pending(
        HlsAccessLeaseId("startup-lease".to_string()),
        HlsPlaybackFamilyKey::new("user", "player"),
        proxy_session_id.clone(),
        "user".to_string(),
        "user-session".to_string(),
        1,
        "12345".to_string(),
        1,
        10,
        60_000,
    );
    lease.state = super::super::HlsAccessLeaseState::Activated;
    lease.active_until_ms = Some(60_000);
    lease.last_manifest_snapshot = Some(snapshot);
    lease
}

fn encrypted_terminal_evidence_manifest(
    proxy_session_id: &ProxySessionId,
    resource_id: &TransientResourceId,
) -> HlsLeaseManifestSnapshot {
    let encryption = Arc::new(HlsEncryptionSignature {
        method: "AES-128".to_string(),
        key_uri: Some(format!("/hls/shared/live/{}/lease/r/{}.key", proxy_session_id.0, resource_id.0)),
        iv: Some("0x00000000000000000000000000000001".to_string()),
        key_format: Some("identity".to_string()),
        key_format_versions: Some("1".to_string()),
        can_reset_to_clear: true,
    });
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 0,
        first_proxy_seq: 1,
        last_proxy_seq: 1,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: 1,
            duration_ms: 4_000,
            uri: "/hls/shared/live/session/lease/1.ts".to_string().into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: Some(encryption.clone()),
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 4_000,
        last_visible_media_end_ms: 4_000,
        active_map: None,
        active_encryption: Some(encryption),
        container: HlsMediaContainer::MpegTs,
    }
}

mod admission;
mod lifecycle;
mod playlist;
mod policy;
mod publication;
mod recovery;
mod streaming;
mod transport;
