use super::{
    lease, manifest_snapshot, new_hls_access_lease_id, publish_manifest_snapshot, timing, HlsAccessLease,
    HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseState,
    HlsAccessLeaseStore, HlsAccessLeaseTouch, HlsLeaseManifestPublicationOutcome,
    HlsLeaseManifestPublicationRejectReason, HlsLeasePlaybackMode, HlsManifestCommitIdentity,
    HlsPlaybackCompletionOutcome, HlsPlaybackFamilyKey, HlsRuntimeCustomTailReason, HlsTerminalTailCompatibility,
};
use crate::ProxySessionId;
use std::sync::Arc;
use tuliprox_session::ConnectionKind;

#[test]
fn known_bitrate_builder_normalizes_zero_and_survives_response_snapshot() {
    let proxy_session_id = ProxySessionId("proxy-a".to_string());
    let zero_lease_id = HlsAccessLeaseId("lease-zero".to_string());
    let measured_lease_id = HlsAccessLeaseId("lease-measured".to_string());
    let mut store = HlsAccessLeaseStore::default();
    assert!(store.prepare_access_lease(
        lease(zero_lease_id.clone(), &proxy_session_id.0, 1_000).with_known_bitrate_bps(Some(0))
    ));
    assert!(store.prepare_access_lease(
        lease(measured_lease_id.clone(), &proxy_session_id.0, 1_000).with_known_bitrate_bps(Some(2_500_000)),
    ));

    assert_eq!(
        store
            .response_snapshot(&zero_lease_id, &proxy_session_id, 2_000)
            .expect("zero bitrate lease")
            .known_bitrate_bps,
        None
    );
    assert_eq!(
        store
            .response_snapshot(&measured_lease_id, &proxy_session_id, 2_000)
            .expect("measured bitrate lease")
            .known_bitrate_bps,
        Some(2_500_000)
    );
}

#[test]
fn standalone_tail_policy_is_limited_to_pending_unpublished_lease() {
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let mut candidate = lease(lease_id, "proxy-a", 1_000);

    assert!(candidate.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));
    assert!(!candidate.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::SessionOrLeaseExpired));

    candidate.last_manifest_snapshot = Some(manifest_snapshot(1));
    candidate.startup_admission = super::super::HlsLeaseStartupAdmissionState::Admitted;
    assert!(!candidate.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));

    let mut activated = lease(HlsAccessLeaseId("lease-b".to_string()), "proxy-a", 1_000);
    activated.state = HlsAccessLeaseState::Activated;
    assert!(!activated.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));

    let mut terminal = lease(HlsAccessLeaseId("lease-c".to_string()), "proxy-a", 1_000);
    terminal.playback_mode = HlsLeasePlaybackMode::TerminalUnavailable {
        decision_generation: 1,
        reason: HlsTerminalTailCompatibility::MissingAsset,
    };
    assert!(!terminal.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));
}

#[test]
fn access_lease_id_is_short_and_opaque() {
    let lease_id = new_hls_access_lease_id();

    assert_eq!(lease_id.0.len(), 22);
    assert!(!lease_id.0.contains("alice"));
    assert!(!lease_id.0.contains("session"));
}

#[test]
fn archive_playback_context_is_opt_in() {
    let live = lease(HlsAccessLeaseId("live".to_string()), "proxy-live", 1_000);
    assert_eq!(live.epg_reference_ts, None);
    assert_eq!(live.archive_origin_url, None);

    let archive = lease(HlsAccessLeaseId("archive".to_string()), "proxy-archive", 1_000).with_archive_playback(
        Some(1_784_898_000),
        Some("http://provider/channel/timeshift_abs-1784898000.m3u8".to_string()),
    );
    assert_eq!(archive.epg_reference_ts, Some(1_784_898_000));
    assert_eq!(archive.archive_origin_url.as_deref(), Some("http://provider/channel/timeshift_abs-1784898000.m3u8"));
}

