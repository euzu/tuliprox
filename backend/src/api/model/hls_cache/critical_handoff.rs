use super::{
    evaluate_lease_reserve,
    manifest_acceptance::{
        HlsManifestAcceptanceGeneration, HlsTerminalAlternativeCompatibility,
    },
    manifest_fetch::{HlsManifestCommitError, HlsManifestRejectLogReason},
    recovery_timing::HlsRecoveryTriggerBudgetMs,
    terminal_tail::{
        evaluate_terminal_tail_compatibility, HlsTerminalAssetIdentity, HlsTerminalBaseTrackIdentity,
        HlsTerminalMediaAsset, HlsTerminalTailBoundaryEvidence, HlsTerminalTailCompatibility,
        HlsTerminalTailCompatibilityInput,
    },
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseStore, HlsCriticalHandoffStateAccess, HlsLeaseReserveInput,
    HlsMediaLeaseIdentity, HlsSession, HlsTsTrackSignature, SegmentCacheStatus, HLS_PLAYBACK_RATE_GUARD_MILLI,
};
use crate::model::{AppConfig, CustomStreamResponse};
use log::{debug, warn};
use std::{future::Future, sync::Arc};

pub(super) const HLS_CRITICAL_HANDOFF_COMMIT_RETRIES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HlsCriticalHandoffSnapshot {
    pub(super) lease_id: HlsAccessLeaseId,
    pub(super) media_identity: HlsMediaLeaseIdentity,
    pub(super) manifest_snapshot_generation: u64,
    pub(super) cursor_generation: u64,
    pub(super) base: HlsTerminalBaseTrackIdentity,
    pub(super) session_origin_epoch: u64,
    pub(super) media_readiness_generation: u64,
    pub(super) origin_progress_generation: u64,
    pub(super) acceptance_generation: HlsManifestAcceptanceGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsCriticalHandoffLockContentionDecision {
    Retry,
    Exhausted,
}

const fn critical_handoff_lock_contention_decision(
    attempt: usize,
) -> HlsCriticalHandoffLockContentionDecision {
    if attempt.saturating_add(1) < HLS_CRITICAL_HANDOFF_COMMIT_RETRIES {
        HlsCriticalHandoffLockContentionDecision::Retry
    } else {
        HlsCriticalHandoffLockContentionDecision::Exhausted
    }
}

pub(super) async fn retry_critical_handoff_state_access<T, Access, AccessFuture>(
    acceptance_generation: HlsManifestAcceptanceGeneration,
    mut access_state: Access,
) -> Result<T, HlsManifestCommitError>
where
    Access: FnMut() -> AccessFuture,
    AccessFuture: Future<Output = HlsCriticalHandoffStateAccess<Result<T, HlsManifestCommitError>>>,
{
    for attempt in 0..HLS_CRITICAL_HANDOFF_COMMIT_RETRIES {
        match access_state().await {
            HlsCriticalHandoffStateAccess::Acquired(result) => return result,
            HlsCriticalHandoffStateAccess::LockBusy => {
                let attempts_completed = attempt.saturating_add(1);
                match critical_handoff_lock_contention_decision(attempt) {
                    HlsCriticalHandoffLockContentionDecision::Retry => {
                        debug!(
                            "HLS critical manifest handoff commit deferred: acceptance_generation={} reason=critical-handoff-lock-busy attempt={attempts_completed} max_attempts={HLS_CRITICAL_HANDOFF_COMMIT_RETRIES}",
                            acceptance_generation.0
                        );
                        tokio::task::yield_now().await;
                    }
                    HlsCriticalHandoffLockContentionDecision::Exhausted => {
                        let reason = HlsManifestRejectLogReason::CriticalHandoffLockContentionExhausted;
                        warn!(
                            "HLS critical manifest handoff rejected: acceptance_generation={} reason={} attempts={attempts_completed}",
                            acceptance_generation.0,
                            reason.status_label()
                        );
                        return Err(staging_error(reason));
                    }
                }
            }
        }
    }
    Err(staging_error(
        HlsManifestRejectLogReason::CriticalHandoffLockContentionExhausted,
    ))
}

struct HlsCriticalHandoffCandidate {
    lease: HlsAccessLease,
    snapshot: HlsCriticalHandoffSnapshot,
    safe_cutover_deadline_ms: u64,
    media_exhaustion_deadline_ms: u64,
}

fn guarded_playback_wall_time_ms(media_time_ms: u64) -> u64 {
    media_time_ms.saturating_mul(1_000) / u64::from(HLS_PLAYBACK_RATE_GUARD_MILLI.max(1))
}

pub(super) fn select_critical_handoff_lease(
    session: &HlsSession,
    leases: &[HlsAccessLease],
    acceptance_generation: HlsManifestAcceptanceGeneration,
    generation_is_current: bool,
    now_ms: u64,
) -> Option<(HlsAccessLease, HlsCriticalHandoffSnapshot)> {
    if !generation_is_current {
        return None;
    }
    leases
        .iter()
        .filter_map(|lease| {
            if lease.proxy_session_id != session.proxy_session_id {
                return None;
            }
            let manifest = lease.last_manifest_snapshot.as_ref()?;
            let media_identity = lease.media_identity().filter(|identity| identity.is_live())?;
            let ready_timeline = session.ready_timeline_snapshot(
                lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
                now_ms,
            );
            let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
                manifest,
                cursor: &lease.playback_cursor,
                ready_timeline: &ready_timeline,
                now_ms,
                playback_rate_guard_milli: HLS_PLAYBACK_RATE_GUARD_MILLI,
                recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(0),
                origin_path_degraded: true,
                recovery_committed: false,
            });
            if !reserve.cutover_required {
                return None;
            }
            let base_entry = session.segments.get(&manifest.last_proxy_seq).filter(|entry| {
                entry.proxy_file_ext.eq_ignore_ascii_case("ts")
                    && matches!(&entry.status, SegmentCacheStatus::Ready { .. })
            })?;
            let media_exhaustion_deadline_ms =
                now_ms.saturating_add(guarded_playback_wall_time_ms(reserve.guaranteed_reserve_ms));
            let safe_cutover_deadline_ms = media_exhaustion_deadline_ms
                .saturating_sub(guarded_playback_wall_time_ms(reserve.transition_margin.as_millis()));
            Some(HlsCriticalHandoffCandidate {
                lease: lease.clone(),
                snapshot: HlsCriticalHandoffSnapshot {
                    lease_id: lease.lease_id.clone(),
                    media_identity,
                    manifest_snapshot_generation: manifest.snapshot_generation,
                    cursor_generation: lease.playback_cursor.cursor_generation,
                    base: HlsTerminalBaseTrackIdentity {
                        proxy_seq: manifest.last_proxy_seq,
                        origin_epoch: base_entry.origin_key.origin_epoch,
                        cache_key: base_entry.cache_key.clone(),
                    },
                    session_origin_epoch: session.origin_epoch,
                    media_readiness_generation: session.activity.media_readiness_generation,
                    origin_progress_generation: session.origin_control.progress_generation,
                    acceptance_generation,
                },
                safe_cutover_deadline_ms,
                media_exhaustion_deadline_ms,
            })
        })
        .min_by(|left, right| {
            (
                left.safe_cutover_deadline_ms,
                left.media_exhaustion_deadline_ms,
                left.lease.issued_at_ms,
                left.lease.lease_id.0.as_str(),
            )
                .cmp(&(
                    right.safe_cutover_deadline_ms,
                    right.media_exhaustion_deadline_ms,
                    right.lease.issued_at_ms,
                    right.lease.lease_id.0.as_str(),
                ))
        })
        .map(|candidate| (candidate.lease, candidate.snapshot))
}

