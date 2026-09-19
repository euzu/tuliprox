use crate::BoxedProviderStream;
use bytes::Bytes;
use futures::{
    stream::Stream,
    task::{Context, Poll},
    StreamExt,
};
use log::debug;
use std::{
    cmp::max,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    select,
    sync::{
        mpsc::{channel, error::TrySendError, Sender},
        Semaphore,
    },
    time::{sleep, Instant, Sleep},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use tuliprox_core::model::{ProviderCloseReason, StreamError};

pub const DEFAULT_DIRECT_BODY_CHANNEL_SIZE: usize = 2;
pub const DEFAULT_DIRECT_BODY_MAX_BYTES: usize = 256 * 1024;
pub const DEFAULT_DIRECT_BODY_IDLE_SECS: u64 = 90;

pub const DEFAULT_BUFFERED_CHANNEL_SIZE: usize = 1024;
pub const DEFAULT_BUFFERED_MAX_BYTES: usize = 5 * 1024 * 1024;
pub const DEFAULT_BUFFERED_IDLE_SECS: u64 = 10;

#[derive(Debug, Clone, Copy)]
pub struct ProviderBodyOwnerConfig {
    pub channel_capacity: usize,
    pub max_buffer_bytes: usize,
    pub idle_timeout: Duration,
}

impl ProviderBodyOwnerConfig {
    pub fn for_direct_body() -> Self {
        Self {
            channel_capacity: DEFAULT_DIRECT_BODY_CHANNEL_SIZE,
            max_buffer_bytes: DEFAULT_DIRECT_BODY_MAX_BYTES,
            idle_timeout: Duration::from_secs(DEFAULT_DIRECT_BODY_IDLE_SECS),
        }
    }

    pub fn for_buffered(buffer_size: usize, max_buffer_bytes: usize) -> Self {
        Self {
            channel_capacity: max(buffer_size, DEFAULT_BUFFERED_CHANNEL_SIZE),
            max_buffer_bytes: if max_buffer_bytes == 0 { DEFAULT_BUFFERED_MAX_BYTES } else { max_buffer_bytes },
            idle_timeout: Duration::from_secs(DEFAULT_BUFFERED_IDLE_SECS),
        }
    }
}

/// An upstream body owner task that decouples upstream body consumption from downstream polling.
/// It observes cancellation independently of downstream `poll_next`, ensuring that upstream
/// connections close promptly upon preemption or client disconnect.
pub struct ProviderBodyOwner {
    stream: ReceiverStream<Result<Bytes, StreamError>>,
    close_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    cancel_token: CancellationToken,
    semaphore: Arc<Semaphore>,
    max_buffer_bytes: usize,
}

impl ProviderBodyOwner {
    pub fn new(
        stream: BoxedProviderStream,
        config: ProviderBodyOwnerConfig,
        cancel_token: CancellationToken,
        completion_token: Option<CancellationToken>,
        close_reason: Option<Arc<AtomicU8>>,
    ) -> Self {
        let (tx, rx) = channel(config.channel_capacity);
        let semaphore = Arc::new(Semaphore::new(config.max_buffer_bytes));

        tokio::spawn(Self::run_owner_task(
            tx,
            stream,
            cancel_token.clone(),
            completion_token,
            close_reason,
            Arc::clone(&semaphore),
            config,
        ));

        Self {
            stream: ReceiverStream::new(rx),
            close_cancelled: Box::pin(cancel_token.clone().cancelled_owned()),
            cancel_token,
            semaphore,
            max_buffer_bytes: config.max_buffer_bytes,
        }
    }

    async fn run_owner_task(
        tx: Sender<Result<Bytes, StreamError>>,
        mut stream: BoxedProviderStream,
        cancel_token: CancellationToken,
        completion_token: Option<CancellationToken>,
        close_reason: Option<Arc<AtomicU8>>,
        semaphore: Arc<Semaphore>,
        config: ProviderBodyOwnerConfig,
    ) {
        let idle_timeout = config.idle_timeout;
        let idle = sleep(idle_timeout);
        tokio::pin!(idle);

        while !cancel_token.is_cancelled() {
            select! {
                biased;
                () = cancel_token.cancelled() => {
                    debug!("Provider body owner task observed cancellation signal");
                    break;
                }
                () = &mut idle => {
                    debug!("Provider body owner task idle timeout expired ({idle_timeout:?})");
                    if let Some(reason) = &close_reason {
                        let _ = reason.compare_exchange(
                            ProviderCloseReason::Unspecified as u8,
                            ProviderCloseReason::IdleTimeout as u8,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                    }
                    cancel_token.cancel();
                    break;
                }
                chunk = stream.next() => {
                    idle.as_mut().reset(Instant::now() + idle_timeout);
                    match chunk {
                        Some(Ok(bytes)) => {
                            if !Self::forward_chunk(
                                &tx,
                                &cancel_token,
                                &mut idle,
                                &semaphore,
                                close_reason.as_ref(),
                                config.max_buffer_bytes,
                                bytes,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        Some(Err(err)) => {
                            if let Some(reason) = &close_reason {
                                let _ = reason.compare_exchange(
                                    ProviderCloseReason::Unspecified as u8,
                                    ProviderCloseReason::ProviderError as u8,
                                    Ordering::AcqRel,
                                    Ordering::Relaxed,
                                );
                            }
                            select! {
                                biased;
                                () = cancel_token.cancelled() => {},
                                _ = tx.send(Err(err)) => {},
                            }
                            break;
                        }
                        None => {
                            debug!("Provider body owner task completed cleanly on EOF");
                            break;
                        }
                    }
                }
            }
        }

        // Explicitly drop stream before signalling completion, releasing reqwest/hyper socket
        drop(stream);
        drop(tx);

        if let Some(completion) = completion_token {
            completion.cancel();
        }
    }

    /// Forwards one upstream chunk into the channel.
    ///
    /// The chunk is split into pieces no larger than `max_buffer_bytes`, and each
    /// piece acquires its own semaphore permits before it is queued. This keeps the
    /// value held in the channel within the configured accounting budget instead of
    /// enqueueing an unbounded `Bytes` that only reserved a capped amount.
    ///
    /// Returns `false` when the owner task must terminate: cancellation, an idle
    /// timeout while blocked, or a closed client channel. The close reason and the
    /// cancellation token are updated in that case.
    async fn forward_chunk(
        tx: &Sender<Result<Bytes, StreamError>>,
        cancel_token: &CancellationToken,
        idle: &mut Pin<&mut Sleep>,
        semaphore: &Arc<Semaphore>,
        close_reason: Option<&Arc<AtomicU8>>,
        max_buffer_bytes: usize,
        bytes: Bytes,
    ) -> bool {
        let piece_size = max_buffer_bytes.max(1);
        let mut offset = 0;
        while offset < bytes.len() {
            let end = (offset + piece_size).min(bytes.len());
            let piece = bytes.slice(offset..end);
            let permits = piece.len();
            if permits > 0 {
                let acquired = select! {
                    biased;
                    () = cancel_token.cancelled() => None,
                    () = idle.as_mut() => {
                        Self::set_close_reason(close_reason, ProviderCloseReason::IdleTimeout);
                        cancel_token.cancel();
                        return false;
                    }
                    permit = Arc::clone(semaphore).acquire_many_owned(u32::try_from(permits).unwrap_or(u32::MAX)) => permit.ok(),
                };
                let Some(permit) = acquired else {
                    cancel_token.cancel();
                    return false;
                };
                permit.forget();
            }
            let send_res = match tx.try_send(Ok(piece)) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(item)) => {
                    select! {
                        biased;
                        () = cancel_token.cancelled() => Err(()),
                        () = idle.as_mut() => {
                            Self::set_close_reason(close_reason, ProviderCloseReason::IdleTimeout);
                            cancel_token.cancel();
                            return false;
                        }
                        res = tx.send(item) => res.map_err(|_| ()),
                    }
                }
                Err(TrySendError::Closed(_)) => Err(()),
            };
            if send_res.is_err() {
                if permits > 0 {
                    semaphore.add_permits(permits);
                }
                Self::set_close_reason(close_reason, ProviderCloseReason::ClientClosed);
                cancel_token.cancel();
                return false;
            }
            offset = end;
        }
        true
    }

    fn set_close_reason(close_reason: Option<&Arc<AtomicU8>>, reason: ProviderCloseReason) {
        if let Some(slot) = close_reason {
            let _ = slot.compare_exchange(
                ProviderCloseReason::Unspecified as u8,
                reason as u8,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

impl Stream for ProviderBodyOwner {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.stream).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.semaphore.add_permits(bytes.len().min(this.max_buffer_bytes));
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if this.close_cancelled.as_mut().poll(cx).is_ready() {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl Drop for ProviderBodyOwner {
    fn drop(&mut self) { self.cancel_token.cancel(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{Stream, StreamExt};
    use tokio::sync::oneshot;

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
    async fn unpolled_body_owner_terminates_upstream_when_cancelled() {
        let (gate_tx, gate_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let cancel = CancellationToken::new();
        let completion = CancellationToken::new();
        let close_reason = Arc::new(AtomicU8::new(0));

        let upstream = GatedDropProbeStream { gate: gate_rx, dropped: Some(dropped_tx), yielded: false };
        let owner = ProviderBodyOwner::new(
            Box::pin(upstream),
            ProviderBodyOwnerConfig::for_direct_body(),
            cancel.clone(),
            Some(completion.clone()),
            Some(close_reason.clone()),
        );

        // Cancel the owner WITHOUT EVER POLLING IT
        cancel.cancel();

        // Completion must fire
        tokio::time::timeout(Duration::from_secs(1), completion.cancelled())
            .await
            .expect("completion token must be signalled when owner task terminates");

        // Upstream stream must have been dropped
        gate_tx.send(()).unwrap_err(); // gate_rx is dropped when upstream is dropped!

        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("upstream stream must be dropped without downstream polling")
            .expect("drop notification delivered");

        drop(owner);
    }

    struct TwoChunksThenErrStream {
        count: usize,
        dropped: Option<oneshot::Sender<()>>,
    }
    impl Stream for TwoChunksThenErrStream {
        type Item = Result<Bytes, StreamError>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.count.cmp(&2) {
                std::cmp::Ordering::Less => {
                    self.count += 1;
                    Poll::Ready(Some(Ok(Bytes::from_static(b"chunk"))))
                }
                std::cmp::Ordering::Equal => {
                    self.count += 1;
                    Poll::Ready(Some(Err(StreamError::Stream("test error".to_owned()))))
                }
                std::cmp::Ordering::Greater => Poll::Pending,
            }
        }
    }
    impl Drop for TwoChunksThenErrStream {
        fn drop(&mut self) {
            if let Some(d) = self.dropped.take() {
                let _ = d.send(());
            }
        }
    }

    #[tokio::test]
    async fn error_delivery_does_not_block_cancellation_when_channel_full() {
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let cancel = CancellationToken::new();
        let completion = CancellationToken::new();
        let close_reason = Arc::new(AtomicU8::new(0));

        let upstream = TwoChunksThenErrStream { count: 0, dropped: Some(dropped_tx) };
        let owner = ProviderBodyOwner::new(
            Box::pin(upstream),
            ProviderBodyOwnerConfig::for_direct_body(),
            cancel.clone(),
            Some(completion.clone()),
            Some(close_reason.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_millis(500), completion.cancelled())
            .await
            .expect("completion token must be signalled when owner task terminates on error delivery");

        tokio::time::timeout(Duration::from_millis(500), dropped_rx)
            .await
            .expect("upstream stream must be dropped")
            .expect("drop notification delivered");

        drop(owner);
    }

    struct FixedChunksStream {
        remaining: Vec<Bytes>,
    }
    impl Stream for FixedChunksStream {
        type Item = Result<Bytes, StreamError>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.remaining.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Some(Ok(self.remaining.remove(0))))
            }
        }
    }

    #[tokio::test]
    async fn oversized_upstream_chunk_is_forwarded_in_bounded_pieces() {
        let max = DEFAULT_DIRECT_BODY_MAX_BYTES;
        let upstream = FixedChunksStream { remaining: vec![Bytes::from(vec![7u8; max + 10])] };
        let mut owner = ProviderBodyOwner::new(
            Box::pin(upstream),
            ProviderBodyOwnerConfig {
                channel_capacity: 2,
                max_buffer_bytes: max,
                idle_timeout: Duration::from_secs(60),
            },
            CancellationToken::new(),
            None,
            None,
        );

        let mut total = 0usize;
        while total < max + 10 {
            let piece = owner.next().await.expect("stream still open").expect("no stream error");
            assert!(piece.len() <= max, "forwarded piece must respect max_buffer_bytes");
            total += piece.len();
        }
        assert_eq!(total, max + 10);
    }

    struct EndlessChunkStream {
        yielded: usize,
    }
    impl Stream for EndlessChunkStream {
        type Item = Result<Bytes, StreamError>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.yielded >= 4 {
                Poll::Pending
            } else {
                self.yielded += 1;
                Poll::Ready(Some(Ok(Bytes::from_static(b"x"))))
            }
        }
    }

    #[tokio::test]
    async fn idle_timeout_terminates_owner_blocked_on_full_channel() {
        let cancel = CancellationToken::new();
        let completion = CancellationToken::new();
        let close_reason = Arc::new(AtomicU8::new(0));
        let owner = ProviderBodyOwner::new(
            Box::pin(EndlessChunkStream { yielded: 0 }),
            ProviderBodyOwnerConfig {
                channel_capacity: 1,
                max_buffer_bytes: 1024,
                idle_timeout: Duration::from_millis(50),
            },
            cancel.clone(),
            Some(completion.clone()),
            Some(close_reason.clone()),
        );

        // Never poll `owner`: the channel stays full and the owner blocks in its send path.
        tokio::time::timeout(Duration::from_secs(2), completion.cancelled())
            .await
            .expect("owner must terminate after the idle timeout");
        assert_eq!(close_reason.load(Ordering::Acquire), ProviderCloseReason::IdleTimeout as u8);

        drop(owner);
    }
}
