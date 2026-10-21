use super::*;

#[test]
fn eof_without_transfer_evidence_does_not_start_the_clock_or_prove_exhaustion() {
    let mut cursor = HlsLeasePlaybackCursor::default();
    let token = cursor.record_request_started(0, 0);
    assert_eq!(cursor.record_request_completed(token, 10_000), HlsPlaybackCompletionOutcome::Advanced);
    let snapshot = reserve(&manifest(), &cursor, &ready_timeline(), 100_000);
    assert_eq!(cursor.first_segment_started_at_ms, None);
    assert!(!snapshot.playback_position_known);
    assert!(!snapshot.is_exhausted());
}

#[test]
fn concurrent_requests_anchor_to_the_first_actual_transfer_and_keep_its_media_start() {
    let session = variable_duration_session();
    let mut cursor = HlsLeasePlaybackCursor::default();
    let first = cursor.record_request_started(0, 0);
    let first = cursor.bind_request_timing(first, &session.segments[&0]);
    let successor = cursor.record_request_started(1, 100);
    let successor = cursor.bind_request_timing(successor, &session.segments[&1]);
    cursor.record_transfer_started(successor, 2_000);
    cursor.record_transfer_started(first, 2_500);
    assert_eq!(cursor.first_segment_started_at_ms, Some(2_000));
    assert_eq!(reserve(&manifest(), &cursor, &ready_timeline(), 3_000).conservative_playback_position_ms, 5_000);
}

#[test]
fn first_body_chunk_starts_exactly_one_x_before_eof_and_is_not_renewed() {
    let session = variable_duration_session();
    let mut cursor = HlsLeasePlaybackCursor::default();
    let request = cursor.record_request_started(0, 0);
    let request = cursor.bind_request_timing(request, &session.segments[&0]);
    let unknown = reserve(&manifest(), &cursor, &ready_timeline(), 2_000);
    assert!(!unknown.playback_position_known);
    assert!(!unknown.is_exhausted());
    assert!(cursor.record_transfer_started(request, 3_000));
    let playing = reserve(&manifest(), &cursor, &ready_timeline(), 5_000);
    assert_eq!(playing.conservative_playback_position_ms, 2_000);
    assert_eq!(cursor.highest_contiguous_completed_proxy_seq, None);
    assert_eq!(cursor.first_segment_completed_at_ms, None);
    let retry = cursor.record_request_started(0, 6_000);
    cursor.record_transfer_started(retry, 6_500);
    let next = cursor.record_request_started(1, 7_000);
    cursor.record_transfer_started(next, 7_500);
    assert_eq!(cursor.record_request_completed(request, 10_000), HlsPlaybackCompletionOutcome::Advanced);
    assert_eq!(cursor.first_segment_started_at_ms, Some(3_000));
    assert_eq!(reserve(&manifest(), &cursor, &ready_timeline(), 11_000).conservative_playback_position_ms, 8_000);
}

#[test]
fn one_hour_of_steady_supply_has_no_session_age_drift_then_exhausts_at_one_x() {
    let session = variable_duration_session();
    let mut cursor = HlsLeasePlaybackCursor::default();
    let first = cursor.record_request_started(0, 0);
    let first = cursor.bind_request_timing(first, &session.segments[&0]);
    cursor.record_transfer_started(first, 3_000);
    let mut current = manifest();
    let mut last_reserve = 0;
    for tick in 0..=360 {
        let media_ms = tick * 10_000;
        let now_ms = 3_000 + media_ms;
        let timeline = HlsReadyTimelineSnapshot {
            units: (tick..tick + 6).map(|seq| ready_unit(seq, seq * 10_000, 10_000)).collect(),
        };
        current.first_proxy_seq = tick;
        current.last_proxy_seq = tick + 5;
        current.playlist_duration_ms = 60_000;
        current.last_visible_media_end_ms = media_ms + 60_000;
        // Completed history allows GC to remove previous metadata, while the
        // original EXTINF clock and body-start anchor remain unchanged.
        cursor.highest_contiguous_completed_proxy_seq = tick.checked_sub(1);
        cursor.highest_contiguous_completed_media_end_ms = Some(media_ms);
        let snapshot = reserve(&current, &cursor, &timeline, now_ms);
        assert_eq!(snapshot.conservative_playback_position_ms, media_ms);
        assert_eq!(snapshot.guaranteed_reserve_ms, 60_000, "tick {tick}");
        last_reserve = snapshot.guaranteed_reserve_ms;
    }
    assert_eq!(last_reserve, 60_000);
    let timeline =
        HlsReadyTimelineSnapshot { units: (360..366).map(|seq| ready_unit(seq, seq * 10_000, 10_000)).collect() };
    for elapsed_ms in [1, 10_000, 59_999, 60_000, 70_000] {
        let snapshot = reserve(&current, &cursor, &timeline, 3_603_000 + elapsed_ms);
        assert_eq!(snapshot.guaranteed_reserve_ms, 60_000_u64.saturating_sub(elapsed_ms));
        assert_eq!(snapshot.is_exhausted(), elapsed_ms >= 60_000);
    }
}

