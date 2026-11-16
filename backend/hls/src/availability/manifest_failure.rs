//! Publish degraded origin evidence before the manifest retry loop starts waiting.

use super::reevaluation::register_terminal_reserve_reevaluation;
use crate::{
    manifest_fetch::{HlsOriginManifestFetchContext, OriginManifestFetchError},
    origin_progress::HlsOriginPathCondition,
    post_refresh_availability::commit_post_refresh_terminal_fallback,
    refresh::{HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason},
};
use std::sync::Arc;
use tuliprox_core::model::PlaybackKind;

pub(crate) const HLS_FAST_MANIFEST_RETRY_MAX_DELAY_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestRetryUrgency {
    Normal,
    FirstFastRetry,
}

pub(crate) async fn record_retryable_manifest_attempt_failure(
    context: &HlsOriginManifestFetchContext,
    expected_progress_generation: u64,
    error: &OriginManifestFetchError,
    now_ms: u64,
    urgency: ManifestRetryUrgency,
) {
    let action = {
        let mut session = context.session.write().await;
        if session.is_gc_marked_for_removal()
            || session.origin_control.progress_generation != expected_progress_generation
        {
            return;
        }
        if matches!(error, OriginManifestFetchError::RetryableStatus(..)) {
            session.origin_control.record_origin_response(now_ms);
        }
        // Give a healthy live session (no degraded path, publication not late) its first short scheduled retry
        // before it commits ChannelUnavailable. The degradation is not written either: an unchanged manifest on the
        // retry would not clear it, and no reevaluation owner would exist. A failed retry records it with the normal
        // policy.
        if urgency == ManifestRetryUrgency::FirstFastRetry
            && session.origin_playback_kind() == PlaybackKind::LiveHls
            && session.origin_refresh.in_flight
            && !session.origin_control.path_is_degraded_at(now_ms)
        {
            return;
        }
        // An individual soft failure must not weaken an already established hard failure or conflict.
        if matches!(
            session.origin_control.path_condition,
            HlsOriginPathCondition::ProgressExpected | HlsOriginPathCondition::RetryableFetchFailure
        ) {
            session.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
        }
        HlsPostRefreshAvailabilityAction::Reevaluate {
            reason: HlsPostRefreshAvailabilityReason::RetryableManifestFailure,
            origin_progress_generation: session.origin_control.progress_generation,
            media_readiness_generation: session.activity.media_readiness_generation,
        }
    };
    let Some(ctx) = context.availability_runtime.as_ref().and_then(crate::hls_ctx::WeakHlsCtx::upgrade) else {
        return;
    };
    ctx.hls_proxy.sync_recovery_binding_protection(&ctx.active_provider, &context.session, now_ms).await;
    let registration = register_terminal_reserve_reevaluation(ctx.clone(), Arc::clone(&context.session)).await;
    let _ = commit_post_refresh_terminal_fallback(ctx, Arc::clone(&context.session), action, registration).await;
}