#[test]
fn access_lease_activates_and_slides_validity() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 10_000, timing(5_000, 30_000)).is_activated());
    assert_eq!(store.lease_state(&lease_id, 24_999), Some(HlsAccessLeaseState::Activated));
    assert!(store.touch_access_lease(&lease_id, 24_000, timing(5_000, 30_000)));
    assert_eq!(store.lease_state(&lease_id, 53_999), Some(HlsAccessLeaseState::Activated));
}

#[test]
fn same_family_leases_remain_independently_valid() {
    let mut store = HlsAccessLeaseStore::default();
    let old_lease_id = HlsAccessLeaseId("old".to_string());
    let new_lease_id = HlsAccessLeaseId("new".to_string());
    let proxy_a = ProxySessionId("proxy-a".to_string());
    let proxy_b = ProxySessionId("proxy-b".to_string());
    let family = HlsPlaybackFamilyKey::new("alice", "client-a");

    store.prepare_access_lease(HlsAccessLease::pending(
        old_lease_id.clone(),
        family.clone(),
        proxy_a.clone(),
        "alice".to_string(),
        "session-a".to_string(),
        1,
        "12345".to_string(),
        12345,
        1_000,
        15_000,
    ));
    assert!(store.activate_access_lease(&old_lease_id, &proxy_a, 2_000, timing(5_000, 15_000)).is_activated());
    store.prepare_access_lease(HlsAccessLease::pending(
        new_lease_id.clone(),
        family,
        proxy_b.clone(),
        "alice".to_string(),
        "session-b".to_string(),
        1,
        "67890".to_string(),
        67890,
        3_000,
        15_000,
    ));

    let activation = store.activate_access_lease(&new_lease_id, &proxy_b, 4_000, timing(5_000, 15_000));
    assert!(activation.is_activated());
    assert_eq!(store.lease_state(&old_lease_id, 4_000), Some(HlsAccessLeaseState::Activated));
    assert!(store.touch_access_lease(&old_lease_id, 5_000, timing(5_000, 15_000)));
    assert_eq!(store.lease_state(&old_lease_id, 19_999), Some(HlsAccessLeaseState::Activated));
}

#[test]
fn manifest_touch_extends_activated_lease_active_window() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

    assert!(matches!(
        store.touch_manifest_access_lease(
            &lease_id,
            &proxy_session_id,
            6_000,
            Some(timing(10_000, 30_000)),
            None,
            15_000,
        ),
        HlsAccessLeaseTouch::Touched { .. }
    ));
    let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
    assert_eq!(lease.state, HlsAccessLeaseState::Activated);
    assert_eq!(lease.last_seen_at_ms, 6_000);
    assert_eq!(lease.pending_deadline, None);
    assert_eq!(lease.active_until_ms, Some(16_000));
    assert_eq!(lease.valid_until_ms, 36_000);
}

#[test]
fn manifest_touch_can_shorten_pending_lease_to_follow_up_deadline() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(matches!(
        store.touch_manifest_access_lease(
            &lease_id,
            &proxy_session_id,
            2_000,
            None,
            Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }),
            300_000,
        ),
        HlsAccessLeaseTouch::Touched { .. }
    ));

    let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
    assert_eq!(lease.state, HlsAccessLeaseState::Pending);
    assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
    assert_eq!(lease.valid_until_ms, 12_000);
    assert_eq!(store.lease_state(&lease_id, 11_999), Some(HlsAccessLeaseState::Pending));
    assert_eq!(store.lease_state(&lease_id, 12_000), Some(HlsAccessLeaseState::Expired));
}

#[test]
fn bootstrap_touch_cannot_extend_existing_follow_up_pending_deadline() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(matches!(
        store.touch_manifest_access_lease(
            &lease_id,
            &proxy_session_id,
            2_000,
            None,
            Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }),
            300_000,
        ),
        HlsAccessLeaseTouch::Touched { .. }
    ));
    assert!(matches!(
        store.touch_manifest_access_lease(
            &lease_id,
            &proxy_session_id,
            3_000,
            None,
            Some(HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms: 100_000 }),
            300_000,
        ),
        HlsAccessLeaseTouch::Touched { .. }
    ));

    let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
    assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
    assert_eq!(lease.valid_until_ms, 12_000);
}

