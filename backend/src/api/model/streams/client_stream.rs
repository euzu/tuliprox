use bytes::Bytes;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Poll};
use futures::{Stream};
use log::trace;
use shared::utils::sanitize_sensitive_info;
use crate::api::model::stream::BoxedProviderStream;
use crate::api::model::stream_error::StreamError;
use crate::utils::trace_if_enabled;
use crate::tools::atomic_once_flag::AtomicOnceFlag;

/// This stream counts the send bytes for reconnecting to the actual position and
/// sets the `close_signal`  if the client drops the connection.
#[repr(align(64))]
pub(in crate::api::model) struct ClientStream {
    inner: BoxedProviderStream,
    close_signal: Arc<AtomicOnceFlag>,
    total_bytes: Arc<Option<AtomicUsize>>,
    url: String,
}

impl ClientStream {
    pub(crate) fn new(inner: BoxedProviderStream, close_signal: Arc<AtomicOnceFlag>, total_bytes: Arc<Option<AtomicUsize>>, url: &str) -> Self {
        Self { inner, close_signal, total_bytes, url: url.to_string() }
    }
}
impl Stream for ClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.close_signal.is_active() {
            loop {
                match Pin::as_mut(&mut self.inner).poll_next(cx) {
                    Poll::Ready(Some(Ok(bytes))) => {
                        if bytes.is_empty() {
                            trace!("client stream empty bytes");
                            continue;
                        }

                        if let Some(counter) = self.total_bytes.as_ref() {
                            counter.fetch_add(bytes.len(), Ordering::SeqCst);
                        }

                        return Poll::Ready(Some(Ok(bytes)));
                    }
                    Poll::Ready(None) => {
                        self.close_signal.notify();
                        return Poll::Ready(None);
                    }
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Err(err))) => {
                        trace!("client stream error: {err}");
                    }
                }
            }
        } else {
            Poll::Ready(None)
        }
    }
}


impl Drop for ClientStream {
    fn drop(&mut self) {
        trace_if_enabled!("Client disconnected {}", sanitize_sensitive_info(&self.url));
        self.close_signal.notify();
    }
}