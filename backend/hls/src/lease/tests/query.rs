use super::{lease, HlsAccessLeaseId, HlsAccessLeaseStore, HlsAvailabilityEvidenceGeneration};
use crate::ProxySessionId;

#[test]
fn hls_availability_reevaluation_evidence_generation_fails_closed_on_overflow() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("availability-evidence-overflow".to_string());
    let lease_id = HlsAccessLeaseId("availability-evidence-overflow".to_string());
    store.last_availability_evidence_generation = HlsAvailabilityEvidenceGeneration::for_test(u64::MAX);

    assert!(!store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
    assert!(store.response_snapshot(&lease_id, &proxy_session_id, 1_000).is_none());
    assert_eq!(store.availability_evidence_generation(&proxy_session_id).as_u64(), 0);
}
