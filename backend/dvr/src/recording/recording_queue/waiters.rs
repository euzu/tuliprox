use super::{RecordingControl, RecordingSlotWaitQueue, RecordingWaitRegistration, RecordingWaiter};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use tokio::sync::{Notify, RwLock};

pub(super) type RecordingWaiters = Arc<StdMutex<Vec<RecordingWaiter>>>;

impl Drop for RecordingWaitRegistration {
    fn drop(&mut self) { lock_unpoisoned(&self.waiters).retain(|waiter| waiter.id != self.id); }
}

#[derive(Clone)]
pub struct RecordingWaiterSnapshot {
    pub id: u64,
    pub input_name: Option<Arc<str>>,
    pub priority: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingWaitOutcome {
    Signalled,
    Paused,
    Cancelled,
    Restarted,
}

impl Default for RecordingSlotWaitQueue {
    fn default() -> Self { Self::new() }
}

impl RecordingSlotWaitQueue {
    pub fn new() -> Self {
        Self {
            waiters: Arc::new(StdMutex::new(Vec::new())),
            next_waiter_id: AtomicU64::new(1),
            registration_changed: Notify::new(),
        }
    }

    pub(super) fn remove_waiter(&self, waiter_id: u64) {
        lock_unpoisoned(&self.waiters).retain(|waiter| waiter.id != waiter_id);
    }

    /// Register and block until this task is signalled or control flow requests pause/cancel.
    pub async fn wait(
        &self,
        input_name: Option<Arc<str>>,
        priority: i8,
        control_signal: &RwLock<RecordingControl>,
        control_notify: &Notify,
    ) -> RecordingWaitOutcome {
        let waiter_id = self.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let notify = Arc::new(Notify::new());
        lock_unpoisoned(&self.waiters).push(RecordingWaiter {
            id: waiter_id,
            input_name,
            priority,
            notify: Arc::clone(&notify),
        });

        let _registration = RecordingWaitRegistration { waiters: Arc::clone(&self.waiters), id: waiter_id };
        self.registration_changed.notify_one();

        if let Some(outcome) = self.control_outcome(waiter_id, control_signal).await {
            return outcome;
        }

        loop {
            let controlled = control_notify.notified();
            tokio::pin!(controlled);
            controlled.as_mut().enable();
            if let Some(outcome) = self.control_outcome(waiter_id, control_signal).await {
                return outcome;
            }
            tokio::select! {
                () = notify.notified() => return RecordingWaitOutcome::Signalled,
                () = &mut controlled => {}
            }
        }
    }

    /// Map the current control signal to a wait outcome, deregistering the
    /// waiter when the wait ends.
    pub(super) async fn control_outcome(
        &self,
        waiter_id: u64,
        control_signal: &RwLock<RecordingControl>,
    ) -> Option<RecordingWaitOutcome> {
        let outcome = match *control_signal.read().await {
            RecordingControl::Pause => RecordingWaitOutcome::Paused,
            RecordingControl::Cancel => RecordingWaitOutcome::Cancelled,
            RecordingControl::Restart => RecordingWaitOutcome::Restarted,
            RecordingControl::None => return None,
        };
        self.remove_waiter(waiter_id);
        Some(outcome)
    }

    pub fn snapshots(&self) -> Vec<RecordingWaiterSnapshot> {
        lock_unpoisoned(&self.waiters)
            .iter()
            .map(|waiter| RecordingWaiterSnapshot {
                id: waiter.id,
                input_name: waiter.input_name.clone(),
                priority: waiter.priority,
            })
            .collect()
    }

    /// Wake a specific waiter by id.
    pub fn signal_waiter(&self, waiter_id: u64) -> bool {
        let mut waiters = lock_unpoisoned(&self.waiters);
        if let Some(idx) = waiters.iter().position(|waiter| waiter.id == waiter_id) {
            let notify = Arc::clone(&waiters[idx].notify);
            waiters.remove(idx);
            notify.notify_one();
            true
        } else {
            false
        }
    }
}

pub(super) fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
