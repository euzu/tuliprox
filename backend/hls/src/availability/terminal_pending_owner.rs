//! Waiting for a terminal decision that another task owns.
//!
//! Only one task may commit a session's terminal state. The rest register as
//! pending and wait on that owner, with a fallback commit time so a lost owner
//! cannot leave them waiting forever. Turning the owner's outcome - or its
//! failure - into a resolution for every waiter happens here.

use super::{
    terminal_cutover::{
        commit_ready_terminal_bundle, terminal_bundle_failure_compatibility, terminal_bundle_incompatibility,
        HlsTerminalCommitContext,
    },
    HlsTerminalFailedClosedReason, HlsTerminalResolution, HLS_TERMINAL_ASSET_REVALIDATION_ATTEMPTS,
    HLS_TERMINAL_PENDING_RETRY_AFTER_MS,
};
use crate::{
    hls_ctx::HlsCtx,
    lease::HlsTerminalTailPreparation,
    manager::{HlsTerminalCommitPayload, HlsTerminalCommitRequest},
    prepared_terminal_bundle::{
        HlsPreparedTerminalBundle, HlsPreparedTerminalBundleCompletion, HlsPreparedTerminalBundleKey,
        HlsPreparedTerminalBundleObservation, HlsPreparedTerminalBundleState,
    },
    recovery_timing::HlsTerminalCommitAcquisitionBudgetMs,
    runtime_custom_tail::{
        current_hls_runtime_custom_tail_identity, HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason,
    },
    session_store::HlsSessionHandle,
    terminal_commit::{HlsTerminalAssetRevisionGuard, HlsTerminalCommitOutcome},
    terminal_pending::{HlsTerminalPendingOwnerKey, HlsTerminalPendingOwnership, HlsTerminalPendingRegistration},
    terminal_tail::{terminal_media_asset_identity, HlsTerminalTailCompatibility},
    HlsAccessLeaseId, ProxySessionId,
};
use log::warn;
use std::{future::Future, sync::Arc, time::Duration};

pub(super) async fn register_terminal_pending_owner(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<crate::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
) -> HlsTerminalResolution {
    let ticket = match context.ctx.hls_proxy.observe_prepared_terminal_bundle(bundle_key) {
        HlsPreparedTerminalBundleObservation::Flight(ticket) => ticket,
        HlsPreparedTerminalBundleObservation::Settled(state) => {
            return terminal_resolution_after_settled_bundle_observation(
                context,
                asset,
                expected_asset,
                bundle_key,
                Some(state),
            )
            .await;
        }
        HlsPreparedTerminalBundleObservation::Missing => {
            return terminal_resolution_after_settled_bundle_observation(
                context,
                asset,
                expected_asset,
                bundle_key,
                None,
            )
            .await;
        }
    };
    let Some(owner_key) = terminal_pending_owner_key(context, expected_asset, bundle_key) else {
        return HlsTerminalResolution::Reevaluate;
    };
    let latest_safe_commit_at_ms =
        context.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    let coordinator = context.ctx.hls_proxy.terminal_pending();
    let task_ctx = context.ctx.clone();
    let task_session = Arc::clone(context.session);
    let task_proxy_session_id = context.proxy_session_id.clone();
    let task_lease_id = context.lease_id.clone();
    let task_preparation = context.preparation.clone();
    let asset_guard = terminal_asset_revision_guard(context.ctx, expected_asset.reason, Some(expected_asset));
    let registration = coordinator.register(owner_key, &asset_guard, move |ownership| {
        run_terminal_pending_owner(
            task_ctx,
            task_session,
            task_proxy_session_id,
            task_lease_id,
            task_preparation,
            asset,
            expected_asset,
            bundle_key,
            ticket,
            ownership,
        )
    });
    terminal_resolution_for_pending_registration(context, expected_asset, latest_safe_commit_at_ms, registration)
}

fn terminal_pending_owner_key(
    context: HlsTerminalCommitContext<'_>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
) -> Option<HlsTerminalPendingOwnerKey> {
    let session_incarnation = context.ctx.hls_proxy.sessions().session_incarnation(context.session)?;
    Some(HlsTerminalPendingOwnerKey {
        session_incarnation,
        proxy_session_id: context.proxy_session_id.clone(),
        lease_id: context.lease_id.clone(),
        lease_issued_at_ms: context.preparation.lease_issued_at_ms,
        expected_admission_generation: context.preparation.expected_admission_generation,
        manifest_snapshot_generation: context.preparation.manifest_snapshot_generation,
        cursor_generation: context.preparation.cursor_generation,
        decision_generation: context.preparation.decision_generation,
        reason: expected_asset.reason,
        bundle_key,
        latest_safe_commit_at_ms: context
            .preparation
            .cutover_timing
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch(),
    })
}

