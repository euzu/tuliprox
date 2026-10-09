//! Tests for availability evaluation.
//!
//! These drive the reevaluation worker and the terminal decision paths end to
//! end, so they stay with the orchestration in `super` rather than with the
//! individual steps in `recovery_pressure`, `reevaluation`, `terminal_cutover`
//! and `terminal_pending_owner`.

use super::{
    super::{
        lease::HlsAccessLeaseTiming,
        media_reserve::{
            HlsLeaseManifestSegment, HlsLeasePlaybackCursor, HlsLeaseReserveAvailabilityBasis,
            HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsReadyMediaState, HlsReadyTimelineUnit,
        },
        post_refresh_availability::{commit_post_refresh_terminal_fallback, HlsPostRefreshFallbackOutcome},
        prepared_terminal_bundle::{
            prepared_terminal_bundle_completion_channel_for_test, HlsPreparedTerminalBundleCompletionTicket,
            HlsPreparedTerminalSegment,
        },
        recovery_timing::{HlsRecoveryBurstWorkload, HlsRecoveryMapWorkload, HlsRecoverySegmentWorkload},
        runtime_custom_tail::{
            commit_hls_runtime_custom_tail, snapshot_hls_runtime_custom_tail_asset, HlsRuntimeCustomTailOutcome,
            HlsRuntimeCustomTailRequest,
        },
        session_store::HlsSessionIncarnation,
        terminal_pending::{HlsTerminalPendingCoordinator, HlsTerminalPendingOwnerKey, HlsTerminalPendingRegistration},
        terminal_tail::{
            terminal_tail_manifest_body, HlsLeasePlaybackMode, HlsMapSignature, HlsMediaContainer,
            HlsTerminalAssetIdentity, HlsTerminalSegmentPath, HlsTerminalTailPlan,
        },
        CacheAccessState, HlsAccessLease, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsSession, HlsSessionKey,
        OriginSegmentKey, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, SegmentFetchPriority,
    },
    recovery_pressure::{
        acceptance_timing_seed_for_pressure, aggregate_session_recovery_pressure,
        evaluate_and_commit_session_recovery_pressure, recovery_trigger_source, HlsLeaseRecoveryEvidence,
        HlsRecoveryBoundarySlackMs, HlsRecoveryPressurePolicy,
    },
    reevaluation::{
        availability_refresh_trigger_decision, register_hls_availability_reevaluation_with_mode,
        HlsAvailabilityAttemptSchedule, HlsAvailabilityRefreshTriggerDecision,
    },
    terminal_cutover::HlsTerminalCommitContext,
    terminal_pending_owner::{
        await_terminal_pending_decision, classify_autonomous_terminal_resolution, run_terminal_pending_owner,
        terminal_asset_revision_guard, terminal_pending_fallback_commit_at_ms, terminal_resolution_for_commit_outcome,
        terminal_resolution_for_pending_registration, HlsAutonomousTerminalObservation, HlsTerminalPendingDecision,
    },
    *,
};
use crate::{
    api::{HlsRuntimeCustomTailReason, OriginRefreshRequest},
    evaluate_lease_reserve,
    lease::HlsTerminalTailPreparation,
    manager::HlsTerminalTailPreparationRequest,
    media_reserve::{HlsLeaseReserveSnapshot, HlsReadyTimelineSnapshot},
    origin_progress::{
        evaluate_origin_progress, HlsOriginPathCondition, HlsOriginProgressPhase, HlsOriginProgressSnapshot,
    },
    post_refresh_availability::{
        evaluate_active_terminal_leases_for_reevaluation, evaluate_owner_failure_fallback, live_reserve_deadline,
        HlsPostRefreshTerminalEvaluation,
    },
    prepared_terminal_bundle::{
        build_prepared_terminal_bundle, HlsPreparedTerminalBundle, HlsPreparedTerminalBundleCompletion,
        HlsPreparedTerminalBundleKey,
    },
    prepared_terminal_bundle_key,
    recovery_timing::{
        HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsLeaseCutoverTiming, HlsRecoveryTriggerBudgetMs,
        HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState,
        HlsTransitionMarginMs,
    },
    refresh::{HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction},
    snapshot_terminal_media_asset,
    terminal_commit::{HlsTerminalAssetRevisionGuard, HlsTerminalCommitOutcome},
    HlsAccessLeaseId, HlsAccessLeaseStore, HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationRegistration,
    HlsLeaseReserveInput, HlsObservedRecoveryLatency, HlsPreparedTerminalBundleState, HlsRecoveryTriggerSource,
    HlsRuntimeCustomTailAssetIdentity, HlsTerminalTailCompatibility, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
};
use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;
use tuliprox_core::model::{Config, CustomStreamResponse};
use tuliprox_mpegts::transport_stream_buffer::{HlsTsSpliceAnchor, TransportStreamBuffer};
use tuliprox_parser::hls::origin_manifest::{
    parse_origin_media_manifest, OriginManifestParseOutcome, ParsedOriginManifest,
};

const TERMINAL_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

const LOW_PRIORITY_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/low_priority_preempted.ts"));

const PROVIDER_EXHAUSTED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/provider_connections_exhausted.ts"));

const USER_EXHAUSTED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_connections_exhausted.ts"));

const ACCOUNT_EXPIRED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_account_expired.ts"));

const SESSION_EXPIRED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/hls_session_or_lease_expired.ts"));

