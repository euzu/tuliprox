use crate::{
    api::model::{StreamError, TransportStreamBuffer},
    tools::atomic_once_flag::AtomicOnceFlag,
};
use bytes::Bytes;
use futures::Stream;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

pub struct ProvisioningStream {
    buffer: TransportStreamBuffer,
    stop_signal: Arc<AtomicOnceFlag>,
}

impl ProvisioningStream {
    pub fn new(buffer: TransportStreamBuffer, stop_signal: Arc<AtomicOnceFlag>) -> Self { Self { buffer, stop_signal } }
}

impl Stream for ProvisioningStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.stop_signal.is_active() {
            return Poll::Ready(None);
        }

        self.buffer.register_waker(cx.waker());
        match self.buffer.next_chunk() {
            Some(chunk) => Poll::Ready(Some(Ok(chunk))),
            None => Poll::Pending,
        }
    }
}