pub(super) fn register_ready_terminal_pending_owner(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<crate::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    bundle: Arc<HlsPreparedTerminalBundle>,
) -> HlsTerminalResolution {
    let Some(owner_key) = terminal_pending_owner_key(context, expected_asset, bundle_key) else {
        return HlsTerminalResolution::Reevaluate;
    };
    let latest_safe_commit_at_ms = owner_key.latest_safe_commit_at_ms;
    let coordinator = context.ctx.hls_proxy.terminal_pending();
    let task_ctx = context.ctx.clone();
    let task_session = Arc::clone(context.session);
    let task_proxy_session_id = context.proxy_session_id.clone();
    let task_lease_id = context.lease_id.clone();
    let task_preparation = context.preparation.clone();
    let asset_guard = terminal_asset_revision_guard(context.ctx, expected_asset.reason, Some(expected_asset));
    let registration = coordinator.register(owner_key, &asset_guard, move |ownership| async move {
        if !ownership.is_current() {
            return;
        }
        let now_ms = task_ctx.hls_proxy.terminal_commit_now_ms();
        let task_context = HlsTerminalCommitContext {
            ctx: &task_ctx,
            session: &task_session,
            proxy_session_id: &task_proxy_session_id,
            lease_id: &task_lease_id,
            preparation: &task_preparation,
            now_ms,
        };
        let outcome = commit_ready_terminal_bundle(task_context, asset, expected_asset, bundle_key, bundle).await;
        if ownership.is_current() {
            observe_autonomous_terminal_resolution(terminal_resolution_with_failed_closed_fallback(
                task_context,
                outcome,
            ));
        }
    });
    terminal_resolution_for_pending_registration(context, expected_asset, latest_safe_commit_at_ms, registration)
}

pub(super) fn terminal_resolution_for_pending_registration(
    context: HlsTerminalCommitContext<'_>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    latest_safe_commit_at_ms: u64,
    registration: HlsTerminalPendingRegistration,
) -> HlsTerminalResolution {
    match registration {
        HlsTerminalPendingRegistration::Scheduled | HlsTerminalPendingRegistration::AlreadyOwned => {
            HlsTerminalResolution::Pending {
                retry_after_ms: terminal_pending_retry_after_ms(context.now_ms, latest_safe_commit_at_ms),
            }
        }
        HlsTerminalPendingRegistration::Superseded => HlsTerminalResolution::Reevaluate,
        HlsTerminalPendingRegistration::CapacityExceeded | HlsTerminalPendingRegistration::RuntimeUnavailable => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::TerminalMediaNotReady,
            );
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
    }
}

async fn terminal_resolution_after_settled_bundle_observation(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<crate::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    state: Option<HlsPreparedTerminalBundleState>,
) -> HlsTerminalResolution {
    match state {
        Some(HlsPreparedTerminalBundleState::Ready { bundle }) if bundle.matches_key_and_shape(bundle_key) => {
            let outcome = commit_ready_terminal_bundle(context, asset, expected_asset, bundle_key, bundle).await;
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Ready { .. }) => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::AssetRevisionMismatch,
            );
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Failed { key, reason }) => {
            let compatibility = terminal_bundle_failure_compatibility(key, bundle_key, reason);
            let outcome = commit_terminal_unavailable(context, Some(expected_asset), compatibility);
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Incompatible { key, reason }) => {
            let compatibility = if key == bundle_key {
                terminal_bundle_incompatibility(reason)
            } else {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            };
            let outcome = commit_terminal_unavailable(context, Some(expected_asset), compatibility);
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Preparing { .. }) | None => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::TerminalMediaNotReady,
            );
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
    }
}

#[derive(Debug)]
pub(super) enum HlsTerminalPendingDecision {
    Ready(Arc<HlsPreparedTerminalBundle>),
    Unavailable(HlsTerminalTailCompatibility),
}

