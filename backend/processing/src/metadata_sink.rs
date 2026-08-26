//! What the playlist pipeline needs from the background metadata worker.
//!
//! Three operations out of a manager several thousand lines long: hold off
//! background work for the duration of an update, enqueue a detail-resolution
//! task, and skip an enqueue that is already pending.
//!
//! The manager itself is server-coupled - it reads provider allocation,
//! connection state, the playlist cache and both HTTP clients - so the pipeline
//! states the narrow contract it needs rather than depending on all of that.
//! The implementation lives beside the manager, in the binary.

use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::OwnedRwLockWriteGuard;
use tuliprox_core::model::UpdateTask;

/// A future returned across the trait boundary.
pub type SinkFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The background metadata worker, as the pipeline sees it.
pub trait MetadataUpdateSink: Send + Sync + 'static {
    /// Held for the lifetime of an update; background work waits on it.
    fn acquire_update_pause_guard(&self) -> SinkFuture<'_, OwnedRwLockWriteGuard<()>>;

    /// Enqueue a detail-resolution task for `input_name`.
    fn queue_task(&self, input_name: Arc<str>, task: UpdateTask) -> SinkFuture<'_, ()>;

    /// `true` when an equivalent task is already pending or ran recently enough
    /// that enqueuing again would be redundant.
    fn should_skip_enqueue<'a>(&'a self, input_name: Arc<str>, task: &'a UpdateTask) -> SinkFuture<'a, bool>;
}

/// Enqueue without waiting: spawns the enqueue and returns immediately.
///
/// The manager used to own this as `queue_task_background`; doing it here keeps
/// the trait to plain `&self` methods.
pub fn queue_task_background(sink: &Arc<dyn MetadataUpdateSink>, input_name: Arc<str>, task: UpdateTask) {
    let sink = Arc::clone(sink);
    tokio::spawn(async move {
        sink.queue_task(input_name, task).await;
    });
}
