use crate::{
    api::model::{BoxedProviderStream, StreamError},
    utils::trace_if_enabled,
};
use bytes::Bytes;
use futures::Stream;
use log::trace;
use shared::utils::sanitize_sensitive_info;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Poll,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Number of consecutive empty keep-alive chunks tolerated in a single poll
/// before backing off, to avoid spinning on a misbehaving provider.
const EMPTY_CHUNK_SKIP_LIMIT: u32 = 10;
/// Cool-down applied after exceeding [`EMPTY_CHUNK_SKIP_LIMIT`] so an endless
/// run of empty chunks cannot keep the task hot via immediate re-wakes.
const EMPTY_CHUNK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

/// This stream counts the send bytes for reconnecting to the actual position and
/// sets the `close_signal`  if the client drops the connection.
pub(in crate::api::model) struct ClientStream {
    inner: BoxedProviderStream,
    close_signal: CancellationToken,
    close_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    total_bytes: Option<Arc<AtomicUsize>>,
    empty_backoff: Option<Pin<Box<tokio::time::Sleep>>>,
    url: String,
}

impl ClientStream {
    pub(crate) fn new(
        inner: BoxedProviderStream,
        close_signal: CancellationToken,
        total_bytes: Option<Arc<AtomicUsize>>,
        url: &str,
    ) -> Self {
        let close_cancelled = Box::pin(close_signal.clone().cancelled_owned());
        Self { inner, close_signal, close_cancelled, total_bytes, empty_backoff: None, url: url.to_string() }
    }
}

impl Stream for ClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.close_cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        // If we are cooling down after a run of empty keep-alive chunks, wait for
        // the timer to elapse before polling the provider again.
        if let Some(backoff) = this.empty_backoff.as_mut() {
            match backoff.as_mut().poll(cx) {
                Poll::Ready(()) => this.empty_backoff = None,
                Poll::Pending => return Poll::Pending,
            }
        }
        // Bound empty-chunk skips per poll invocation.  A misbehaving provider
        // that sends an endless run of empty keep-alive chunks must not spin
        // the executor indefinitely: after EMPTY_CHUNK_SKIP_LIMIT consecutive
        // empty chunks we back off for a short cool-down before retrying.
        let mut empty_chunk_count = 0u32;
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if bytes.is_empty() {
                        // Skip keep-alive empty chunks rather than treating them as EOF.
                        // Some providers send empty chunks as heartbeats; closing on them
                        // would prematurely terminate valid streams.
                        empty_chunk_count += 1;
                        if empty_chunk_count > EMPTY_CHUNK_SKIP_LIMIT {
                            let mut backoff = Box::pin(tokio::time::sleep(EMPTY_CHUNK_BACKOFF));
                            // Poll once so the timer registers our waker.
                            let _ = backoff.as_mut().poll(cx);
                            this.empty_backoff = Some(backoff);
                            return Poll::Pending;
                        }
                        trace!("client stream: skipping empty keep-alive chunk");
                        continue;
                    }
                    if let Some(counter) = &this.total_bytes {
                        counter.fetch_add(bytes.len(), Ordering::Relaxed);
                    }
                    return Poll::Ready(Some(Ok(bytes)));
                }
                Poll::Ready(None) => {
                    this.close_signal.cancel();
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(err))) => {
                    trace!("client stream error: {err}");
                    this.close_signal.cancel();
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for ClientStream {
    fn drop(&mut self) {
        trace_if_enabled!("Client disconnected {}", sanitize_sensitive_info(&self.url));
        self.close_signal.cancel();
    }
}