#[test]
fn repeated_follow_up_touch_cannot_extend_existing_pending_deadline() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(matches!(
        store.touch_manifest_access_lease(
            &lease_id,
            &proxy_session_id,
            2_000,
            None,
            Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }),
            300_000,
        ),
        HlsAccessLeaseTouch::Touched { .. }
    ));
    assert!(matches!(
        store.touch_manifest_access_lease(
            &lease_id,
            &proxy_session_id,
            3_000,
            None,
            Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 30_000 }),
            300_000,
        ),
        HlsAccessLeaseTouch::Touched { .. }
    ));

    let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
    assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
    assert_eq!(lease.valid_until_ms, 12_000);
}

#[test]
fn session_follow_up_shortens_pending_lease_once() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    let shortened = store.mark_pending_manifest_follow_up_for_session(
        &proxy_session_id,
        2_000,
        HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 },
    );
    assert_eq!(shortened.len(), 1);
    let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
    assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
    assert_eq!(lease.valid_until_ms, 12_000);

    let unchanged = store.mark_pending_manifest_follow_up_for_session(
        &proxy_session_id,
        3_000,
        HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 30_000 },
    );
    assert!(unchanged.is_empty());
    let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
    assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
    assert_eq!(lease.valid_until_ms, 12_000);
}

#[test]
fn activated_lease_remains_valid_after_media_touch() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

    assert!(store.touch_access_lease(&lease_id, 3_000, timing(5_000, 15_000)));
    assert_eq!(store.lease_state(&lease_id, 17_999), Some(HlsAccessLeaseState::Activated));
}

#[test]
fn session_snapshot_prefers_normal_origin_policy_over_soft() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(
        lease(HlsAccessLeaseId("soft".to_string()), &proxy_session_id.0, 1_000)
            .with_origin_acquire_policy(ConnectionKind::Soft, -20),
    );
    store.prepare_access_lease(
        lease(HlsAccessLeaseId("normal".to_string()), &proxy_session_id.0, 1_000)
            .with_origin_acquire_policy(ConnectionKind::Normal, 50),
    );

    let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
    let policy = snapshot.effective_origin_policy.expect("usable lease policy");
    assert_eq!(policy.connection_kind, ConnectionKind::Normal);
    assert_eq!(policy.priority, 50);
}

#[test]
fn origin_policy_update_reclassifies_existing_access_lease() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    store.prepare_access_lease(
        lease(lease_id.clone(), &proxy_session_id.0, 1_000).with_origin_acquire_policy(ConnectionKind::Soft, 20),
    );

    let updated =
        store.update_origin_acquire_policy(&lease_id, ConnectionKind::Normal, -5).expect("lease should update");
    assert_eq!(updated.origin_connection_kind, ConnectionKind::Normal);
    assert_eq!(updated.origin_priority, -5);

    let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
    let policy = snapshot.effective_origin_policy.expect("updated policy");
    assert_eq!(policy.connection_kind, ConnectionKind::Normal);
    assert_eq!(policy.priority, -5);
}

#[test]
fn lease_manifest_generation_advances_for_snapshots_rendered_in_the_same_millisecond() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

    assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(2_000), 2_000)
        .is_committed());
    let first = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("first manifest snapshot");
    assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(2_000), 2_000)
        .is_committed());
    let second = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("second manifest snapshot");

    assert_eq!(first.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation), Some(1));
    assert_eq!(second.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation), Some(2));
    assert_eq!(
        first.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.delivered_at_ms),
        second.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.delivered_at_ms)
    );
}

#[test]
fn newer_source_publication_wins_before_delayed_older_request() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let older_request =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("older request guard");
    let newer_request =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("newer request guard");

    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, newer_request, manifest_snapshot(20), 2_100,),
        HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: 1 }
    );
    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, older_request, manifest_snapshot(10), 2_200,),
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::SourceRegressive)
    );
    let current = store
        .response_snapshot(&lease_id, &proxy_session_id, 2_200)
        .and_then(|lease| lease.last_manifest_snapshot)
        .expect("newer snapshot remains current");
    assert_eq!(current.source_commit_identity, HlsManifestCommitIdentity::new(20));
    assert_eq!(current.snapshot_generation, 1);
}

