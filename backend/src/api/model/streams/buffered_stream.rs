use crate::api::model::{BoxedProviderStream, StreamError, STREAM_IDLE_TIMEOUT};
use crate::tools::atomic_once_flag::AtomicOnceFlag;
use futures::{stream::Stream, task::{Context, Poll}, StreamExt};
use log::{debug};
use std::{
    cmp::max,
    pin::Pin,
    sync::Arc,
};
use tokio::select;
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tokio::time::{sleep, Duration, Instant};

pub const CHANNEL_SIZE: usize = 1024;

pub(in crate::api::model) struct BufferedStream {
    stream: ReceiverStream<Result<bytes::Bytes, StreamError>>,
    close_signal: Arc<AtomicOnceFlag>,
}

impl BufferedStream {
    pub fn new(stream: BoxedProviderStream, buffer_size: usize, client_close_signal: Arc<AtomicOnceFlag>, _url: &str) -> Self {
        // TODO make channel_size  based on bytes not entries
        let (tx, rx) = channel(max(buffer_size, CHANNEL_SIZE));
        tokio::spawn(Self::buffer_stream(tx, stream, Arc::clone(&client_close_signal)));
        Self {
            stream: ReceiverStream::new(rx),
            close_signal: client_close_signal,
        }
    }

    async fn buffer_stream(
        tx: Sender<Result<bytes::Bytes, StreamError>>,
        mut stream: BoxedProviderStream,
        client_close_signal: Arc<AtomicOnceFlag>,
    ) {
        let idle_timeout = Duration::from_secs(STREAM_IDLE_TIMEOUT);
        let idle = sleep(idle_timeout);
        tokio::pin!(idle);

        while client_close_signal.is_active() {
            select! {
                () = &mut idle => {
                    debug!("Buffered stream idle for too long, closing");
                    client_close_signal.notify();
                    break;
                }
                chunk = stream.next() => {
                    idle.as_mut().reset(Instant::now() + idle_timeout);
                    match chunk {
                        Some(Ok(chunk)) => {
                                let chunk_len = chunk.len();
                                if tx.send(Ok(chunk)).await.is_err() {
                                    debug!("Buffered stream channel closed before delivering {chunk_len} bytes to client");
                                    client_close_signal.notify();
                                    break;
                                }
                            }
                            Some(Err(err)) => {
                                let err_msg = err.to_string();
                                if tx.send(Err(err)).await.is_err() {
                                    debug!("Buffered stream dropped stream error due to closed receiver: {err_msg}");
                                    client_close_signal.notify();
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
        if !client_close_signal.is_active() {
            debug!("Client close signal fired; buffered stream exiting");
        }
        drop(tx);
    }
}

impl Stream for BufferedStream {
    type Item = Result<bytes::Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.close_signal.is_active() {
            Pin::new(&mut self.get_mut().stream).poll_next(cx)
        } else {
            Poll::Ready(None)
        }
    }
}
