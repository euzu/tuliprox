use crate::api::model::StreamError;
use crate::api::model::TransportStreamBuffer;
use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};


pub struct CustomVideoStream {
    buffer: TransportStreamBuffer,
}

impl CustomVideoStream {
    pub fn new(buffer: TransportStreamBuffer) -> Self {
        Self {
            buffer
        }
    }
}

impl Stream for CustomVideoStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.buffer.register_waker(cx.waker());
        match self.buffer.next_chunk() {
            Some(chunk) => Poll::Ready(Some(Ok(chunk))),
            None => Poll::Pending,
        }
    }
}