#[test]
fn replacement_lease_incarnation_rejects_older_manifest_publication() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let guard =
        store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("original incarnation guard");
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 3_000));

    assert_eq!(
        store.commit_manifest_publication(&lease_id, &proxy_session_id, guard, manifest_snapshot(10), 3_100,),
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseIncarnationChanged)
    );
}

#[test]
fn late_segment_completion_after_forward_seek_is_stale_and_does_not_advance_cursor() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    let identity = store
        .response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .and_then(|lease| lease.media_identity())
        .expect("live identity");
    let stale_token = store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_000)
        .expect("first request token");
    let _current_token = store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 50, 2_100)
        .expect("forward-seek request token");

    assert_eq!(
        store.record_segment_request_completed_if_identity_matches(
            &lease_id,
            &proxy_session_id,
            identity,
            stale_token,
            2_200,
        ),
        Some(HlsPlaybackCompletionOutcome::StaleRequest)
    );
    let lease = store.response_snapshot(&lease_id, &proxy_session_id, 2_300).expect("live lease");
    assert_eq!(lease.playback_cursor.first_requested_proxy_seq, Some(50));
    assert_eq!(lease.playback_cursor.highest_contiguous_completed_proxy_seq, None);
    assert_eq!(lease.playback_cursor.first_segment_completed_at_ms, None);
}

#[test]
fn hls_availability_reevaluation_evidence_generation_is_per_session_and_noop_stable() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_a = ProxySessionId("availability-evidence-a".to_string());
    let proxy_b = ProxySessionId("availability-evidence-b".to_string());
    let primary_lease_id = HlsAccessLeaseId("availability-evidence-a".to_string());
    let secondary_lease_id = HlsAccessLeaseId("availability-evidence-b".to_string());
    let lease_a = lease(primary_lease_id.clone(), &proxy_a.0, 1_000);
    assert!(store.prepare_access_lease(lease_a.clone()));
    let added_a = store.availability_evidence_generation(&proxy_a);
    assert!(added_a.as_u64() > 0);

    assert!(store.prepare_access_lease(lease_a));
    assert_eq!(store.availability_evidence_generation(&proxy_a), added_a);
    assert!(matches!(
        store.activate_access_lease(&primary_lease_id, &proxy_b, 2_000, timing(5_000, 30_000)),
        HlsAccessLeaseActivation::SessionMismatch
    ));
    assert_eq!(store.availability_evidence_generation(&proxy_a), added_a);

    assert!(store.prepare_access_lease(lease(secondary_lease_id, &proxy_b.0, 1_000)));
    assert_eq!(store.availability_evidence_generation(&proxy_a), added_a);
    assert!(store.availability_evidence_generation(&proxy_b).as_u64() > added_a.as_u64());

    assert!(store.activate_access_lease(&primary_lease_id, &proxy_a, 2_000, timing(5_000, 30_000)).is_activated());
    let activated_a = store.availability_evidence_generation(&proxy_a);
    assert!(activated_a > added_a);
    assert_eq!(store.availability_evidence_generation(&proxy_b).as_u64(), added_a.as_u64().saturating_add(1));

    let identity = store
        .response_snapshot(&primary_lease_id, &proxy_a, 2_000)
        .and_then(|lease| lease.media_identity())
        .expect("activated live lease identity");
    let token = store
        .record_segment_request_started_if_identity_matches(&primary_lease_id, &proxy_a, identity, 7, 2_100)
        .expect("cursor request starts");
    let requested_a = store.availability_evidence_generation(&proxy_a);
    assert!(requested_a > activated_a);
    assert_eq!(
        store
            .record_segment_request_completed_if_identity_matches(&primary_lease_id, &proxy_a, identity, token, 2_200,),
        Some(HlsPlaybackCompletionOutcome::Advanced)
    );
    let completed_a = store.availability_evidence_generation(&proxy_a);
    assert!(completed_a > requested_a);
    assert_eq!(
        store
            .record_segment_request_completed_if_identity_matches(&primary_lease_id, &proxy_a, identity, token, 2_300,),
        Some(HlsPlaybackCompletionOutcome::Duplicate)
    );
    assert_eq!(store.availability_evidence_generation(&proxy_a), completed_a);

    assert!(store.remove_access_lease(&primary_lease_id).is_some());
    let removed_a = store.availability_evidence_generation(&proxy_a);
    assert!(removed_a > completed_a);
    assert_eq!(store.availability_evidence_generation(&proxy_b).as_u64(), added_a.as_u64().saturating_add(1));
    assert!(store.prepare_access_lease(lease(primary_lease_id, &proxy_a.0, 3_000)));
    assert!(store.availability_evidence_generation(&proxy_a) > removed_a);
}

