use super::{
    lease, manifest_snapshot, HlsAccessLeaseId, HlsAccessLeaseStore, HlsLeaseManifestPublicationOutcome,
    HlsLeaseManifestPublicationRejectReason,
};
use crate::ProxySessionId;
use tuliprox_session::ConnectionKind;

#[test]
fn session_snapshot_uses_best_priority_within_same_origin_policy_kind() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(
        lease(HlsAccessLeaseId("low-priority".to_string()), &proxy_session_id.0, 1_000)
            .with_origin_acquire_policy(ConnectionKind::Normal, 30),
    );
    store.prepare_access_lease(
        lease(HlsAccessLeaseId("high-priority".to_string()), &proxy_session_id.0, 1_000)
            .with_origin_acquire_policy(ConnectionKind::Normal, -5),
    );

    let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
    let policy = snapshot.effective_origin_policy.expect("usable lease policy");
    assert_eq!(policy.connection_kind, ConnectionKind::Normal);
    assert_eq!(policy.priority, -5);
}

#[test]
fn changed_admission_generation_rejects_manifest_publication() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let guard =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("original admission guard");
    let lease = store.by_lease_id.get_mut(&lease_id).expect("stored lease");
    lease.admission_generation = lease.admission_generation.saturating_add(1);

    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, guard, manifest_snapshot(10), 2_100,),
        HlsLeaseManifestPublicationOutcome::Rejected(
            HlsLeaseManifestPublicationRejectReason::AdmissionGenerationChanged
        )
    );
}
