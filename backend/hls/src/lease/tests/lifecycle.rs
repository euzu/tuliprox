use super::{
    lease, manifest_snapshot, timing, HlsAccessLeaseActivation, HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome,
    HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseState, HlsAccessLeaseStore,
    HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason,
};
use crate::ProxySessionId;
use tuliprox_session::ConnectionKind;

#[test]
fn access_lease_idles_at_exact_active_until_boundary() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());

    let snapshot = store.lifecycle_snapshot(&lease_id, 7_000).expect("lease should exist");

    assert_eq!(snapshot.state, HlsAccessLeaseState::Idle);
    assert!(snapshot.idle_release.is_some());
    assert_eq!(store.active_access_lease_count_for_session(&proxy_session_id, 7_000), 0);
}

#[test]
fn access_lease_expires_at_exact_valid_until_boundary() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

    let snapshot = store.lifecycle_snapshot(&lease_id, 17_000).expect("lease should exist");

    assert_eq!(snapshot.state, HlsAccessLeaseState::Expired);
    assert!(snapshot.idle_release.is_some());
    assert_eq!(store.lease_state(&lease_id, 17_000), Some(HlsAccessLeaseState::Expired));
}

#[test]
fn access_lease_expires_without_activity() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert_eq!(
        store.activate_access_lease(&lease_id, &proxy_session_id, 17_000, timing(5_000, 15_000)),
        HlsAccessLeaseActivation::Expired
    );
}

#[test]
fn pending_lease_expires_at_pending_deadline_even_when_valid_window_is_longer() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    let mut lease = lease(lease_id.clone(), &proxy_session_id.0, 1_000);
    lease.pending_deadline = Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 6_000 });
    lease.valid_until_ms = 31_000;
    store.prepare_access_lease(lease);

    assert_eq!(store.lease_state(&lease_id, 5_999), Some(HlsAccessLeaseState::Pending));
    let snapshot = store.lifecycle_snapshot(&lease_id, 6_000).expect("lease should exist");
    assert_eq!(snapshot.state, HlsAccessLeaseState::Expired);
    assert!(snapshot.idle_release.is_some(), "pending expiry must release counted user admission");
    assert_eq!(store.lease_state(&lease_id, 6_000), Some(HlsAccessLeaseState::Expired));
}

#[test]
fn activated_lease_becomes_idle_after_active_window_but_remains_reactivatable() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());

    let snapshot = store.session_snapshot(&proxy_session_id, 8_000);
    assert_eq!(snapshot.active_count, 0);
    assert_eq!(snapshot.idle_releases.len(), 1);
    assert_eq!(snapshot.idle_releases[0].lease_id, lease_id);
    assert_eq!(store.lease_state(&lease_id, 8_000), Some(HlsAccessLeaseState::Idle));
    assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 8_000));
    assert!(store.access_lease(&lease_id, &proxy_session_id, 8_000).is_some());

    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 8_000, timing(5_000, 30_000)).is_activated());
    assert_eq!(store.session_snapshot(&proxy_session_id, 8_000).active_count, 1);
}

#[test]
fn activated_lease_validity_expiry_reports_idle_release() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(30_000, 5_000)).is_activated());

    let snapshot = store.session_snapshot(&proxy_session_id, 8_000);
    assert_eq!(snapshot.active_count, 0);
    assert_eq!(snapshot.idle_releases.len(), 1);
    assert_eq!(snapshot.idle_releases[0].lease_id, lease_id);
    assert_eq!(store.lease_state(&lease_id, 8_000), Some(HlsAccessLeaseState::Expired));
}

#[test]
fn usable_access_lease_query_accepts_pending_idle_and_active_activated_only() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    let pending_id = HlsAccessLeaseId("pending".to_string());
    let idle_id = HlsAccessLeaseId("idle".to_string());
    let activated_id = HlsAccessLeaseId("activated".to_string());
    let denied_id = HlsAccessLeaseId("denied".to_string());
    let expired_id = HlsAccessLeaseId("expired".to_string());

    store.prepare_access_lease(lease(pending_id.clone(), &proxy_session_id.0, 1_000));
    store.prepare_access_lease(lease(idle_id.clone(), &proxy_session_id.0, 1_000));
    store.prepare_access_lease(lease(activated_id.clone(), &proxy_session_id.0, 1_000));
    store.prepare_access_lease(lease(denied_id.clone(), &proxy_session_id.0, 1_000));
    store.prepare_access_lease(lease(expired_id.clone(), &proxy_session_id.0, 1_000));
    assert!(store.activate_access_lease(&idle_id, &proxy_session_id, 2_000, timing(1_000, 15_000)).is_activated());
    assert!(store.activate_access_lease(&activated_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());
    assert!(matches!(
        store.deny_access_lease(&denied_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));

    assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 2_000));
    let snapshot = store.session_snapshot(&proxy_session_id, 3_000);
    assert_eq!(snapshot.active_count, 1);
    assert_eq!(snapshot.idle_releases.len(), 1);
    assert_eq!(snapshot.idle_releases[0].lease_id, idle_id);
    assert_eq!(store.lease_state(&idle_id, 3_000), Some(HlsAccessLeaseState::Idle));
    assert_eq!(store.active_access_lease_count_for_session(&proxy_session_id, 3_000), 1);
    assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 3_000));

    assert!(matches!(
        store.deny_access_lease(&pending_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));
    assert!(matches!(
        store.deny_access_lease(&activated_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));
    assert!(matches!(
        store.deny_access_lease(&expired_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));
    assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 3_000));
    assert!(matches!(
        store.deny_access_lease(&idle_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));
    assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 2_000));
    assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 17_000));
    assert_eq!(store.lease_state(&expired_id, 17_000), Some(HlsAccessLeaseState::Expired));
}

#[test]
fn session_snapshot_ignores_expired_and_denied_origin_policies() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    let denied_id = HlsAccessLeaseId("denied".to_string());
    store.prepare_access_lease(
        lease(denied_id.clone(), &proxy_session_id.0, 1_000).with_origin_acquire_policy(ConnectionKind::Normal, -100),
    );
    store.prepare_access_lease(
        lease(HlsAccessLeaseId("expired".to_string()), &proxy_session_id.0, 1_000)
            .with_origin_acquire_policy(ConnectionKind::Normal, -50),
    );
    store.prepare_access_lease(
        lease(HlsAccessLeaseId("active-soft".to_string()), &proxy_session_id.0, 10_000)
            .with_origin_acquire_policy(ConnectionKind::Soft, 10),
    );
    assert!(matches!(
        store.deny_access_lease(&denied_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    ));

    let snapshot = store.session_snapshot(&proxy_session_id, 17_000);
    let policy = snapshot.effective_origin_policy.expect("usable lease policy");
    assert_eq!(policy.connection_kind, ConnectionKind::Soft);
    assert_eq!(policy.priority, 10);
}

#[test]
fn lease_expiry_between_manifest_publication_prepare_and_commit_rejects_snapshot() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let guard =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("live publication guard");

    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, guard, manifest_snapshot(10), 16_000,),
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseExpired)
    );
    let lease = store.response_snapshot(&lease_id, &proxy_session_id, 16_000).expect("expired lease retained");
    assert_eq!(lease.state, HlsAccessLeaseState::Expired);
    assert!(lease.last_manifest_snapshot.is_none());
}

#[test]
fn expired_lease_lookup_rejects_stale_entry_without_removing_before_lifecycle() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(store.access_lease(&lease_id, &proxy_session_id, 17_000).is_none());
    assert_eq!(store.lease_state(&lease_id, 17_000), Some(HlsAccessLeaseState::Expired));
}
