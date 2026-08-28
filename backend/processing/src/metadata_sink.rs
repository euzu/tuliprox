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

use std::{future::Future, sync::Arc};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};
use tuliprox_core::model::UpdateTask;

/// The background metadata worker, as the pipeline sees it.
///
/// The two async methods used to return `Pin<Box<dyn Future + Send>>` and the
/// pipeline held the whole thing as `Arc<dyn MetadataUpdateSink>`: a heap
/// allocation per call and a vtable hop on `should_skip_enqueue`, which runs
/// once per playlist item. Nothing ever needed the erasure - there is one
/// implementor - so the futures are returned by value and the pipeline carries
/// the concrete type.
pub trait MetadataUpdateSink: Send + Sync + 'static {
    /// Held for the lifetime of an update; background work waits on it.
    fn acquire_update_pause_guard(&self) -> impl Future<Output = OwnedRwLockWriteGuard<()>> + Send;

    /// Load the per-input state that `should_skip_enqueue` reads.
    ///
    /// Idempotent, but the first call for an input reads from disk. Call it
    /// once, lazily, before a run of `should_skip_enqueue` checks for the same
    /// input - not eagerly, or inputs with nothing to enqueue pay for a load
    /// they never use.
    fn prepare_enqueue_state(&self, input_name: Arc<str>) -> impl Future<Output = ()> + Send;

    /// `true` when an equivalent task is already pending or ran recently enough
    /// that enqueuing again would be redundant.
    ///
    /// Reads only state `prepare_enqueue_state` has already loaded, so it is
    /// synchronous: the pipeline calls it once per playlist item.
    fn should_skip_enqueue(&self, input_name: &str, task: &UpdateTask) -> bool;

    /// Enqueue a detail-resolution task without waiting for it to be accepted.
    fn queue_task_background(self: Arc<Self>, input_name: Arc<str>, task: UpdateTask);
}

/// The sink type parameter of a run that has no metadata worker.
///
/// A run without one holds `None`, so none of these methods is ever called -
/// the type exists only to name the parameter, the same role `NoopSink` plays
/// for events. `unreachable!` would be a landmine in a type whose whole purpose
/// is to be absent, so each method is a correct no-op instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopMetadataSink;

impl MetadataUpdateSink for NoopMetadataSink {
    async fn acquire_update_pause_guard(&self) -> OwnedRwLockWriteGuard<()> {
        // A guard over a lock nobody else holds: acquired immediately, and
        // pausing nothing is the correct behaviour with no worker to pause.
        Arc::new(RwLock::new(())).write_owned().await
    }

    async fn prepare_enqueue_state(&self, _input_name: Arc<str>) {}

    fn should_skip_enqueue(&self, _input_name: &str, _task: &UpdateTask) -> bool {
        true
    }

    fn queue_task_background(self: Arc<Self>, _input_name: Arc<str>, _task: UpdateTask) {}
}
