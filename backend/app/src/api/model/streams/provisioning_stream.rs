use crate::api::model::{StreamError, TransportStreamBuffer, STREAM_IDLE_TIMEOUT};
use bytes::Bytes;
use futures::Stream;
use log::debug;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{sleep_until, Instant, Sleep};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

pub struct ProvisioningStream {
    buffer: TransportStreamBuffer,
    stop_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    idle_deadline: Pin<Box<Sleep>>,
}

impl ProvisioningStream {
    pub fn new(buffer: TransportStreamBuffer, stop_signal: CancellationToken) -> Self {
        let idle_deadline = Box::pin(sleep_until(Instant::now() + Duration::from_secs(STREAM_IDLE_TIMEOUT)));
        Self { buffer, stop_cancelled: Box::pin(stop_signal.cancelled_owned()), idle_deadline }
    }
}

impl Stream for ProvisioningStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.stop_cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }

        if self.idle_deadline.as_mut().poll(cx).is_ready() {
            debug!("Provisioning stream idle timeout reached; terminating client stream");
            return Poll::Ready(None);
        }

        if let Some(chunk) = self.buffer.next_chunk() {
            // Reset idle deadline on each delivered chunk.
            self.idle_deadline
                .as_mut()
                .reset(Instant::now() + Duration::from_secs(STREAM_IDLE_TIMEOUT));
            Poll::Ready(Some(Ok(chunk)))
        } else {
            self.buffer.register_waker(cx.waker());
            Poll::Pending
        }
    }
}
