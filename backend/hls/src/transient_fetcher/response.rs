use super::{
    hls_client_body_send_deadline, hls_transient_resource_fetch_kind,
    lifecycle::transient_resource_affects_media_readiness, log_hls_resource_body_failure,
    record_successful_transient_segment_fetch, record_temporary_transient_segment_fetch_failure,
    refresh_hls_client_body_send_deadline, HlsLogIdentity, HlsResourceFetchLogContext, HlsResourceFetchSource,
    HlsSessionHandle, HlsTransientDecodedOriginResponse, HlsTransientDirectResponseFinalizer,
    HlsTransientDirectResponseLifecycleContext, HlsTransientOriginIoGuard, HlsTransientReadGuard, SegmentFetchPolicy,
    TransientResourceKind, TransientResourceRef,
};
use axum::{body::Body, http::header, response::IntoResponse};
use futures::StreamExt;
use log::warn;
use std::{io, sync::Arc};
use tokio::time::sleep;
use tokio_util::io::ReaderStream;
use tuliprox_core::{
    try_unwrap_body,
    utils::{current_time_millis, response_compression::mark_response_as_uncompressed},
};

/// Context retained until a direct transient response body reaches one terminal outcome.
pub struct HlsTransientDirectResponseContext {
    pub session: HlsSessionHandle,
    pub resource: TransientResourceRef,
    pub policy: SegmentFetchPolicy,
    pub now_ms: u64,
    pub log_identity: HlsLogIdentity,
}

#[derive(Clone, Copy)]
pub(super) enum HlsTransientDirectStreamOutcome {
    CleanEof,
    OriginBodyFailure,
    ClientAborted,
}

impl HlsTransientDirectResponseFinalizer {
    pub(super) fn new(context: HlsTransientDirectResponseLifecycleContext) -> Self {
        let context = transient_resource_affects_media_readiness(context.resource.kind).then_some(context);
        Self { context }
    }

    pub(super) async fn finish(&mut self, outcome: HlsTransientDirectStreamOutcome) {
        let Some(completion) = self.begin_finish(outcome) else {
            return;
        };
        if let Err(error) = completion.await {
            warn!(
                "HLS transient direct lifecycle task failed: cancelled={} panic={}",
                error.is_cancelled(),
                error.is_panic()
            );
        }
    }

    pub(super) fn begin_finish(
        &mut self,
        outcome: HlsTransientDirectStreamOutcome,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let context = self.context.take()?;
        if matches!(outcome, HlsTransientDirectStreamOutcome::ClientAborted) {
            return None;
        }
        // Once EOF or an origin-body failure has been observed, its state transition
        // must survive cancellation of the downstream body poll.
        Some(tokio::spawn(async move {
            match outcome {
                HlsTransientDirectStreamOutcome::CleanEof => {
                    record_successful_transient_segment_fetch(&context.session, &context.resource).await;
                }
                HlsTransientDirectStreamOutcome::OriginBodyFailure => {
                    let failed_at_ms = current_time_millis();
                    record_temporary_transient_segment_fetch_failure(
                        &context.session,
                        &context.resource,
                        &context.policy,
                        failed_at_ms,
                    )
                    .await;
                }
                HlsTransientDirectStreamOutcome::ClientAborted => {}
            }
        }))
    }

    pub(super) fn finish_client_aborted(&mut self) { self.context.take(); }
}

impl Drop for HlsTransientDirectResponseFinalizer {
    fn drop(&mut self) {
        // A body dropped before EOF is a downstream abort. It must release the
        // retained guards without changing provider/segment failure state.
        self.finish_client_aborted();
    }
}

pub fn hls_transient_origin_response(
    response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
    context: HlsTransientDirectResponseContext,
) -> axum::response::Response {
    let mut builder = axum::response::Response::builder().status(response.decoded.status);
    for header_name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CACHE_CONTROL,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        if let Some(value) = response.decoded.headers.get(&header_name) {
            builder = builder.header(header_name, value.clone());
        }
    }

    let mut response = try_unwrap_body!(builder.body(hls_transient_direct_body(response, context)));
    mark_response_as_uncompressed(&mut response);
    response
}

