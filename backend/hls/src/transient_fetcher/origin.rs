use super::{
    build_hls_origin_resource_headers_with_client_range, run_hls_origin_resource_retry_loop_with_attempt_prepare,
    HlsLogIdentity, HlsOriginByteRangeExpectation, HlsOriginResourceBodyDeadline, HlsOriginResourceClients,
    HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsResourceFetchAttempt, HlsResourceFetchKind,
    HlsResourceFetchSource, SegmentFetchPolicy, TransientResourceFile, TransientResourceKind,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use futures::{future::BoxFuture, FutureExt};
use tuliprox_core::utils::content_coding::DecodedHttpResponse;

pub enum HlsTransientObjectFetchFailure {
    Retryable,
    Permanent { status: Option<StatusCode> },
}

pub fn hls_transient_object_fetch_failure(error: &HlsOriginResourceFetchError) -> HlsTransientObjectFetchFailure {
    if let Some(status) = error.permanent_status() {
        return HlsTransientObjectFetchFailure::Permanent { status: Some(status) };
    }
    if !error.retryable_failure() {
        return HlsTransientObjectFetchFailure::Permanent { status: None };
    }
    HlsTransientObjectFetchFailure::Retryable
}

pub fn hls_transient_resource_fetch_kind(resource_kind: TransientResourceKind) -> HlsResourceFetchKind {
    match resource_kind {
        TransientResourceKind::Key => HlsResourceFetchKind::Key,
        TransientResourceKind::Map => HlsResourceFetchKind::Map,
        TransientResourceKind::Segment => HlsResourceFetchKind::Segment,
        TransientResourceKind::Part => HlsResourceFetchKind::Part,
        TransientResourceKind::Other => HlsResourceFetchKind::Other,
    }
}

pub(super) fn build_hls_transient_resource_fetch_target(
    resolved_origin_uri: &str,
    origin_headers: &HeaderMap,
    origin_provider_session_headers: &HeaderMap,
    mode: HlsTransientOriginFetchMode,
    resource_id: &str,
    resource_kind: TransientResourceKind,
) -> Result<HlsOriginResourceFetchTarget, HlsOriginResourceFetchError> {
    let (range_header, byte_range_expectation) = match mode {
        HlsTransientOriginFetchMode::CacheFullObject => (None, HlsOriginByteRangeExpectation::FullObject),
        HlsTransientOriginFetchMode::DirectPassthrough { client_range } => {
            (client_range, HlsOriginByteRangeExpectation::AnySuccess)
        }
    };
    Ok(HlsOriginResourceFetchTarget {
        kind: hls_transient_resource_fetch_kind(resource_kind),
        source: HlsResourceFetchSource::Transient,
        object_id: resource_id.to_string(),
        origin_url: resolved_origin_uri.to_string(),
        headers: build_hls_origin_resource_headers_with_client_range(
            origin_headers,
            origin_provider_session_headers,
            range_header,
        )?,
        byte_range_expectation,
    })
}

/// Keeps full-object cache fills separate from decoded client-range passthrough.
pub(super) enum HlsTransientOriginFetchMode {
    CacheFullObject,
    DirectPassthrough { client_range: Option<HeaderValue> },
}

pub struct HlsTransientOriginFetchRequest {
    pub resolved_origin_uri: String,
    pub origin_headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub range_header: Option<HeaderValue>,
    pub resource_file: TransientResourceFile,
    pub resource_kind: TransientResourceKind,
    pub clients: HlsOriginResourceClients,
    pub policy: SegmentFetchPolicy,
    pub log_identity: HlsLogIdentity,
}

/// A decoded direct-origin response together with the attempt and guards that own its origin work.
pub struct HlsTransientDecodedOriginResponse<G> {
    pub decoded: DecodedHttpResponse,
    pub body_deadline: HlsOriginResourceBodyDeadline,
    pub attempt: HlsResourceFetchAttempt,
    pub guard: G,
}

pub async fn fetch_hls_transient_origin_response_with_attempt_prepare<G, P>(
    request: HlsTransientOriginFetchRequest,
    prepare_attempt: P,
) -> Result<HlsTransientDecodedOriginResponse<G>, HlsOriginResourceFetchError>
where
    G: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
{
    let target = build_hls_transient_resource_fetch_target(
        &request.resolved_origin_uri,
        &request.origin_headers,
        &request.origin_provider_session_headers,
        HlsTransientOriginFetchMode::DirectPassthrough { client_range: request.range_header },
        request.resource_file.resource_id.0.as_str(),
        request.resource_kind,
    )?;
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        request.clients,
        &request.policy,
        &request.log_identity,
        prepare_attempt,
        |guard| async move { drop(guard) }.boxed(),
        |decoded, attempt, body_deadline, guard| {
            async move { Ok(HlsTransientDecodedOriginResponse { decoded, body_deadline, attempt, guard }) }.boxed()
        },
    )
    .await
}
