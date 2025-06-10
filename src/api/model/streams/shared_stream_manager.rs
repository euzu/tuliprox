use crate::api::model::app_state::AppState;
use crate::api::model::streams::provider_stream_factory::STREAM_QUEUE_SIZE; // Assuming this constant is defined
use crate::api::model::stream_error::StreamError;
use crate::utils::debug_if_enabled;
use crate::utils::request::sanitize_sensitive_info;
use bytes::Bytes;
use futures::stream::Stream;
use futures::StreamExt; // For .next()
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use std::pin::Pin;
use std::task::{Context, Poll};
use log::trace;
use crate::api::model::stream::BoxedProviderStream; // Import our type alias

///
/// Wrap BroadcastStream as Stream<Item = Result<Bytes, StreamError>>.
/// This struct adapts the error type from `BroadcastStreamRecvError` to `StreamError`.
///
struct BroadcastStreamWrapper<S> {
    stream: S,
}

impl<S> Stream for BroadcastStreamWrapper<S>
where
    S: Stream<Item = Result<Bytes, BroadcastStreamRecvError>> + Unpin,
{
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(n)))) => {
                trace!("Subscriber lagged behind by {} messages", n);
                // Convert the BroadcastStreamRecvError::Lagged into a custom StreamError
                Poll::Ready(Some(Err(StreamError::Custom(format!("lagged {}", n)))))
            }
            Poll::Ready(None) => Poll::Ready(None), // Stream has ended
            Poll::Pending => Poll::Pending,         // Not ready yet
        }
    }
}

/// Converts a `tokio::sync::broadcast::Receiver<Bytes>` into a `BoxedProviderStream`.
/// This involves wrapping it in `BroadcastStream`, then in `BroadcastStreamWrapper`
/// to handle error conversion, and finally boxing and pinning it.
fn convert_stream(rx: broadcast::Receiver<Bytes>) -> BoxedProviderStream {
    let stream = BroadcastStream::new(rx);
    Box::pin(BroadcastStreamWrapper { stream })
}

/// State for a shared stream with a broadcast channel.
/// Each `SharedStreamState` instance manages one distinct shared stream.
struct SharedStreamState {
    headers: Vec<(String, String)>,
    broadcast_tx: broadcast::Sender<Bytes>,
}

impl SharedStreamState {
    /// Creates a new `SharedStreamState` with a given buffer size for the broadcast channel.
    fn new(headers: Vec<(String, String)>, buf_size: usize) -> Self {
        let (broadcast_tx, _) = broadcast::channel(buf_size);
        Self { headers, broadcast_tx }
    }

    /// Subscribes to the broadcast channel and returns a `BoxedProviderStream`
    /// for a new consumer.
    async fn subscribe(&self) -> BoxedProviderStream {
        let rx = self.broadcast_tx.subscribe();
        convert_stream(rx)
    }

    /// Starts broadcasting data from a source stream into the internal
    /// broadcast channel. This runs in a separate Tokio task.
    ///
    /// The task will automatically unregister the stream from the manager
    /// once the source stream is exhausted.
    fn broadcast<S, E>(&self, stream_url: &str, bytes_stream: S, shared_streams: Arc<SharedStreamManager>)
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
        E: std::fmt::Debug + Send + 'static, // Error type from the source stream
    {
        let mut source_stream = Box::pin(bytes_stream);
        let broadcast_tx = self.broadcast_tx.clone();
        let stream_url = stream_url.to_string(); // Clone for use in the async task

        tokio::spawn(async move {
            while let Some(item) = source_stream.next().await {
                if let Ok(data) = item {
                    // Send data to all subscribers.
                    // If no receivers, this will return an Err, which we ignore here.
                    let _ = broadcast_tx.send(data);
                }
                // TODO: Consider logging source stream errors (Err(E)) if `E` is meaningful.
            }
            // Once the source stream is exhausted, unregister it.
            debug_if_enabled!(
                "Shared stream exhausted. Closing shared provider stream {}",
                sanitize_sensitive_info(&stream_url)
            );
            shared_streams.unregister(&stream_url).await;
        });
    }
}

/// Type alias for the internal HashMap holding shared stream states, protected by an RwLock.
type SharedStreamRegister = RwLock<HashMap<String, SharedStreamState>>;

/// Manages a collection of shared streams, allowing multiple clients to subscribe
/// to the same data stream without fetching/processing it multiple times.
pub struct SharedStreamManager {
    shared_streams: SharedStreamRegister,
}

impl SharedStreamManager {
    /// Creates a new `SharedStreamManager`.
    pub(crate) fn new() -> Self {
        Self { shared_streams: RwLock::new(HashMap::new()) }
    }

    /// Retrieves the headers associated with a shared stream, if it exists.
    pub async fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        // Acquire a read lock, get the state, clone its headers, and release the lock.
        self.shared_streams.read().await.get(stream_url).map(|s| s.headers.clone())
    }

    /// Unregisters (removes) a shared stream from the manager.
    /// This is called automatically when a source stream exhausts.
    async fn unregister(&self, stream_url: &str) {
        let _ = self.shared_streams.write().await.remove(stream_url);
    }

    /// Subscribes to an existing shared stream. Returns `None` if the stream is not found.
    async fn subscribe_stream(&self, stream_url: &str) -> Option<BoxedProviderStream> {
        // Acquire a read lock, get the state, subscribe, and release the lock.
        // The `?` operator handles the `None` case.
        // `.into()` here is effectively a no-op if `BoxedProviderStream` is the target type.
        self.shared_streams.read().await.get(stream_url)?.subscribe().await.into()
    }

    /// Registers a new `SharedStreamState` with the manager.
    async fn register(&self, stream_url: &str, shared_state: SharedStreamState) {
        let _ = self.shared_streams.write().await.insert(stream_url.to_string(), shared_state);
    }

    /// Registers a new source stream to be shared and starts broadcasting its data.
    ///
    /// If a stream for the given `stream_url` already exists, this method would
    /// overwrite it, effectively restarting the broadcast for that URL.
    pub(crate) async fn subscribe<S, E>(
        app_state: &AppState, // Provides access to the shared_stream_manager instance
        stream_url: &str,
        bytes_stream: S, // The actual data source stream
        headers: Vec<(String, String)>,
        buffer_size: usize,
    )
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
        E: std::fmt::Debug + Send + 'static,
    {
        // Ensure the broadcast channel has at least `STREAM_QUEUE_SIZE` buffer.
        let buf_size = std::cmp::max(buffer_size, STREAM_QUEUE_SIZE);
        let shared_state = SharedStreamState::new(headers, buf_size);

        // Start the broadcasting task. It will register itself and clean up on exhaustion.
        shared_state.broadcast(stream_url, bytes_stream, Arc::clone(&app_state.shared_stream_manager));

        // Register the new shared state in the manager's map.
        app_state.shared_stream_manager.register(stream_url, shared_state).await;

        debug_if_enabled!("Created shared provider stream {}", sanitize_sensitive_info(stream_url));
    }

    /// Subscribes a client to an already existing shared stream.
    /// Returns `None` if the stream for `stream_url` does not exist.
    pub async fn subscribe_shared_stream(
        app_state: &AppState,
        stream_url: &str,
    ) -> Option<BoxedProviderStream> {
        debug_if_enabled!("Responding to existing shared client stream {}", sanitize_sensitive_info(stream_url));
        app_state.shared_stream_manager.subscribe_stream(stream_url).await
    }
}