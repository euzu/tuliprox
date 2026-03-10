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
    total_bytes: Arc<Option<AtomicUsize>>,
    url: String,
}

impl ClientStream {
    pub(crate) fn new(
        inner: BoxedProviderStream,
        close_signal: CancellationToken,
        total_bytes: Arc<Option<AtomicUsize>>,
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
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if bytes.is_empty() {
                    trace!("client stream empty bytes");
                    this.close_signal.cancel();
                } else if let Some(counter) = this.total_bytes.as_ref() {
                    counter.fetch_add(bytes.len(), Ordering::Relaxed);
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(None) => {
                this.close_signal.cancel();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                trace!("client stream error: {err}");
                this.close_signal.cancel();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ClientStream {
    fn drop(&mut self) {
        trace_if_enabled!("Client disconnected {}", sanitize_sensitive_info(&self.url));
        self.close_signal.cancel();
    }
}
