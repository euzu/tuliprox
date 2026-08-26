//! What the playlist pipeline needs from the background metadata worker.
//!
//! Four operations out of a manager several thousand lines long: hold off
//! background work for the duration of an update, load the per-input enqueue
//! state, skip an enqueue that is already pending, and enqueue a
//! detail-resolution task.
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

    /// Load the per-input state that `should_skip_enqueue` reads.
    ///
    /// Idempotent, but the first call for an input reads from disk. Call it
    /// once, lazily, before a run of `should_skip_enqueue` checks for the same
    /// input - not eagerly, or inputs with nothing to enqueue pay for a load
    /// they never use.
    fn prepare_enqueue_state(&self, input_name: Arc<str>) -> SinkFuture<'_, ()>;

    /// `true` when an equivalent task is already pending or ran recently enough
    /// that enqueuing again would be redundant.
    ///
    /// Reads only state `prepare_enqueue_state` has already loaded, so it is
    /// synchronous: the pipeline calls it once per playlist item.
    fn should_skip_enqueue(&self, input_name: &str, task: &UpdateTask) -> bool;

    /// Enqueue a detail-resolution task without waiting for it to be accepted.
    fn queue_task_background(self: Arc<Self>, input_name: Arc<str>, task: UpdateTask);
}
