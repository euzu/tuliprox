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
    // One queued chunk and one pending chunk bound memory. The producer owns
    // origin resources so its absolute timer runs even if the client never polls.
    let (chunks_tx, chunks_rx) = tokio::sync::mpsc::channel(1);
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let read_guard = guard;
        let mut origin_io_guard = origin_io_guard;
        let mut finalizer = finalizer;
        let mut reader = ReaderStream::with_capacity(decoded.body, 64 * 1024);
        let mut send_deadline = Box::pin(sleep(hls_client_body_send_deadline()));
        let mut client_timed_out = false;
        let result = async {
            loop {
                let chunk = tokio::select! {
                    biased;
                    () = chunks_tx.closed() => return Ok(false),
                    () = send_deadline.as_mut() => { client_timed_out = true; return Ok(false); },
                    () = tokio::time::sleep_until(body_deadline.deadline()) => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "transient origin transfer deadline exceeded"));
                    }
                    chunk = reader.next() => chunk,
                };
                let Some(chunk) = chunk else { return Ok(true); };
                let chunk = chunk?;
                tokio::select! {
                    biased;
                    () = chunks_tx.closed() => return Ok(false),
                    () = send_deadline.as_mut() => { client_timed_out = true; return Ok(false); },
                    () = tokio::time::sleep_until(body_deadline.deadline()) => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "transient origin transfer deadline exceeded"));
                    }
                    result = chunks_tx.send(chunk) => if result.is_err() { return Ok(false); },
                }
                refresh_hls_client_body_send_deadline(send_deadline.as_mut());
            }
        }.await;
        // Stop origin I/O before lifecycle work or downstream notification.
        drop(reader);
        let logical_deadline = attempt.logical_deadline();
        let finish = async {
            match &result {
                Ok(true) => {
                    finalizer.finish(HlsTransientDirectStreamOutcome::CleanEof).await;
                    if let Some(guard) = origin_io_guard.take() {
                        guard.finish_clean().await;
                    }
                }
                Ok(false) => finalizer.finish(HlsTransientDirectStreamOutcome::ClientAborted).await,
                Err(error) => {
                    log_hls_resource_body_failure(
                        &log_identity,
                        hls_transient_direct_log_context(&resource_id, resource_kind),
                        &attempt,
                        error,
                        body_deadline.timeout().as_millis(),
                    );
                    finalizer.finish(HlsTransientDirectStreamOutcome::OriginBodyFailure).await;
                }
            }
        };
        if let Some(deadline) = logical_deadline {
            let _ = tokio::time::timeout_at(deadline, finish).await;
        } else {
            finish.await;
        }
        drop(origin_io_guard);
        drop(read_guard);
        let completion = if client_timed_out {
            Err(io::Error::new(io::ErrorKind::TimedOut, "hls client body send timed out"))
        } else {
            result.map(|_| ())
        };
        let _ = completion_tx.send(completion);
        drop(chunks_tx);
    });
    let stream = futures::stream::unfold((chunks_rx, Some(completion_rx)), |(mut chunks, mut completion)| async move {
        if let Some(chunk) = chunks.recv().await {
            return Some((Ok(chunk), (chunks, completion)));
        }
        if let Some(done) = completion.take() {
            match done.await {
                Ok(Err(error)) => return Some((Err(error), (chunks, completion))),
                Err(_) => return Some((Err(io::Error::other("origin transfer task stopped")), (chunks, completion))),
                Ok(Ok(())) => {}
            }
        }
        None
    });
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
