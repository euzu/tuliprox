use crate::{
    api::model::{BoxedProviderStream, StreamError},
    utils::trace_if_enabled,
};
use bytes::Bytes;
use futures::Stream;
use log::trace;
use shared::utils::sanitize_sensitive_info;
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Poll,
};
use tokio_util::sync::CancellationToken;

/// This stream counts the send bytes for reconnecting to the actual position and
/// sets the `close_signal`  if the client drops the connection.
#[repr(align(64))]
pub(in crate::api::model) struct ClientStream {
    inner: BoxedProviderStream,
    close_signal: CancellationToken,
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
        Self { inner, close_signal, total_bytes, url: url.to_string() }
    }
}
impl Stream for ClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        if self.close_signal.is_cancelled() {
            Poll::Ready(None)
        } else {
            match Pin::as_mut(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if bytes.is_empty() {
                        trace!("client stream empty bytes");
                        // Empty payload signals upstream closure; notify and let consumer see final chunk
                        self.close_signal.cancel();
                    } else if let Some(counter) = self.total_bytes.as_ref() {
                        counter.fetch_add(bytes.len(), Ordering::AcqRel);
                    }

                    Poll::Ready(Some(Ok(bytes)))
                }
                Poll::Ready(None) => {
                    self.close_signal.cancel();
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Err(err))) => {
                    trace!("client stream error: {err}");
                    self.close_signal.cancel();
                    Poll::Ready(Some(Err(err)))
                }
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
