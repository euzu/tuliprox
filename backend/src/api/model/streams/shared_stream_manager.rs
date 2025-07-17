use crate::api::model::app_state::AppState;
use crate::api::model::stream_error::StreamError;
use crate::api::model::streams::provider_stream_factory::STREAM_QUEUE_SIZE;
use crate::utils::debug_if_enabled;
use bytes::Bytes;
use futures::stream::{BoxStream, FuturesUnordered};
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;

use crate::api::model::stream::BoxedProviderStream;
use dashmap::DashMap;
use log::trace;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use shared::utils::sanitize_sensitive_info;

///
/// Wraps a `ReceiverStream` as Stream<Item = Result<Bytes, `StreamError`>>
///
struct ReceiverStreamWrapper<S> {
    stream: S,
}

impl<S> Stream for ReceiverStreamWrapper<S>
where
    S: Stream<Item=Bytes> + Unpin,
{
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn convert_stream(stream: BoxStream<Bytes>) -> BoxStream<Result<Bytes, StreamError>> {
    ReceiverStreamWrapper { stream }.boxed()
}

type SubscriberId = usize;

/// Represents the state of a shared provider URL.
///
/// - `headers`: The initial connection headers used during the setup of the shared stream.
struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    subscribers: Arc<DashMap<SubscriberId, Sender<Bytes>>>, //Arc<RwLock<Vec<Sender<Bytes>>>>,
    next_subscriber_id: AtomicUsize,
}

impl SharedStreamState {
    fn new(headers: Vec<(String, String)>,
           buf_size: usize) -> Self {
        Self {
            headers,
            buf_size,
            subscribers: Arc::new(DashMap::new()), //Arc::new(RwLock::new(Vec::new())),
            next_subscriber_id: AtomicUsize::new(1),
        }
    }

    fn subscribe(&self) -> BoxedProviderStream {
        let (tx, rx) = mpsc::channel(self.buf_size);
        let id = self.next_subscriber_id.fetch_add(1, Ordering::AcqRel);
        self.subscribers.insert(id, tx);
        convert_stream(ReceiverStream::new(rx).boxed())
    }

    fn broadcast<S, E>(
        &self,
        stream_url: &str,
        bytes_stream: S,
        shared_streams: Arc<SharedStreamManager>,
    )
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let mut source_stream = Box::pin(bytes_stream);
        let subscribers = self.subscribers.clone();
        let streaming_url = stream_url.to_string();

        tokio::spawn(async move {
            while let Some(item) = source_stream.next().await {
                let Ok(data) = item else {
                    trace!("Shared stream received Err, skipping...");
                    continue;
                };

                if subscribers.is_empty() {
                    debug_if_enabled!("No active subscribers. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
                    shared_streams.unregister(&streaming_url).await;
                    break;
                }

                // Fast-path: at least one has capacity
                if subscribers.iter().any(|sender| sender.value().capacity() > 0) {
                    // Try sending immediately
                    subscribers.retain(|_id, sender| match sender.try_send(data.clone()) {
                        Ok(()) => true,
                        Err(TrySendError::Closed(_)) => false,
                        Err(err) => {
                            trace!("broadcast try_send error: {err:?}");
                            true
                        }
                    });
                    continue;
                }

                // All are full → wait until one becomes available
                let mut futures = FuturesUnordered::new();
                for sender in subscribers.iter() {
                    let tx = sender.value().clone();
                    futures.push(async move {
                        match tx.reserve().await {
                            Ok(permit) => {
                                drop(permit);
                                Ok(())
                            }
                            Err(_) => Err(()),
                        }
                    });
                }

                let mut has_fillable_subscriber = false;
                while let Some(result) = futures.next().await {
                    if let Ok(()) = result {
                        has_fillable_subscriber = true;
                        break;
                    }
                }

                if !has_fillable_subscriber {
                    debug_if_enabled!("All subscribers closed. Shutting down shared provider stream {}",sanitize_sensitive_info(&streaming_url));
                    shared_streams.unregister(&streaming_url).await;
                    break;
                }

                // Re-validate subscribers and send again
                subscribers.retain(|_id, sender| match sender.try_send(data.clone()) {
                    Ok(()) => true,
                    Err(TrySendError::Closed(_)) => false,
                    Err(err) => {
                        trace!("broadcast try_send error after reserve: {err:?}");
                        true
                    }
                });
            }

            debug_if_enabled!("Shared stream exhausted. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
            shared_streams.unregister(&streaming_url).await;
        });
    }
}

type SharedStreamRegister = RwLock<HashMap<String, SharedStreamState>>;

pub struct SharedStreamManager {
    shared_streams: SharedStreamRegister,
}

impl SharedStreamManager {
    pub(crate) fn new() -> Self {
        Self {
            shared_streams: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        self.shared_streams.read().await.get(stream_url).map(|s| s.headers.clone())
    }

    async fn unregister(&self, stream_url: &str) {
        let _ = self.shared_streams.write().await.remove(stream_url);
    }

    async fn subscribe_stream(&self, stream_url: &str) -> Option<BoxedProviderStream> {
        let stream_data = self.shared_streams.read().await.get(stream_url)?.subscribe();
        Some(stream_data)
    }

    async fn register(&self, stream_url: &str, shared_state: SharedStreamState) {
        let _ = self.shared_streams.write().await.insert(stream_url.to_string(), shared_state);
    }

    pub(crate) async fn subscribe<S, E>(
        app_state: &AppState,
        stream_url: &str,
        bytes_stream: S,
        headers: Vec<(String, String)>,
        buffer_size: usize, ) -> Option<BoxedProviderStream>
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + std::marker::Send,
        E: std::fmt::Debug + std::marker::Send,
    {
        let buf_size = std::cmp::max(buffer_size, STREAM_QUEUE_SIZE);
        let shared_state = SharedStreamState::new(headers, buf_size);
        shared_state.broadcast(stream_url, bytes_stream, Arc::clone(&app_state.shared_stream_manager));
        app_state.shared_stream_manager.register(stream_url, shared_state).await;
        debug_if_enabled!("Created shared provider stream {}", sanitize_sensitive_info(stream_url));
        Self::subscribe_shared_stream(app_state, stream_url).await
    }

    /// Creates a broadcast notify stream for the given URL if a shared stream exists.
    pub async fn subscribe_shared_stream(
        app_state: &AppState,
        stream_url: &str,
    ) -> Option<BoxedProviderStream> {
        debug_if_enabled!("Responding existing shared client stream {}", sanitize_sensitive_info(stream_url));
        app_state.shared_stream_manager.subscribe_stream(stream_url).await
    }
}