//! Publish degraded origin evidence before the manifest retry loop starts waiting.

use super::reevaluation::register_terminal_reserve_reevaluation;
use crate::{
    manifest_fetch::{HlsOriginManifestFetchContext, OriginManifestFetchError},
    origin_progress::HlsOriginPathCondition,
    post_refresh_availability::commit_post_refresh_terminal_fallback,
    refresh::{HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason},
};
use std::sync::Arc;

pub(crate) async fn record_retryable_manifest_attempt_failure(
    context: &HlsOriginManifestFetchContext,
    expected_progress_generation: u64,
    error: &OriginManifestFetchError,
    now_ms: u64,
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
    let registration = register_terminal_reserve_reevaluation(ctx.clone(), Arc::clone(&context.session)).await;
    let _ = commit_post_refresh_terminal_fallback(ctx, Arc::clone(&context.session), action, registration).await;
}
