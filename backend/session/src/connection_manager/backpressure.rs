use super::{
    socket::SocketActivityEvent, BackpressureSender, BackpressureState, SharedCleanupCapability, SocketActivityTracker,
};
use log::{debug, warn};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
};
use tokio::sync::{mpsc, Notify};
use tuliprox_core::model::SharedSubscriberId;

pub(super) fn notify_capacity(capacity_notify: &Notify) { capacity_notify.notify_waiters(); }

impl SharedCleanupCapability {
    pub(crate) fn new(subscriber_id: SharedSubscriberId) -> Self { Self { subscriber_id } }

    pub fn stream_uid(&self) -> u32 { self.subscriber_id.stream_uid() }

    pub fn subscriber_id(&self) -> SharedSubscriberId { self.subscriber_id }
}

impl<T> BackpressureSender<T>
where
    T: Send + 'static,
{
    pub(super) fn new(tx: mpsc::Sender<T>, queue_name: &'static str, overflow_capacity: usize) -> Self {
        Self {
            tx,
            state: Arc::new(Mutex::new(BackpressureState { overflow: VecDeque::new(), draining: false })),
            queue_name,
            overflow_capacity,
            dropped_events: AtomicU64::new(0),
        }
    }

    pub(super) fn enqueue(&self, event: T) {
        let runtime = tokio::runtime::Handle::try_current().ok();
        {
            let mut state = lock_backpressure_state(self.state.as_ref());
            if state.draining {
                if state.overflow.len() >= self.overflow_capacity {
                    let count = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                    if count == 1 || count.is_multiple_of(1024) {
                        warn!(
                            "{} overflow buffer full (capacity={}), {} events dropped total",
                            self.queue_name, self.overflow_capacity, count
                        );
                    }
                    return;
                }
                state.overflow.push_back(event);
                return;
            }

            match self.tx.try_send(event) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                    state.draining = true;
                    if state.overflow.len() >= self.overflow_capacity {
                        let count = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                        if count == 1 || count.is_multiple_of(1024) {
                            warn!(
                                "{} overflow buffer full (capacity={}), {} events dropped total",
                                self.queue_name, self.overflow_capacity, count
                            );
                        }
                        state.draining = false;
                        return;
                    }
                    state.overflow.push_back(event);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_event)) => {
                    debug!("{} channel closed, dropping event", self.queue_name);
                    return;
                }
            }
        }

        let tx = self.tx.clone();
        let state = Arc::clone(&self.state);
        let queue_name = self.queue_name;
        if let Some(handle) = runtime {
            handle.spawn(async move {
                Self::drain_async(&tx, &state, queue_name).await;
            });
        } else {
            thread::spawn(move || Self::drain_blocking(&tx, &state, queue_name));
        }
    }

    pub(super) async fn drain_async(
        tx: &mpsc::Sender<T>,
        state: &Arc<Mutex<BackpressureState<T>>>,
        queue_name: &'static str,
    ) {
        loop {
            let Some(event) = Self::next_event(state) else {
                break;
            };
            if tx.send(event).await.is_err() {
                debug!("{queue_name} channel closed while draining backpressure");
                Self::clear_and_stop(state);
                break;
            }
        }
    }

    pub(super) fn drain_blocking(
        tx: &mpsc::Sender<T>,
        state: &Arc<Mutex<BackpressureState<T>>>,
        queue_name: &'static str,
    ) {
        loop {
            let Some(event) = Self::next_event(state) else {
                break;
            };
            if tx.blocking_send(event).is_err() {
                warn!("{queue_name} channel closed while draining backpressure");
                Self::clear_and_stop(state);
                break;
            }
        }
    }

    pub(super) fn next_event(state: &Arc<Mutex<BackpressureState<T>>>) -> Option<T> {
        let mut state = lock_backpressure_state(state.as_ref());
        if let Some(event) = state.overflow.pop_front() {
            return Some(event);
        }
        state.draining = false;
        None
    }

    pub(super) fn clear_and_stop(state: &Arc<Mutex<BackpressureState<T>>>) {
        let mut state = lock_backpressure_state(state.as_ref());
        state.overflow.clear();
        state.draining = false;
    }

    pub(super) fn dropped_count(&self) -> u64 { self.dropped_events.load(Ordering::Relaxed) }
}

pub(super) fn lock_backpressure_state<T>(state: &Mutex<BackpressureState<T>>) -> MutexGuard<'_, BackpressureState<T>> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("Backpressure queue state was poisoned, continuing with recovered state");
            poisoned.into_inner()
        }
    }
}

impl SocketActivityTracker {
    pub(super) fn new() -> Self {
        Self { pending: Arc::new(Mutex::new(HashSet::new())), notify: Arc::new(Notify::new()) }
    }

    pub(super) fn track(&self, event: SocketActivityEvent) {
        let mut pending = lock_socket_activity_pending(self.pending.as_ref());
        pending.insert(event);
        drop(pending);
        self.notify.notify_one();
    }

    pub(super) fn drain(&self) -> Vec<SocketActivityEvent> {
        let mut pending = lock_socket_activity_pending(self.pending.as_ref());
        pending.drain().collect()
    }

    pub(super) async fn notified(&self) { self.notify.notified().await; }
}

fn lock_socket_activity_pending(
    pending: &Mutex<HashSet<SocketActivityEvent>>,
) -> MutexGuard<'_, HashSet<SocketActivityEvent>> {
    pending.lock().unwrap_or_else(|poisoned| {
        warn!("Socket activity state was poisoned, continuing with recovered state");
        poisoned.into_inner()
    })
}
