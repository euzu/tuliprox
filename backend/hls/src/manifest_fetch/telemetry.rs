//! Origin transfer telemetry for manifest requests: one record per request attempt.
//!
//! The request and its body read happen in one future, so only the totals are observed:
//! header and first-byte marks coincide with completion, and a timeout carries no finer phase.

use super::{error::OriginManifestFetchError, FetchedOriginManifest, HlsOriginManifestFetchContext};
use crate::origin_telemetry::{OriginFetchOutcome, OriginFetchPhaseError, OriginResourceKind};
use std::future::Future;

pub(super) async fn observe_manifest_attempt<F>(
    context: &HlsOriginManifestFetchContext,
    attempt_no: usize,
    fetch: F,
) -> Result<FetchedOriginManifest, OriginManifestFetchError>
where
    F: Future<Output = Result<FetchedOriginManifest, OriginManifestFetchError>>,
{
    if !context.origin_telemetry.is_enabled() {
        return fetch.await;
    }
    let input_name = context.session.read().await.origin_source.input_name.clone();
    // Dropping the future before it resolves drops the probe, which records the attempt as cancelled.
    let mut probe = context.origin_telemetry.begin(
        &input_name,
        OriginResourceKind::Manifest,
        None,
        u32::try_from(attempt_no).unwrap_or(u32::MAX),
    );
    let result = fetch.await;
    match &result {
        Ok(fetched) => {
            probe.mark_headers(fetched.status.as_u16(), None);
            probe.add_bytes(fetched.body.len() as u64);
            probe.mark_body_complete();
            probe.finish(OriginFetchOutcome::Ok, None);
        }
        Err(error) => {
            let (outcome, phase) = classify_manifest_error(error);
            if let Some(status) = error_status(error) {
                probe.mark_headers(status, None);
            }
            probe.finish(outcome, Some(phase));
        }
    }
    result
}

fn error_status(error: &OriginManifestFetchError) -> Option<u16> {
    match error {
        OriginManifestFetchError::PermanentStatus(status)
        | OriginManifestFetchError::RetryableStatus(status, _)
        | OriginManifestFetchError::NonRetryableStatus(status) => Some(status.as_u16()),
        _ => None,
    }
}

fn classify_manifest_error(error: &OriginManifestFetchError) -> (OriginFetchOutcome, OriginFetchPhaseError) {
    use OriginManifestFetchError as E;
    match error {
        E::PermanentStatus(_) | E::RetryableStatus(..) | E::NonRetryableStatus(_) => {
            (OriginFetchOutcome::HttpError, OriginFetchPhaseError::Headers)
        }
        E::Timeout => (OriginFetchOutcome::Timeout, OriginFetchPhaseError::Headers),
        E::ContentCoding(_)
        | E::ContentDecoding { .. }
        | E::DecodedBodyLimitExceeded { .. }
        | E::InvalidUtf8 { .. } => (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Body),
        _ => (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Connect),
    }
}