fn hls_transient_direct_body(
    response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
    context: HlsTransientDirectResponseContext,
) -> Body {
    let HlsTransientDecodedOriginResponse { decoded, body_deadline, attempt, guard: origin_io_guard } = response;
    let HlsTransientDirectResponseContext { session, resource, policy, now_ms, log_identity } = context;

    let guard = HlsTransientReadGuard::new(Arc::clone(&resource.access), now_ms);
    let finalizer = HlsTransientDirectResponseFinalizer::new(HlsTransientDirectResponseLifecycleContext {
        session,
        resource: resource.clone(),
        policy,
    });
    let resource_id = resource.id.0.clone();
    let resource_kind = resource.kind;
    let stream = futures::stream::unfold(
        (
            ReaderStream::new(decoded.body),
            Some(guard),
            origin_io_guard,
            finalizer,
            Box::pin(sleep(hls_client_body_send_deadline())),
            false,
        ),
        move |(mut stream, guard, mut origin_io_guard, mut finalizer, mut send_deadline, finished)| {
            let log_identity = log_identity.clone();
            let resource_id = resource_id.clone();
            async move {
                if finished {
                    return None;
                }
                let next_chunk = tokio::select! {
                    () = send_deadline.as_mut() => {
                        finalizer.finish(HlsTransientDirectStreamOutcome::ClientAborted).await;
                        return Some((
                            Err(io::Error::new(io::ErrorKind::TimedOut, "hls client body send timed out")),
                            (stream, None, None, finalizer, send_deadline, true),
                        ));
                    }
                    // Decoder setup is bounded by the absolute attempt deadline before this
                    // response is built. Once handed to the client, retain the existing
                    // per-chunk origin-body idle timeout instead of imposing a total-body limit.
                    next_chunk = tokio::time::timeout(body_deadline.timeout(), stream.next()) => next_chunk,
                };
                match next_chunk {
                    Ok(Some(Ok(chunk))) => {
                        refresh_hls_client_body_send_deadline(send_deadline.as_mut());
                        Some((Ok(chunk), (stream, guard, origin_io_guard, finalizer, send_deadline, false)))
                    }
                    Ok(Some(Err(err))) => {
                        log_hls_resource_body_failure(
                            &log_identity,
                            hls_transient_direct_log_context(&resource_id, resource_kind),
                            attempt,
                            &err,
                            body_deadline.timeout().as_millis(),
                        );
                        finalizer.finish(HlsTransientDirectStreamOutcome::OriginBodyFailure).await;
                        Some((Err(err), (stream, None, None, finalizer, send_deadline, true)))
                    }
                    Ok(None) => {
                        finalizer.finish(HlsTransientDirectStreamOutcome::CleanEof).await;
                        if let Some(guard) = origin_io_guard.take() {
                            guard.finish_clean().await;
                        }
                        None
                    }
                    Err(_) => {
                        let error = io::Error::new(io::ErrorKind::TimedOut, "transient passthrough body timed out");
                        log_hls_resource_body_failure(
                            &log_identity,
                            hls_transient_direct_log_context(&resource_id, resource_kind),
                            attempt,
                            &error,
                            body_deadline.timeout().as_millis(),
                        );
                        finalizer.finish(HlsTransientDirectStreamOutcome::OriginBodyFailure).await;
                        Some((Err(error), (stream, None, None, finalizer, send_deadline, true)))
                    }
                }
            }
        },
    );
    Body::from_stream(stream)
}

fn hls_transient_direct_log_context(
    resource_id: &str,
    resource_kind: TransientResourceKind,
) -> HlsResourceFetchLogContext<'_> {
    HlsResourceFetchLogContext {
        kind: hls_transient_resource_fetch_kind(resource_kind),
        source: HlsResourceFetchSource::Transient,
        object_id: resource_id,
        origin_url: None,
    }
}
