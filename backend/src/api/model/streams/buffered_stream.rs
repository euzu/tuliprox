use crate::{
    api::model::{BoxedProviderStream, StreamError, STREAM_IDLE_TIMEOUT},
};
use futures::{
    stream::Stream,
    task::{Context, Poll},
    StreamExt,
};
use log::debug;
use std::{cmp::max, future::Future, pin::Pin, sync::Arc};
use tokio::{
    select,
    sync::{
        mpsc::{error::TrySendError, channel, Sender},
        Semaphore,
    },
    time::{sleep, Duration, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

pub const CHANNEL_SIZE: usize = 1024;
pub const MAX_BUFFER_BYTES: usize = 5 * 1024 * 1024;

pub(in crate::api::model) struct BufferedStream {
    stream: ReceiverStream<Result<bytes::Bytes, StreamError>>,
    close_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    semaphore: Arc<Semaphore>,
    max_buffer_bytes: usize,
}

impl BufferedStream {
    pub fn new(
        stream: BoxedProviderStream,
        buffer_size: usize,
        max_buffer_bytes: usize,
        client_close_signal: CancellationToken,
        _url: &str,
    ) -> Self {
        let max_buffer_bytes = if max_buffer_bytes == 0 { MAX_BUFFER_BYTES } else { max_buffer_bytes };
        // Item-count limit remains as a secondary cap; byte-level backpressure
        // is enforced via `max_buffer_bytes` and `Semaphore`.
        let (tx, rx) = channel(max(buffer_size, CHANNEL_SIZE));
        let semaphore = Arc::new(Semaphore::new(max_buffer_bytes));
        tokio::spawn(Self::buffer_stream(
            tx,
            stream,
            client_close_signal.clone(),
            Arc::clone(&semaphore),
            max_buffer_bytes,
        ));
        Self {
            stream: ReceiverStream::new(rx),
            close_cancelled: Box::pin(client_close_signal.cancelled_owned()),
            semaphore,
            max_buffer_bytes,
        }
    }

    async fn buffer_stream(
        tx: Sender<Result<bytes::Bytes, StreamError>>,
        mut stream: BoxedProviderStream,
        client_close_signal: CancellationToken,
        semaphore: Arc<Semaphore>,
        max_buffer_bytes: usize,
    ) {
        let idle_timeout = Duration::from_secs(STREAM_IDLE_TIMEOUT);
        let idle = sleep(idle_timeout);
        tokio::pin!(idle);

        while !client_close_signal.is_cancelled() {
            select! {
                biased;
                () = client_close_signal.cancelled() => {
                    break;
                }
                () = &mut idle => {
                    debug!("Buffered stream idle for too long, closing");
                    client_close_signal.cancel();
                    break;
                }
                chunk = stream.next() => {
                    idle.as_mut().reset(Instant::now() + idle_timeout);
                    match chunk {
                        Some(Ok(chunk)) => {
                            let chunk_len = chunk.len();
                            // Cap permits at max_buffer_bytes per chunk.  A single chunk larger
                            // than the cap consumes fewer permits than its actual byte count, so
                            // the semaphore may temporarily allow more bytes in the channel than
                            // max_buffer_bytes.  This is an intentional trade-off: upstream
                            // providers are expected to emit chunks well below this limit; the
                            // inaccuracy is bounded to a single oversized chunk and is self-
                            // correcting once that chunk is delivered.
                            let permits = chunk_len.min(max_buffer_bytes);
                            if permits > 0 {
                                let acquired = select! {
                                    biased;
                                    () = client_close_signal.cancelled() => None,
                                    permit = Arc::clone(&semaphore).acquire_many_owned(u32::try_from(permits).unwrap_or(u32::MAX)) => permit.ok(),
                                };
                                let Some(permit) = acquired else {
                                    client_close_signal.cancel();
                                    break;
                                };
                                permit.forget();
                            }
                            let send_res = match tx.try_send(Ok(chunk)) {
                                Ok(()) => Ok(()),
                                Err(TrySendError::Full(item)) => {
                                    select! {
                                        biased;
                                        () = client_close_signal.cancelled() => Err(()),
                                        res = tx.send(item) => res.map_err(|_| ()),
                                    }
                                }
                                Err(TrySendError::Closed(_)) => Err(()),
                            };
                            if send_res.is_err() {
                                if permits > 0 {
                                    semaphore.add_permits(permits);
                                }
                                debug!("Buffered stream channel closed before delivering {chunk_len} bytes to client");
                                client_close_signal.cancel();
                                break;
                            }
                        }
                        Some(Err(err)) => {
                            let err_msg = err.to_string();
                            let send_err_res = match tx.try_send(Err(err)) {
                                Ok(()) => Ok(()),
                                Err(TrySendError::Full(item)) => {
                                    select! {
                                        biased;
                                        () = client_close_signal.cancelled() => Err(()),
                                        res = tx.send(item) => res.map_err(|_| ()),
                                    }
                                }
                                Err(TrySendError::Closed(_)) => Err(()),
                            };
                            if send_err_res.is_err() {
                                debug!("Buffered stream dropped stream error due to closed receiver: {err_msg}");
                                client_close_signal.cancel();
                            }
                            break;
                        }
                        None => {
                            debug!("Upstream provider completed buffered stream");
                            break;
                        }
                    }
                }
            }
        }
        if client_close_signal.is_cancelled() {
            debug!("Client close signal fired; buffered stream exiting");
        }
        drop(tx);
    }
}

impl Stream for BufferedStream {
    type Item = Result<bytes::Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.close_cancelled.as_mut().poll(cx).is_ready() {
            Poll::Ready(None)
        } else {
            match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.semaphore.add_permits(bytes.len().min(this.max_buffer_bytes));
                    Poll::Ready(Some(Ok(bytes)))
                }
                other => other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BufferedStream;
    use crate::api::model::StreamError;
    use bytes::Bytes;
    use futures::Stream;
    use std::{future::Future, pin::Pin, task::{Context, Poll}, time::Duration};
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    struct GatedDropProbeStream {
        gate: oneshot::Receiver<()>,
        dropped: Option<oneshot::Sender<()>>,
        yielded: bool,
    }

    impl Stream for GatedDropProbeStream {
        type Item = Result<Bytes, StreamError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.yielded {
                return Poll::Pending;
            }
            match Pin::new(&mut self.gate).poll(cx) {
                Poll::Ready(Ok(())) => {
                    self.yielded = true;
                    Poll::Ready(Some(Ok(Bytes::from_static(b"chunk"))))
                }
                Poll::Ready(Err(_)) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl Drop for GatedDropProbeStream {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[tokio::test]
    async fn closed_consumer_cancels_and_terminates_buffered_producer() {
        let (gate_tx, gate_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let cancel = CancellationToken::new();
        let upstream = GatedDropProbeStream {
            gate: gate_rx,
            dropped: Some(dropped_tx),
            yielded: false,
        };
        let buffered = BufferedStream::new(Box::pin(upstream), 1, 0, cancel.clone(), "test");

        drop(buffered);
        gate_tx.send(()).expect("producer should still own the gated upstream");

        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("producer should stop after sending to the closed consumer")
            .expect("upstream Drop probe should be delivered");
        assert!(cancel.is_cancelled(), "closed receiver must cancel the buffered producer token");
    }
}