pub(super) async fn await_terminal_pending_decision<Fallback>(
    ticket: crate::prepared_terminal_bundle::HlsPreparedTerminalBundleCompletionTicket,
    ownership: &HlsTerminalPendingOwnership,
    bundle_key: HlsPreparedTerminalBundleKey,
    fallback: Fallback,
) -> Option<HlsTerminalPendingDecision>
where
    Fallback: Future<Output = ()>,
{
    tokio::pin!(fallback);
    let completion = tokio::select! {
        biased;
        () = ownership.cancelled() => return None,
        completion = ticket.wait() => Some(completion),
        () = &mut fallback => None,
    };
    if !ownership.is_current() {
        return None;
    }
    match completion {
        Some(HlsPreparedTerminalBundleCompletion::Ready { bundle }) if bundle.matches_key_and_shape(bundle_key) => {
            Some(HlsTerminalPendingDecision::Ready(bundle))
        }
        Some(HlsPreparedTerminalBundleCompletion::Ready { .. }) => {
            Some(HlsTerminalPendingDecision::Unavailable(HlsTerminalTailCompatibility::AssetRevisionMismatch))
        }
        Some(HlsPreparedTerminalBundleCompletion::Failed { key, reason }) => Some(
            HlsTerminalPendingDecision::Unavailable(terminal_bundle_failure_compatibility(key, bundle_key, reason)),
        ),
        Some(HlsPreparedTerminalBundleCompletion::Incompatible { key, reason }) => {
            let compatibility = if key == bundle_key {
                terminal_bundle_incompatibility(reason)
            } else {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            };
            Some(HlsTerminalPendingDecision::Unavailable(compatibility))
        }
        Some(HlsPreparedTerminalBundleCompletion::FlightReplaced { key, generation }) => {
            let compatibility = if key == bundle_key && generation > 0 {
                HlsTerminalTailCompatibility::TerminalMediaNotReady
            } else {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            };
            Some(HlsTerminalPendingDecision::Unavailable(compatibility))
        }
        None => Some(HlsTerminalPendingDecision::Unavailable(HlsTerminalTailCompatibility::TerminalMediaNotReady)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_terminal_pending_owner(
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    preparation: HlsTerminalTailPreparation,
    asset: Arc<crate::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    ticket: crate::prepared_terminal_bundle::HlsPreparedTerminalBundleCompletionTicket,
    ownership: HlsTerminalPendingOwnership,
) {
    let latest_safe_commit_at_ms = ownership.latest_safe_commit_at_ms();
    // Terminal-media preparation is started before this path and may use the
    // acquisition window. The final bounded handoff still reserves enough time
    // for an initially contended fail-closed CAS and one retry, both strictly
    // before the exclusive safe deadline.
    let fallback_commit_at_ms = terminal_pending_fallback_commit_at_ms(latest_safe_commit_at_ms);
    let fallback_wait_ms = fallback_commit_at_ms.saturating_sub(ctx.hls_proxy.terminal_commit_now_ms());
    let Some(decision) = await_terminal_pending_decision(
        ticket,
        &ownership,
        bundle_key,
        tokio::time::sleep(Duration::from_millis(fallback_wait_ms)),
    )
    .await
    else {
        return;
    };
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    let context = HlsTerminalCommitContext {
        ctx: &ctx,
        session: &session,
        proxy_session_id: &proxy_session_id,
        lease_id: &lease_id,
        preparation: &preparation,
        now_ms,
    };
    let outcome = match decision {
        HlsTerminalPendingDecision::Ready(bundle) => {
            commit_ready_terminal_bundle(context, asset, expected_asset, bundle_key, bundle).await
        }
        HlsTerminalPendingDecision::Unavailable(compatibility) => {
            commit_terminal_unavailable(context, Some(expected_asset), compatibility)
        }
    };
    observe_autonomous_terminal_resolution(terminal_resolution_with_failed_closed_fallback(context, outcome));
}

fn terminal_pending_retry_after_ms(now_ms: u64, latest_safe_commit_at_ms: u64) -> u64 {
    latest_safe_commit_at_ms.saturating_sub(now_ms).clamp(1, HLS_TERMINAL_PENDING_RETRY_AFTER_MS)
}

pub(super) fn terminal_pending_fallback_commit_at_ms(latest_safe_commit_at_ms: u64) -> u64 {
    latest_safe_commit_at_ms
        .saturating_sub(HlsTerminalCommitAcquisitionBudgetMs::fail_closed_handoff_from_retry_policy().as_millis())
}

pub(super) fn terminal_resolution_for_commit_outcome(
    outcome: HlsTerminalCommitOutcome,
    now_ms: u64,
) -> HlsTerminalResolution {
    match outcome {
        HlsTerminalCommitOutcome::Committed | HlsTerminalCommitOutcome::AlreadyCommitted => {
            HlsTerminalResolution::Committed
        }
        HlsTerminalCommitOutcome::SupersededGeneration
        | HlsTerminalCommitOutcome::LeaseNoLongerEligible
        | HlsTerminalCommitOutcome::RecoveryCommitted
        | HlsTerminalCommitOutcome::CutoverNoLongerRequired => HlsTerminalResolution::Reevaluate,
        HlsTerminalCommitOutcome::BundleNotReady => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::BundleNotReadyWithoutOwner }
        }
        HlsTerminalCommitOutcome::BundleIncompatible => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::BundleIncompatible }
        }
        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed }
        }
        HlsTerminalCommitOutcome::RetryCapacityExceeded => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::RetryCapacityExceeded }
        }
        HlsTerminalCommitOutcome::RetryAttemptsExhausted => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::RetryAttemptsExhausted }
        }
        HlsTerminalCommitOutcome::RetryWorkerUnavailable => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::RuntimeUnavailable }
        }
        HlsTerminalCommitOutcome::LockBusy { retry_before_ms } => {
            HlsTerminalResolution::Pending { retry_after_ms: retry_before_ms.saturating_sub(now_ms).max(1) }
        }
    }
}

