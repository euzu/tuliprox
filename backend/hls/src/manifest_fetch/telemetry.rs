//! Origin transfer telemetry for manifest requests: one record per request attempt.

use super::{error::OriginManifestFetchError, FetchedOriginManifest, HlsOriginManifestFetchContext};
use crate::origin_telemetry::{OriginFetchOutcome, OriginFetchPhaseError, OriginResourceKind};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    time::Instant,
};

#[derive(Default)]
#[allow(clippy::struct_field_names)]
pub(super) struct ManifestTiming {
    headers_at: OnceLock<Instant>,
    first_byte_at: OnceLock<Instant>,
    body_complete_at: OnceLock<Instant>,
}

impl ManifestTiming {
    #[must_use]
    pub(super) fn new() -> Self { Self::default() }

    pub(super) fn mark_headers(&self) { let _ = self.headers_at.set(Instant::now()); }

    pub(super) fn mark_first_byte(&self) { let _ = self.first_byte_at.set(Instant::now()); }

    pub(super) fn mark_body_complete(&self) { let _ = self.body_complete_at.set(Instant::now()); }

    pub(super) fn headers_at(&self) -> Option<Instant> { self.headers_at.get().copied() }

    pub(super) fn first_byte_at(&self) -> Option<Instant> { self.first_byte_at.get().copied() }

    pub(super) fn body_complete_at(&self) -> Option<Instant> { self.body_complete_at.get().copied() }
}

pub(super) struct FirstByteMark<'a, R> {
    pub(super) inner: &'a mut R,
    pub(super) timing: Option<&'a ManifestTiming>,
}

impl<'a, R> FirstByteMark<'a, R> {
    #[must_use]
    pub(super) fn new(inner: &'a mut R, timing: Option<&'a ManifestTiming>) -> Self { Self { inner, timing } }
}

impl<R: AsyncRead + Unpin> AsyncRead for FirstByteMark<'_, R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Some(timing) = this.timing else {
            return Pin::new(&mut *this.inner).poll_read(cx, buf);
        };
        let prev_len = buf.filled().len();
        let poll = Pin::new(&mut *this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            if buf.filled().len() > prev_len {
                timing.mark_first_byte();
                this.timing = None;
            }
        }
        poll
    }
}

