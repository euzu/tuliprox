use super::{
    lease, manifest_snapshot, HlsAccessLeaseId, HlsAccessLeaseStore, HlsLeaseManifestPublicationOutcome,
    HlsLeaseManifestPublicationRejectReason, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
};
use crate::ProxySessionId;

#[test]
fn commit_generation_orders_cross_mode_sources_rendered_in_the_same_millisecond() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let older_request =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("older request guard");
    let newer_request =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("newer request guard");
    let mut newer = manifest_snapshot(20);
    newer.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;
    newer.source_commit_identity = HlsManifestCommitIdentity::committed(2, 20);
    newer.finalized_transient_manifest_generation = Some(super::super::super::TransientManifestGeneration::for_test(2));
    let mut older = manifest_snapshot(20);
    older.delivery_mode = HlsManifestDeliveryMode::NormalCacheTimeline;
    older.source_commit_identity = HlsManifestCommitIdentity::committed(1, 20);
    older.finalized_transient_manifest_generation = None;

    assert!(store
        .commit_manifest_publication(&lease_id, &proxy_session_id, newer_request, newer, 2_100)
        .is_committed());
    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, older_request, older, 2_200),
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::SourceRegressive)
    );

    let lease_id = HlsAccessLeaseId("lease-b".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let older_request =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("older request guard");
    let newer_request =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("newer request guard");
    let mut newer = manifest_snapshot(20);
    newer.source_commit_identity = HlsManifestCommitIdentity::committed(4, 20);
    let mut older = manifest_snapshot(20);
    older.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;
    older.source_commit_identity = HlsManifestCommitIdentity::committed(3, 20);
    older.finalized_transient_manifest_generation = Some(super::super::super::TransientManifestGeneration::for_test(1));

    assert!(store
        .commit_manifest_publication(&lease_id, &proxy_session_id, newer_request, newer, 2_100)
        .is_committed());
    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, older_request, older, 2_200),
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::SourceRegressive)
    );
}