pub(super) fn terminal_resolution_with_failed_closed_fallback(
    context: HlsTerminalCommitContext<'_>,
    outcome: HlsTerminalCommitOutcome,
) -> HlsTerminalResolution {
    let resolution = terminal_resolution_for_commit_outcome(outcome, context.now_ms);
    if !matches!(resolution, HlsTerminalResolution::FailedClosed { .. }) {
        return resolution;
    }
    if context.preparation.trigger.is_runtime_policy() {
        return resolution;
    }
    commit_prepared_terminal_unavailable_after_owner_failure(
        context.ctx,
        context.session,
        context.proxy_session_id,
        context.lease_id,
        context.preparation,
        context.now_ms,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsAutonomousTerminalObservation {
    Committed,
    StateSuperseded,
    CommitRetry { retry_after_ms: u64 },
    FailedClosed { reason: HlsTerminalFailedClosedReason },
    NoCutoverRequired,
}

pub(super) fn classify_autonomous_terminal_resolution(
    resolution: HlsTerminalResolution,
) -> HlsAutonomousTerminalObservation {
    match resolution {
        HlsTerminalResolution::Committed => HlsAutonomousTerminalObservation::Committed,
        HlsTerminalResolution::Reevaluate => HlsAutonomousTerminalObservation::StateSuperseded,
        HlsTerminalResolution::Pending { retry_after_ms } => {
            HlsAutonomousTerminalObservation::CommitRetry { retry_after_ms }
        }
        HlsTerminalResolution::FailedClosed { reason } => HlsAutonomousTerminalObservation::FailedClosed { reason },
        HlsTerminalResolution::LiveAllowed => HlsAutonomousTerminalObservation::NoCutoverRequired,
    }
}

fn observe_autonomous_terminal_resolution(resolution: HlsTerminalResolution) {
    match classify_autonomous_terminal_resolution(resolution) {
        HlsAutonomousTerminalObservation::Committed => {
            log::debug!("HLS autonomous terminal owner completed: outcome=committed");
        }
        HlsAutonomousTerminalObservation::StateSuperseded => {
            log::debug!("HLS autonomous terminal owner stopped: outcome=state_superseded");
        }
        HlsAutonomousTerminalObservation::CommitRetry { retry_after_ms } => {
            log::debug!(
                "HLS autonomous terminal owner handed off: outcome=commit_retry retry_after_ms={retry_after_ms}"
            );
        }
        HlsAutonomousTerminalObservation::FailedClosed { reason } => {
            warn!("HLS autonomous terminal owner failed closed: reason={}", reason.as_label());
        }
        HlsAutonomousTerminalObservation::NoCutoverRequired => {
            log::debug!("HLS autonomous terminal owner stopped: outcome=no_cutover_required");
        }
    }
}

pub(super) fn commit_terminal_unavailable(
    context: HlsTerminalCommitContext<'_>,
    mut expected_asset: Option<HlsRuntimeCustomTailAssetIdentity>,
    mut reason: HlsTerminalTailCompatibility,
) -> HlsTerminalCommitOutcome {
    let manager = &context.ctx.hls_proxy;
    let commit_now_ms = manager.terminal_commit_now_ms();
    let mut remaining_revalidations = HLS_TERMINAL_ASSET_REVALIDATION_ATTEMPTS;
    loop {
        let outcome = manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session: context.session,
            lease_id: context.lease_id,
            proxy_session_id: context.proxy_session_id,
            preparation: context.preparation,
            now_ms: commit_now_ms.max(context.now_ms),
            payload: HlsTerminalCommitPayload::Unavailable(reason),
            asset_revision_guard: terminal_asset_revision_guard(
                context.ctx,
                context.preparation.trigger.reason(),
                expected_asset,
            ),
        });
        if outcome != HlsTerminalCommitOutcome::BundleIncompatible || remaining_revalidations == 0 {
            return outcome;
        }
        remaining_revalidations = remaining_revalidations.saturating_sub(1);
        expected_asset = configured_terminal_asset_identity(context.ctx, context.preparation.trigger.reason());
        reason = if expected_asset.is_some() {
            HlsTerminalTailCompatibility::AssetRevisionMismatch
        } else {
            HlsTerminalTailCompatibility::MissingAsset
        };
    }
}

