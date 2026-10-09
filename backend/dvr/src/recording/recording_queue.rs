//! The persisted recording queue.
//!
//! One queue holds every recording task — scheduled Live captures and
//! immediate VOD/Series transfers alike. Three deliberately separate shapes
//! exist:
//!
//! * [`RecordingTask`] — the internal, mutable, in-memory task. Carries the
//!   resolved source URL, owner, path state and transfer progress.
//! * [`PersistedRecordingTask`] — the on-disk shape. Never serialized to a
//!   client.
//! * [`shared::model::RecordingTaskDto`] — the owner-safe public projection,
//!   produced only through [`RecordingTask::to_owner_view`].

pub use shared::model::RecordingTaskState;
use std::{
    collections::{HashMap, VecDeque},
    sync::{atomic::AtomicU64, Arc, Mutex as StdMutex},
};
use tokio::sync::{Mutex, Notify, RwLock};
use tuliprox_repository::recording_repository::RecordingRepository;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use tuliprox_repository::recording_repository::{
    IdempotencyOutcome, PersistedIdempotency, PersistedRecordingTask, RecordingPartition,
};

fn poisoned_repository() -> std::io::Error {
    std::io::Error::other("recording repository lock was poisoned by a panicking writer")
}

const RECORDING_WINDOW_EXPIRED_ERR: &str = "Recording window already expired";

const RECORDING_INTERRUPTED_ERR: &str = "Recording was interrupted by a restart and cannot be resumed";

static RECORDING_TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// How many `_N` names are tried for a file already on disk before the
/// request is refused as having no usable path.
const MAX_COLLISION_PROBES: usize = 1000;

/// Priority-aware wait queue for provider connection slots.
/// When the provider is at capacity, tasks register here and are
/// woken one-at-a-time in descending priority order (lowest i8 = highest priority).
struct RecordingWaiter {
    id: u64,
    input_name: Option<Arc<str>>,
    priority: i8,
    notify: Arc<Notify>,
}

struct RecordingWaitRegistration {
    waiters: RecordingWaiters,
    id: u64,
}

pub struct RecordingSlotWaitQueue {
    waiters: RecordingWaiters,
    next_waiter_id: AtomicU64,
    pub registration_changed: Notify,
}

pub struct RecordingQueue {
    pub queue: Arc<Mutex<VecDeque<RecordingTask>>>,
    pub scheduled: Arc<RwLock<Vec<RecordingTask>>>,
    pub active: Arc<RwLock<Vec<RecordingTask>>>,
    pub finished: Arc<RwLock<Vec<RecordingTask>>>,
    workers: StdMutex<HashMap<String, Arc<RecordingWorkerState>>>,
    // Inputs can share providers, so policy inspection and allocation use one
    // guard. Transfer I/O runs outside it.
    pub(crate) capacity_guard: Mutex<()>,
    pub(crate) queue_changed: Notify,
    /// The recoverable store. `None` for an in-memory queue in tests.
    pub repository: Option<Arc<StdMutex<RecordingRepository>>>,
    /// Priority-aware waiter queue for provider connection slots.
    pub slot_waiters: Arc<RecordingSlotWaitQueue>,
    /// In-memory mirror of the persisted queue revision. Incremented
    /// once per committed mutation.
    pub revision: Arc<AtomicU64>,
    mutation_guard: Arc<Mutex<()>>,
}

impl Default for RecordingQueue {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests;

mod control;
mod error;
mod mutation;
mod persistence;
mod promotion;
mod task;
mod waiters;
mod workers;
pub use control::RecordingControl;
pub use error::{PersistedError, QueueMutationError};
pub use mutation::{mutate, mutate_optional, mutate_prepared, mutate_then, mutate_with_idempotency};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use promotion::{
    attach_to_completed, media_held_by_another, media_is_still_referenced, promote_from_queue, promotion_decision,
    PromotionDecision,
};
pub use task::{PersistedRecordingQueue, RecordingTask};
use waiters::RecordingWaiters;
pub use waiters::{RecordingWaitOutcome, RecordingWaiterSnapshot};
pub use workers::RecordingWorkerState;
