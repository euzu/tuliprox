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
}

impl BufferedStream {
    pub fn new(
        stream: BoxedProviderStream,
        buffer_size: usize,
        client_close_signal: CancellationToken,
        _url: &str,
    ) -> Self {
        // Item-count limit remains as a secondary cap; byte-level backpressure
        // is enforced via `MAX_BUFFER_BYTES` and `Semaphore`.
        let (tx, rx) = channel(max(buffer_size, CHANNEL_SIZE));
        let semaphore = Arc::new(Semaphore::new(MAX_BUFFER_BYTES));
        tokio::spawn(Self::buffer_stream(
            tx,
            stream,
            client_close_signal.clone(),
            Arc::clone(&semaphore),
        ));
        Self {
            stream: ReceiverStream::new(rx),
            close_cancelled: Box::pin(client_close_signal.cancelled_owned()),
            semaphore,
        }
    }

    async fn buffer_stream(
        tx: Sender<Result<bytes::Bytes, StreamError>>,
        mut stream: BoxedProviderStream,
        client_close_signal: CancellationToken,
        semaphore: Arc<Semaphore>,
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
                            let permits = chunk_len.min(MAX_BUFFER_BYTES);
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
                    this.semaphore.add_permits(bytes.len().min(MAX_BUFFER_BYTES));
                    Poll::Ready(Some(Ok(bytes)))
                }
                other => other,
            }
        }
    }
}
