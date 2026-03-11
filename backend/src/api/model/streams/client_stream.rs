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

/// This stream counts the send bytes for reconnecting to the actual position and
/// sets the `close_signal`  if the client drops the connection.
pub(in crate::api::model) struct ClientStream {
    inner: BoxedProviderStream,
    close_signal: CancellationToken,
    close_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    total_bytes: Option<Arc<AtomicUsize>>,
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
        Self { inner, close_signal, close_cancelled, total_bytes, url: url.to_string() }
    }
}

impl Stream for ClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.close_cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        // Bound empty-chunk skips per poll invocation.  A misbehaving provider
        // that sends an endless run of empty keep-alive chunks must not spin
        // the executor indefinitely: after 10 consecutive empty chunks we
        // yield back to the runtime via wake_by_ref + Poll::Pending.
        let mut empty_chunk_count = 0u32;
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if bytes.is_empty() {
                        // Skip keep-alive empty chunks rather than treating them as EOF.
                        // Some providers send empty chunks as heartbeats; closing on them
                        // would prematurely terminate valid streams.
                        empty_chunk_count += 1;
                        if empty_chunk_count > 10 {
                            cx.waker().wake_by_ref();
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
