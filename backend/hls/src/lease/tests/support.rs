use super::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsLeaseManifestPublicationOutcome,
    HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
    HlsMediaContainer, HlsPlaybackFamilyKey,
};
use crate::ProxySessionId;
use std::sync::Arc;

pub(in crate::lease::tests) fn lease(
    lease_id: HlsAccessLeaseId,
    proxy_session_id: &str,
    now_ms: u64,
) -> HlsAccessLease {
    HlsAccessLease::pending(
        lease_id,
        HlsPlaybackFamilyKey::new("alice", "client-a"),
        ProxySessionId(proxy_session_id.to_string()),
        "alice".to_string(),
        "session-a".to_string(),
        1,
        "12345".to_string(),
        12345,
        now_ms,
        15_000,
    )
}

pub(in crate::lease::tests) const fn timing(active_window_ms: u64, valid_window_ms: u64) -> HlsAccessLeaseTiming {
    HlsAccessLeaseTiming { active_window_ms, valid_window_ms }
}

pub(in crate::lease::tests) fn manifest_snapshot(source_rendered_at_ms: u64) -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(source_rendered_at_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: 2_000,
        first_proxy_seq: 40,
        last_proxy_seq: 41,
        visible_segments: Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 40,
                duration_ms: 6_000,
                uri: "/live/40.ts".to_string().into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 41,
                duration_ms: 6_000,
                uri: "/live/41.ts".to_string().into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]),
        discontinuity_sequence: 3,
        target_duration_ms: 12_000,
        playlist_duration_ms: 12_000,
        last_visible_media_end_ms: 12_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(in crate::lease::tests) fn publish_manifest_snapshot(
    store: &mut HlsAccessLeaseStore,
    lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    snapshot: HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> HlsLeaseManifestPublicationOutcome {
    let guard =
        store.prepare_manifest_publication(lease_id, proxy_session_id, now_ms).expect("live manifest publication");
    store.commit_manifest_publication(lease_id, proxy_session_id, guard, snapshot, now_ms)
}