pub(super) fn commit_prepared_terminal_unavailable_after_owner_failure(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    now_ms: u64,
) -> HlsTerminalResolution {
    let expected_asset = configured_terminal_asset_identity(ctx, HlsRuntimeCustomTailReason::ChannelUnavailable);
    let outcome = ctx.hls_proxy.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session,
        lease_id,
        proxy_session_id,
        preparation,
        now_ms: ctx.hls_proxy.terminal_commit_now_ms().max(now_ms),
        payload: HlsTerminalCommitPayload::UnavailableAfterOwnerFailure(
            HlsTerminalTailCompatibility::TerminalMediaNotReady,
        ),
        asset_revision_guard: terminal_asset_revision_guard(
            ctx,
            HlsRuntimeCustomTailReason::ChannelUnavailable,
            expected_asset,
        ),
    });
    terminal_resolution_for_commit_outcome(outcome, now_ms)
}

fn configured_terminal_asset_identity(
    ctx: &HlsCtx,
    reason: HlsRuntimeCustomTailReason,
) -> Option<HlsRuntimeCustomTailAssetIdentity> {
    current_hls_runtime_custom_tail_identity(ctx, reason)
}

pub(super) fn terminal_asset_revision_guard(
    ctx: &HlsCtx,
    reason: HlsRuntimeCustomTailReason,
    expected: Option<HlsRuntimeCustomTailAssetIdentity>,
) -> HlsTerminalAssetRevisionGuard {
    let custom_stream_response = Arc::clone(&ctx.app_config.custom_stream_response);
    let custom_video_stream_enabled = Arc::clone(&ctx.app_config.config);
    HlsTerminalAssetRevisionGuard::for_optional_runtime_tail(reason, expected, move || {
        if !custom_video_stream_enabled.load().custom_stream_response_enabled {
            return None;
        }
        custom_stream_response
            .load_full()
            .as_ref()
            .and_then(|responses| match reason {
                HlsRuntimeCustomTailReason::ChannelUnavailable => responses.channel_unavailable.as_ref(),
                HlsRuntimeCustomTailReason::LowPriorityPreempted => responses.low_priority_preempted.as_ref(),
                HlsRuntimeCustomTailReason::UserConnectionsExhausted => responses.user_connections_exhausted.as_ref(),
                HlsRuntimeCustomTailReason::ProviderConnectionsExhausted => {
                    responses.provider_connections_exhausted.as_ref()
                }
                HlsRuntimeCustomTailReason::UserAccountExpired => responses.user_account_expired.as_ref(),
                HlsRuntimeCustomTailReason::SessionOrLeaseExpired => responses.hls_session_or_lease_expired.as_ref(),
            })
            .and_then(terminal_media_asset_identity)
            .map(|media| HlsRuntimeCustomTailAssetIdentity { reason, media })
    })
}