pub(super) fn critical_handoff_snapshot_is_current(
    leases: &mut HlsAccessLeaseStore,
    session: &HlsSession,
    acceptance_generation: HlsManifestAcceptanceGeneration,
    generation_is_current: bool,
    expected: &HlsCriticalHandoffSnapshot,
    now_ms: u64,
) -> bool {
    let active = leases.active_live_playback_snapshots_for_session(&session.proxy_session_id, now_ms);
    select_critical_handoff_lease(
        session,
        &active,
        acceptance_generation,
        generation_is_current,
        now_ms,
    )
    .is_some_and(|(_, current)| current == *expected)
}

pub(super) fn terminal_alternative_compatibility_for_critical_lease(
    terminal_asset: Option<&HlsTerminalMediaAsset>,
    lease: &HlsAccessLease,
    base_tracks: &HlsTsTrackSignature,
) -> HlsTerminalAlternativeCompatibility {
    let Some(manifest) = lease.last_manifest_snapshot.as_ref() else {
        return HlsTerminalAlternativeCompatibility::LiveHandoffSafer;
    };
    let Some(asset) = terminal_asset else {
        return HlsTerminalAlternativeCompatibility::LiveHandoffSafer;
    };
    let compatibility = evaluate_terminal_tail_compatibility(HlsTerminalTailCompatibilityInput {
        manifest,
        base_track_signature: Some(base_tracks),
        boundary_evidence: HlsTerminalTailBoundaryEvidence::StructuralOnly,
        expected_asset: HlsTerminalAssetIdentity::from_asset(asset),
        asset,
    });
    if compatibility == HlsTerminalTailCompatibility::Compatible {
        HlsTerminalAlternativeCompatibility::TerminalTailPreferred
    } else {
        HlsTerminalAlternativeCompatibility::LiveHandoffSafer
    }
}

pub(super) fn critical_handoff_terminal_response_is_current(
    app_config: &AppConfig,
    expected: Option<&Arc<CustomStreamResponse>>,
) -> bool {
    let current = app_config.custom_stream_response.load_full();
    match (current.as_ref(), expected) {
        (None, None) => true,
        (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
        (None, Some(_)) | (Some(_), None) => false,
    }
}

const fn staging_error(reason: HlsManifestRejectLogReason) -> HlsManifestCommitError {
    HlsManifestCommitError::TimelineRejected { reason }
}