pub(super) async fn observe_manifest_attempt<F, Fut>(
    context: &HlsOriginManifestFetchContext,
    attempt_no: usize,
    fetch: F,
) -> Result<FetchedOriginManifest, OriginManifestFetchError>
where
    F: FnOnce(Option<Arc<ManifestTiming>>) -> Fut,
    Fut: Future<Output = Result<FetchedOriginManifest, OriginManifestFetchError>>,
{
    if !context.origin_telemetry.is_enabled() {
        return fetch(None).await;
    }
    let input_name = context.session.read().await.origin_source.input_name.clone();
    // Dropping the future before it resolves drops the probe, which records the attempt as cancelled.
    let mut probe = context.origin_telemetry.begin(
        &input_name,
        OriginResourceKind::Manifest,
        None,
        u32::try_from(attempt_no).unwrap_or(u32::MAX),
    );
    let timing = Arc::new(ManifestTiming::new());
    let result = fetch(Some(Arc::clone(&timing))).await;
    match &result {
        Ok(fetched) => {
            if let Some(headers_at) = timing.headers_at() {
                probe.mark_headers_at(headers_at, Some(fetched.status.as_u16()), None);
            } else {
                probe.mark_headers(fetched.status.as_u16(), None);
            }
            if let Some(first_byte_at) = timing.first_byte_at() {
                probe.mark_first_byte_at(first_byte_at);
            }
            probe.add_bytes(fetched.body.len() as u64);
            if let Some(body_complete_at) = timing.body_complete_at() {
                probe.mark_body_complete_at(body_complete_at);
            } else {
                probe.mark_body_complete();
            }
            probe.finish(OriginFetchOutcome::Ok, None);
        }
        Err(error) => {
            let headers_marked = timing.headers_at().is_some();
            let (outcome, phase) = classify_manifest_error(error, headers_marked);
            let status = error_status(error);
            if let Some(headers_at) = timing.headers_at() {
                probe.mark_headers_at(headers_at, status, None);
            } else if let Some(status) = status {
                probe.mark_headers(status, None);
            }
            if let Some(first_byte_at) = timing.first_byte_at() {
                probe.mark_first_byte_at(first_byte_at);
            }
            if let Some(body_complete_at) = timing.body_complete_at() {
                probe.mark_body_complete_at(body_complete_at);
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

fn classify_manifest_error(
    error: &OriginManifestFetchError,
    headers_marked: bool,
) -> (OriginFetchOutcome, OriginFetchPhaseError) {
    use OriginManifestFetchError as E;
    match error {
        E::PermanentStatus(_) | E::RetryableStatus(..) | E::NonRetryableStatus(_) => {
            (OriginFetchOutcome::HttpError, OriginFetchPhaseError::Headers)
        }
        E::Timeout => (
            OriginFetchOutcome::Timeout,
            if headers_marked { OriginFetchPhaseError::Body } else { OriginFetchPhaseError::Headers },
        ),
        E::ContentCoding(_)
        | E::ContentDecoding { .. }
        | E::DecodedBodyLimitExceeded { .. }
        | E::InvalidUtf8 { .. } => (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Body),
        E::RetryExhausted
        | E::RecoveryUnavailable { .. }
        | E::ProviderUnavailable(_)
        | E::DeterministicTimelineConflict(_)
        | E::LocalRepresentationLimit(_)
        | E::MalformedTransientRepresentation
        | E::CommitGenerationExhausted => (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Local),
        _ => (
            OriginFetchOutcome::TransportError,
            if headers_marked { OriginFetchPhaseError::Body } else { OriginFetchPhaseError::Connect },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        deterministic_conflict::{HlsDeterministicConflictFingerprint, HlsDeterministicTimelineConflict},
        manifest_fetch::error::HlsManifestRecoveryUnavailableReason,
        manifest_limits::{HlsManifestLimitKind, HlsManifestLimitViolation},
        resource_identity::HlsMediaResourceIdentity,
        timeline::HlsResourceReplayDecision,
        HlsBoundAccountAcquireErrorKind,
    };
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn first_byte_mark_disabled_passes_through() {
        let mut data = &b"hello world"[..];
        let mut reader = FirstByteMark::new(&mut data, None);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.expect("read");
        assert_eq!(buf, b"hello world");
    }

    #[tokio::test]
    async fn first_byte_mark_enabled_records_on_first_filled_bytes() {
        let timing = ManifestTiming::new();
        assert!(timing.first_byte_at().is_none());

        let mut data = &b"hello world"[..];
        let mut reader = FirstByteMark::new(&mut data, Some(&timing));
        let mut byte = [0u8; 1];
        let n = reader.read(&mut byte).await.expect("read 1");
        assert_eq!(n, 1);
        assert!(timing.first_byte_at().is_some());
        assert!(reader.timing.is_none());

        let first_marked = timing.first_byte_at();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.expect("read rest");
        assert_eq!(rest, b"ello world");
        assert_eq!(timing.first_byte_at(), first_marked);
    }

    #[test]
    fn classify_manifest_error_local_variants() {
        let dummy_conflict = HlsDeterministicTimelineConflict {
            previous_proxy_tail: None,
            existing_proxy_seq: 0,
            candidate_position: 0,
            candidate_origin_seq: 0,
            resource_key: HlsMediaResourceIdentity::from_url("http://origin.example.com/1.ts", None).semantic_key(),
            decision: HlsResourceReplayDecision::RejectContradictoryOrder,
            candidate_fingerprint: HlsDeterministicConflictFingerprint {
                segment_count: 0,
                first_program_date_time_ms: None,
                last_program_date_time_ms: None,
                duration_pattern_hash: [0; 32],
                discontinuity_pattern_hash: [0; 32],
                semantic_resource_pattern_hash: None,
                map_and_encryption_hash: [0; 32],
                container_signature_hash: [0; 32],
                segment_samples: Vec::new(),
            },
        };

        let local_cases: Vec<OriginManifestFetchError> = vec![
            OriginManifestFetchError::RetryExhausted,
            OriginManifestFetchError::RecoveryUnavailable {
                reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            },
            OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Unavailable),
            OriginManifestFetchError::DeterministicTimelineConflict(Box::new(dummy_conflict)),
            OriginManifestFetchError::LocalRepresentationLimit(HlsManifestLimitViolation::new(
                HlsManifestLimitKind::TransientResources,
                10,
                5,
            )),
            OriginManifestFetchError::MalformedTransientRepresentation,
            OriginManifestFetchError::CommitGenerationExhausted,
        ];

        for error in local_cases {
            let (outcome, phase) = classify_manifest_error(&error, false);
            assert_eq!(outcome, OriginFetchOutcome::TransportError);
            assert_eq!(phase, OriginFetchPhaseError::Local);

            let (outcome, phase) = classify_manifest_error(&error, true);
            assert_eq!(outcome, OriginFetchOutcome::TransportError);
            assert_eq!(phase, OriginFetchPhaseError::Local);
        }

        let req_err = OriginManifestFetchError::Request("req failed".into());
        assert_eq!(
            classify_manifest_error(&req_err, false),
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Connect)
        );
        assert_eq!(
            classify_manifest_error(&req_err, true),
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Body)
        );

        let redir_err = OriginManifestFetchError::Redirect("redir failed".into());
        assert_eq!(
            classify_manifest_error(&redir_err, false),
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Connect)
        );
        assert_eq!(
            classify_manifest_error(&redir_err, true),
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Body)
        );
    }
}