#[test]
fn fourteen_minutes_of_supply_then_a_manifest_failure_and_retry_keep_positive_reserve_live() {
    let session = variable_duration_session();
    let mut cursor = HlsLeasePlaybackCursor::default();
    let first = cursor.record_request_started(0, 0);
    let first = cursor.bind_request_timing(first, &session.segments[&0]);
    cursor.record_transfer_started(first, 3_000);
    let mut current = manifest();
    for tick in 0..=84 {
        let media_ms = tick * 10_000;
        let timeline = HlsReadyTimelineSnapshot {
            units: (tick..tick + 3).map(|seq| ready_unit(seq, seq * 10_000, 10_000)).collect(),
        };
        current.first_proxy_seq = tick;
        current.last_proxy_seq = tick + 2;
        current.playlist_duration_ms = 30_000;
        current.last_visible_media_end_ms = media_ms + 30_000;
        cursor.highest_contiguous_completed_proxy_seq = tick.checked_sub(1);
        cursor.highest_contiguous_completed_media_end_ms = Some(media_ms);
        assert_eq!(reserve(&current, &cursor, &timeline, 3_000 + media_ms).guaranteed_reserve_ms, 30_000);
    }
    let unchanged =
        HlsReadyTimelineSnapshot { units: (84..87).map(|seq| ready_unit(seq, seq * 10_000, 10_000)).collect() };
    // One retryable manifest failure degrades the origin while the last READY
    // horizon stays unchanged. The former 5% drift would already consume 42s.
    for elapsed_ms in [1_000, 2_000] {
        let failed = reserve(&current, &cursor, &unchanged, 843_000 + elapsed_ms);
        assert_eq!(failed.guaranteed_reserve_ms, 30_000 - elapsed_ms);
        assert!(!failed.is_exhausted());
        assert!(!failed.cutover_required);
    }
    // The successful retry supplies one successor. Recovery neither resets
    // the first-body anchor nor creates a session-age reserve penalty.
    let recovered =
        HlsReadyTimelineSnapshot { units: (84..88).map(|seq| ready_unit(seq, seq * 10_000, 10_000)).collect() };
    let healthy = evaluate_lease_reserve(HlsLeaseReserveInput {
        manifest: &current,
        cursor: &cursor,
        ready_timeline: &recovered,
        now_ms: 846_000,
        recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(4_000),
        origin_path_degraded: false,
        recovery_committed: true,
    });
    assert_eq!(healthy.conservative_playback_position_ms, 843_000);
    assert_eq!(healthy.guaranteed_reserve_ms, 37_000);
    assert!(!healthy.cutover_required);
    assert_eq!(cursor.first_segment_started_at_ms, Some(3_000));
}

#[test]
fn seek_discards_the_start_anchor_and_rejects_old_first_chunks() {
    let session = variable_duration_session();
    let mut cursor = HlsLeasePlaybackCursor::default();
    let old = cursor.record_request_started(0, 0);
    let old = cursor.bind_request_timing(old, &session.segments[&0]);
    cursor.record_transfer_started(old, 1_000);
    let seek = cursor.record_request_started(4, 2_000);
    let seek = cursor.bind_request_timing(seek, &session.segments[&4]);
    assert_eq!(cursor.first_segment_started_at_ms, None);
    assert!(!cursor.record_transfer_started(old, 2_500));
    assert!(!reserve(&manifest(), &cursor, &ready_timeline(), 2_500).playback_position_known);
    cursor.record_transfer_started(seek, 3_000);
    assert_eq!(reserve(&manifest(), &cursor, &ready_timeline(), 5_000).conservative_playback_position_ms, 20_000);
    assert_eq!(cursor.highest_contiguous_completed_proxy_seq, None);
}
