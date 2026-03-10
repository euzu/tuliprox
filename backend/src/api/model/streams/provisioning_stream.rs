use crate::{
    api::model::{StreamError, TransportStreamBuffer},
};
use bytes::Bytes;
use futures::Stream;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

pub struct ProvisioningStream {
    buffer: TransportStreamBuffer,
    stop_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl ProvisioningStream {
    pub fn new(buffer: TransportStreamBuffer, stop_signal: CancellationToken) -> Self {
        Self { buffer, stop_cancelled: Box::pin(stop_signal.cancelled_owned()) }
    }
}

impl Stream for ProvisioningStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.stop_cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }

        self.buffer.register_waker(cx.waker());
        match self.buffer.next_chunk() {
            Some(chunk) => Poll::Ready(Some(Ok(chunk))),
            None => Poll::Pending,
        }
    }
}
