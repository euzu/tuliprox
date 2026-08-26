use crate::api::model::StreamError;
use bytes::Bytes;
use futures::Stream;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{sleep, Instant, Sleep};

pub struct ThrottledStream<S> {
    inner: S,
    rate_bytes_per_sec: f64,
    delay: Pin<Box<Sleep>>,
    delay_active: bool,
}

impl<S> ThrottledStream<S> {
    #[allow(clippy::cast_precision_loss)]
    pub fn new(inner: S, throttle_kbps: usize) -> Self {
        assert!(throttle_kbps > 0, "Rate must be greater than 0");
        let rate_bytes_per_sec = (throttle_kbps as f64) * 1000.0 / 8.0;
        Self { inner, rate_bytes_per_sec, delay: Box::pin(sleep(Duration::ZERO)), delay_active: false }
    }
}

impl<S> Stream for ThrottledStream<S>
where
    S: Stream<Item = Result<Bytes, StreamError>> + Unpin,
{
    type Item = Result<Bytes, StreamError>;

    #[allow(clippy::cast_precision_loss)]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        if this.delay_active {
            match this.delay.as_mut().poll(cx) {
                Poll::Ready(()) => this.delay_active = false,
                Poll::Pending => return Poll::Pending,
            }
        }

        // Poll the inner stream
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                let len = bytes.len() as f64;
                let delay_secs = (len / this.rate_bytes_per_sec).max(0.001);
                let delay_duration = Duration::from_secs_f64(delay_secs);

                this.delay.as_mut().reset(Instant::now() + delay_duration);
                this.delay_active = true;

                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                // Emit error without delaying
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: Unpin> Unpin for ThrottledStream<S> {}

#[cfg(test)]
mod tests {
    use super::ThrottledStream;
    use crate::api::model::StreamError;
    use bytes::Bytes;
    use futures::{stream, StreamExt};
    use std::{task::Poll, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn throttled_stream_delays_between_chunks_without_recreating_stream_state() {
        let inner = stream::iter([
            Ok::<Bytes, StreamError>(Bytes::from_static(b"abcd")),
            Ok::<Bytes, StreamError>(Bytes::from_static(b"efgh")),
        ]);
        let mut stream = ThrottledStream::new(inner, 32);

        assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"abcd"));
        assert!(
            matches!(futures::poll!(stream.next()), Poll::Pending),
            "second chunk must wait for the throttle delay"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"efgh"));
    }
}
