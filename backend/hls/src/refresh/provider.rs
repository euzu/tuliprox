//! The refresh owner retries local admission using the manifest attempt's budget.
use super::{commit::refresh_origin_work_generation_is_current, manifest_fetch_context, OriginRefreshRequest};
use crate::{
    availability::record_retryable_manifest_attempt_failure,
    manifest_fetch::{HlsManifestRecoveryUnavailableReason, OriginManifestFetchError},
    origin::{begin_hls_origin_account_io_bounded, HlsOriginAccountIoLeaseGuard},
    HlsBoundAccountAcquireErrorKind, HlsOriginIoContext,
};
use log::debug;
use std::time::Duration;
use tokio::time::Instant;

pub(super) async fn acquire_manifest_provider(
    request: &OriginRefreshRequest,
) -> Result<Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>, OriginManifestFetchError> {
    let Some(io) = request.origin_io.as_ref() else { return Ok(None) };
    let deadline = Instant::now() + crate::hls_object_body_deadline(request.origin_manifest_timeout_ms);
    loop {
        let (binding, progress_generation, target_duration) = {
            let session = request.session.read().await;
            if !refresh_origin_work_generation_is_current(&session, request) {
                return Err(OriginManifestFetchError::RecoveryUnavailable {
                    reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
                });
            }
            (
                session.origin_account_binding.clone(),
                session.origin_control.progress_generation,
                session.target_duration,
            )
        };
        let Some(binding) = binding else { return Ok(None) };
        let Some(attempt) =
            request.attempt.as_ref().filter(|attempt| !attempt.is_closed() && attempt.remaining_attempts() > 0)
        else {
            return Err(OriginManifestFetchError::RetryExhausted);
        };
        if Instant::now() >= deadline {
            log_completion(attempt, "acquire-deadline");
            return Err(OriginManifestFetchError::ProviderUnavailable(
                HlsBoundAccountAcquireErrorKind::AcquireTimedOut,
            ));
        }
        match begin_hls_origin_account_io_bounded(
            io,
            &request.session,
            &binding,
            deadline.saturating_duration_since(Instant::now()),
        )
        .await
        {
            Ok(guard) => {
                // A successful reservation is part of the following fetch, not an extra attempt.
                if Instant::now() >= deadline {
                    return Err(OriginManifestFetchError::ProviderUnavailable(
                        HlsBoundAccountAcquireErrorKind::AcquireTimedOut,
                    ));
                }
                return Ok(Some((io.clone(), guard)));
            }
            Err(kind) => {
                drop(io.take_preacquired_provider_handle());
                if !attempt.begin_logical_attempt() {
                    return Err(OriginManifestFetchError::RetryExhausted);
                }
                let error = OriginManifestFetchError::ProviderUnavailable(kind);
                let (retryable, reason) = if !kind.is_retryable_resource_failure() {
                    (false, "final-error")
                } else if attempt.remaining_attempts() == 0 {
                    (false, "budget-exhausted")
                } else if Instant::now() >= deadline {
                    (false, "acquire-deadline")
                } else {
                    (true, "retry")
                };
                log_completion(attempt, reason);
                if !retryable {
                    return Err(error);
                }
                let context = manifest_fetch_context(request, target_duration);
                context.ensure_current_origin().await?;
                record_retryable_manifest_attempt_failure(
                    &context,
                    progress_generation,
                    &error,
                    request.hls_proxy.now_ms(),
                )
                .await;
                let delay_ms = request
                    .retry_policy
                    .delay_for_attempt_ms(attempt.logical_attempts(), request.retry_policy.sample_jitter_ms())
                    .unwrap_or_default();
                debug!(
                    "HLS manifest provider retry scheduled: used={} remaining={} delay_ms={delay_ms} error={}",
                    attempt.logical_attempts(),
                    attempt.remaining_attempts(),
                    error.log_label()
                );
                tokio::time::sleep_until((Instant::now() + Duration::from_millis(delay_ms)).min(deadline)).await;
                attempt.mark_admission_backoff_paid();
            }
        }
    }
}

fn log_completion(attempt: &crate::manifest_fetch::HlsManifestAttemptContext, reason: &str) {
    debug!(
        "HLS manifest provider attempt: used={} remaining={} completion={reason}",
        attempt.logical_attempts(),
        attempt.remaining_attempts()
    );
}