#[test]
fn bulk_lease_removal_advances_and_preserves_session_evidence_generation() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("bulk-removal".to_string());
    let first = HlsAccessLeaseId("bulk-removal-a".to_string());
    let second = HlsAccessLeaseId("bulk-removal-b".to_string());
    assert!(store.prepare_access_lease(lease(first, &proxy_session_id.0, 1_000)));
    assert!(store.prepare_access_lease(lease(second, &proxy_session_id.0, 1_001)));
    let before_removal = store.availability_evidence_generation(&proxy_session_id);

    assert_eq!(store.remove_access_leases_for_session(&proxy_session_id).len(), 2);
    let after_removal = store.availability_evidence_generation(&proxy_session_id);
    assert!(after_removal > before_removal);
    assert!(store.remove_access_leases_for_session(&proxy_session_id).is_empty());
    assert_eq!(store.availability_evidence_generation(&proxy_session_id), after_removal);
}

#[test]
fn published_uri_rejects_a_new_attempt_before_the_first_media_request() -> std::io::Result<()> {
    let session = ProxySessionId("proxy".into());
    let lease_id = HlsAccessLeaseId("revision-lease".into());
    let mut leases = HlsAccessLeaseStore::default();
    leases.prepare_access_lease(lease(lease_id.clone(), &session.0, 1000));
    let revisions = crate::SegmentRevisionStore::default();
    let published = revisions.create(session.clone(), 40, crate::SegmentRevisionKind::Raw)?;
    let tail = revisions.create(session.clone(), 41, crate::SegmentRevisionKind::Raw)?;
    for revision in [&published, &tail] {
        revision.revision().prefix_available.store(8, std::sync::atomic::Ordering::Release);
    }
    let mut first = manifest_snapshot(10);
    first.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
        mode: shared::model::HlsStartupMode::Progressive,
        retained_start_seq: 40,
        revisions: [(40, published.clone()), (41, tail.clone())].into_iter().collect(),
    }));
    let guard = leases
        .prepare_manifest_publication(&lease_id, &session, 2000)
        .ok_or_else(|| std::io::Error::other("publication guard"))?;
    assert!(leases.commit_manifest_publication(&lease_id, &session, guard, first, 2000).is_committed());
    published.revision().fail();
    let next = revisions.create(session.clone(), 40, crate::SegmentRevisionKind::Raw)?;
    next.revision().prefix_available.store(8, std::sync::atomic::Ordering::Release);
    let mut retry = manifest_snapshot(11);
    retry.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
        mode: shared::model::HlsStartupMode::Progressive,
        retained_start_seq: 40,
        revisions: [(40, next), (41, tail)].into_iter().collect(),
    }));
    let guard = leases
        .prepare_manifest_publication(&lease_id, &session, 2100)
        .ok_or_else(|| std::io::Error::other("publication guard"))?;
    assert_eq!(
        leases.commit_manifest_publication(&lease_id, &session, guard, retry, 2100),
        HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::RevisionConflict)
    );
    let bound = leases.published_segment_revision(&lease_id, 40).ok_or_else(|| std::io::Error::other("binding"))?;
    assert_eq!(bound, published);
    assert!(matches!(*bound.revision().subscribe().borrow(), crate::SegmentRevisionState::Failed));
    Ok(())
}
