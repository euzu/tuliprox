use super::{
    lease, manifest_snapshot, publish_manifest_snapshot, timing, HlsAccessLeaseActivation, HlsAccessLeaseId,
    HlsAccessLeaseState, HlsAccessLeaseStore,
};
use crate::ProxySessionId;

#[test]
fn old_media_access_cannot_activate_a_new_idle_entry() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy = ProxySessionId("strip-entry".into());
    let id = HlsAccessLeaseId("same-viewer".into());
    store.prepare_access_lease(lease(id.clone(), &proxy.0, 1_000));
    assert!(publish_manifest_snapshot(&mut store, &id, &proxy, manifest_snapshot(1), 2_000).is_committed());
    assert!(store.activate_access_lease(&id, &proxy, 2_001, timing(1_000, 60_000)).is_activated());
    assert_eq!(store.session_snapshot(&proxy, 3_002).idle_releases.len(), 1);
    assert_eq!(store.lease_state(&id, 3_002), Some(HlsAccessLeaseState::Idle));
    assert_eq!(
        store.activate_access_lease(&id, &proxy, 3_003, timing(1_000, 60_000)),
        HlsAccessLeaseActivation::AvailabilityPending
    );
    assert_eq!(store.lease_state(&id, 3_003), Some(HlsAccessLeaseState::Idle));
}

#[test]
fn multiple_idle_cycles_create_one_entry_per_transition() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy = ProxySessionId("cycles".into());
    let id = HlsAccessLeaseId("stable-id".into());
    store.prepare_access_lease(lease(id.clone(), &proxy.0, 1_000));
    let mut now = 2_000;
    for generation in 1..=4 {
        let current = store.response_snapshot(&id, &proxy, now).unwrap();
        assert_eq!(current.entry_generation(), Some(generation));
        let issued = current.issued_at_ms;
        let admission = current.admission_generation;
        assert!(publish_manifest_snapshot(&mut store, &id, &proxy, manifest_snapshot(generation), now).is_committed());
        let published = store.response_snapshot(&id, &proxy, now).unwrap();
        assert!(published.has_current_entry_manifest());
        assert_eq!(published.issued_at_ms, issued);
        assert_eq!(published.admission_generation, admission);
        assert!(store.activate_access_lease(&id, &proxy, now + 1, timing(1_000, 60_000)).is_activated());
        if generation == 4 {
            break;
        }
        now += 1_002;
        assert_eq!(store.session_snapshot(&proxy, now).idle_releases.len(), 1);
        for _ in 0..3 {
            assert_eq!(store.session_snapshot(&proxy, now).idle_releases.len(), 0);
            assert_eq!(store.response_snapshot(&id, &proxy, now).unwrap().entry_generation(), Some(generation + 1));
        }
    }
}

#[test]
fn old_completion_stays_in_its_published_entry_context() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy = ProxySessionId("late-completion".into());
    let id = HlsAccessLeaseId("same-viewer".into());
    store.prepare_access_lease(lease(id.clone(), &proxy.0, 1_000));
    assert!(publish_manifest_snapshot(&mut store, &id, &proxy, manifest_snapshot(1), 2_000).is_committed());
    assert!(store.activate_access_lease(&id, &proxy, 2_001, timing(1_000, 60_000)).is_activated());
    let old_identity = store.response_snapshot(&id, &proxy, 2_001).unwrap().media_identity().unwrap();
    let token = store.record_segment_request_started_if_identity_matches(&id, &proxy, old_identity, 40, 2_002).unwrap();
    store.session_snapshot(&proxy, 3_002);
    assert_eq!(store.response_snapshot(&id, &proxy, 3_002).unwrap().media_identity(), Some(old_identity));
    assert!(publish_manifest_snapshot(&mut store, &id, &proxy, manifest_snapshot(1), 3_003).is_committed());
    let current = store.response_snapshot(&id, &proxy, 3_003).unwrap().media_identity().unwrap();
    assert_ne!(old_identity, current);
    assert!(old_identity.same_playback_authorization(current));
    assert!(store
        .record_segment_request_completed_if_identity_matches(&id, &proxy, old_identity, token, 3_004)
        .is_none());
}

#[test]
fn entry_generation_exhaustion_never_wraps_or_publishes() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy = ProxySessionId("exhausted".into());
    let id = HlsAccessLeaseId("same-viewer".into());
    let mut current = lease(id.clone(), &proxy.0, 1_000);
    current.entry_generation = Some(u64::MAX);
    current.admitted_entry_generation = Some(u64::MAX);
    current.progressive_startup_claimed = true;
    store.prepare_access_lease(current);
    assert!(store.activate_access_lease(&id, &proxy, 2_000, timing(1_000, 60_000)).is_activated());
    store.session_snapshot(&proxy, 3_001);
    let current = store.response_snapshot(&id, &proxy, 3_001).unwrap();
    assert_eq!(current.entry_generation(), None);
    assert!(current.progressive_startup_claimed);
    assert!(store.prepare_manifest_publication(&id, &proxy, 3_001).is_none());
    assert_eq!(
        store.activate_access_lease(&id, &proxy, 3_002, timing(1_000, 60_000)),
        HlsAccessLeaseActivation::AvailabilityPending
    );
}

#[test]
fn entry_wait_deadline_does_not_restart_for_polls_or_policy_retries() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy = ProxySessionId("wait-budget".into());
    let id = HlsAccessLeaseId("same-viewer".into());
    store.prepare_access_lease(lease(id.clone(), &proxy.0, 1_000));
    assert_eq!(store.bound_entry_wait(&id, &proxy, 1, 3_000), Some(3_000));
    assert_eq!(store.bound_entry_wait(&id, &proxy, 1, 5_000), Some(3_000));
    assert_eq!(store.bound_entry_wait(&id, &proxy, 1, 2_500), Some(2_500));
    assert_eq!(store.bound_entry_wait(&id, &proxy, 2, 5_000), None);
}