fn runtime_custom_responses() -> Arc<CustomStreamResponse> {
    runtime_custom_responses_with_low_priority(LOW_PRIORITY_ASSET_BYTES)
}

fn runtime_custom_responses_with_low_priority(low_priority_bytes: &[u8]) -> Arc<CustomStreamResponse> {
    Arc::new(CustomStreamResponse {
        channel_unavailable: Some(TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec())),
        user_connections_exhausted: Some(TransportStreamBuffer::new(USER_EXHAUSTED_ASSET_BYTES.to_vec())),
        provider_connections_exhausted: Some(TransportStreamBuffer::new(PROVIDER_EXHAUSTED_ASSET_BYTES.to_vec())),
        low_priority_preempted: Some(TransportStreamBuffer::new(low_priority_bytes.to_vec())),
        user_account_expired: Some(TransportStreamBuffer::new(ACCOUNT_EXPIRED_ASSET_BYTES.to_vec())),
        panel_api_provisioning: None,
        hls_session_or_lease_expired: Some(TransportStreamBuffer::new(SESSION_EXPIRED_ASSET_BYTES.to_vec())),
        panel_api_provisioning_hls_segments: Vec::new(),
    })
}

fn publication_late_decision(reserve_ms: u64) -> HlsOriginProgressDecision {
    evaluate_origin_progress(HlsOriginProgressSnapshot {
        phase: HlsOriginProgressPhase::Fresh,
        condition: HlsOriginPathCondition::ProgressExpected,
        target_duration_ms: 10_000,
        last_media_progress_at_ms: Some(0),
        session_recovery_required: reserve_ms <= 14_000,
        session_cutover_evaluation_required: reserve_ms <= 10_000,
        recovery_committed: false,
        now_ms: 15_000,
    })
}

fn lease_timing_seed() -> HlsAcceptanceEpisodeTimingSeed {
    HlsAcceptanceEpisodeTimingSeed {
        target_duration_ms: 10_000,
        transition_margin: HlsTransitionMarginMs::from_millis(10_000),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
    }
}

fn pending_decision_bundle_key() -> HlsPreparedTerminalBundleKey {
    HlsPreparedTerminalBundleKey {
        asset: HlsTerminalAssetIdentity { revision: 7, fingerprint: [7; 32] },
        target_duration_ms: 1_000,
        segment_count: 2,
    }
}

fn pending_decision_owner_key(bundle_key: HlsPreparedTerminalBundleKey) -> HlsTerminalPendingOwnerKey {
    HlsTerminalPendingOwnerKey {
        session_incarnation: HlsSessionIncarnation::for_test(1),
        proxy_session_id: ProxySessionId("pending-session".to_string()),
        lease_id: HlsAccessLeaseId("pending-lease".to_string()),
        lease_issued_at_ms: 10,
        expected_admission_generation: 20,
        manifest_snapshot_generation: 30,
        cursor_generation: 40,
        decision_generation: 50,
        reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
        bundle_key,
        latest_safe_commit_at_ms: 10_000,
    }
}

fn pending_decision_ready_bundle(bundle_key: HlsPreparedTerminalBundleKey) -> Arc<HlsPreparedTerminalBundle> {
    let segments = (0..bundle_key.segment_count)
        .map(|index| HlsPreparedTerminalSegment {
            index,
            timestamp_offset_ticks_90khz: u64::from(index).saturating_mul(45_000),
            bytes: Bytes::from_static(b"terminal"),
        })
        .collect::<Vec<_>>();
    Arc::new(HlsPreparedTerminalBundle {
        key: bundle_key,
        source_asset_duration_ms: 500,
        source_asset_duration_ticks_90khz: 45_000,
        segments: segments.into(),
    })
}

struct HlsTerminalPendingCommitFixture {
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    preparation: HlsTerminalTailPreparation,
    asset: Arc<super::super::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    now_ms: u64,
}

struct PostRefreshTerminalFixture {
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    now_ms: u64,
}

mod admission;

mod policy;

mod publication;

mod recovery;

mod storage;

mod terminal;

mod transport;

mod terminal_fixtures;
use terminal_fixtures::{
    assert_post_refresh_registration_failure_leaves_no_unowned_live_lease,
    assert_terminal_pending_registration_failure_commits_unavailable, terminal_base_without_timestamps,
    terminal_pending_commit_fixture, terminal_pending_commit_fixture_with_base, terminal_pending_commit_reserve,
};
mod pressure_fixtures;
use pressure_fixtures::{
    atomic_pressure_policy, atomic_pressure_session, evaluated_pressure, install_atomic_pressure_lease,
    pressure_manifest, pressure_manifest_at,
};
mod owner_fixtures;
use owner_fixtures::{
    assert_availability_owner_registered, assert_post_refresh_owner_checks_refresh_gate_once,
    post_refresh_owner_request, post_refresh_terminal_fixture, post_refresh_terminal_fixture_with_bundle_state,
    post_refresh_terminal_fixture_with_progress, register_real_post_refresh_owner,
    wait_for_availability_owner_completion,
};
mod runtime_fixtures;
use runtime_fixtures::{
    advance_post_refresh_fixture_playback, assert_active_policy_reason_commits,
    assert_multi_lease_fallback_handles_pending_and_unavailable, commit_runtime_custom_reason,
    configured_runtime_custom_buffer, payload_continuity_bounds, prepare_runtime_custom_bundle, segment_bytes,
    wait_for_runtime_custom_plan, wait_for_terminal_pending_owners, with_internal_payload_continuity_jump,
};
