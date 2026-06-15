use super::{HlsAccessLeaseId, ProxySessionId};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    time::Duration,
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum HlsLifecycleEventKey {
    AccessLeaseActive { lease_id: HlsAccessLeaseId, proxy_session_id: ProxySessionId },
    AccessLeaseValidity { lease_id: HlsAccessLeaseId, proxy_session_id: ProxySessionId },
    SessionIdle { proxy_session_id: ProxySessionId },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsLifecycleEvent {
    pub key: HlsLifecycleEventKey,
    pub due_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct ScheduledInfo {
    due_at_ms: u64,
    sequence: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct QueueEntry {
    key: HlsLifecycleEventKey,
    due_at_ms: u64,
    sequence: u64,
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other.due_at_ms.cmp(&self.due_at_ms).then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

#[derive(Debug, Default)]
struct HlsLifecycleState {
    scheduled: HashMap<HlsLifecycleEventKey, ScheduledInfo>,
    queue: BinaryHeap<QueueEntry>,
    next_sequence: u64,
}

#[derive(Debug, Default)]
pub struct HlsLifecycleManager {
    state: Mutex<HlsLifecycleState>,
    notify: Notify,
}

impl HlsLifecycleManager {
    pub fn new() -> Self { Self::default() }

    pub async fn schedule(&self, key: HlsLifecycleEventKey, due_at_ms: u64) {
        let should_notify = {
            let mut state = self.state.lock().await;
            state.next_sequence = state.next_sequence.saturating_add(1);
            let sequence = state.next_sequence;
            let previous_earliest = state.queue.peek().map(|entry| entry.due_at_ms);
            state.scheduled.insert(key.clone(), ScheduledInfo { due_at_ms, sequence });
            state.queue.push(QueueEntry { key, due_at_ms, sequence });
            previous_earliest.is_none_or(|previous| due_at_ms <= previous)
        };
        if should_notify {
            self.notify.notify_one();
        }
    }

    pub async fn cancel(&self, key: &HlsLifecycleEventKey) {
        self.state.lock().await.scheduled.remove(key);
        self.notify.notify_one();
    }

    pub async fn next_event(&self, cancel_token: &CancellationToken) -> Option<HlsLifecycleEvent> {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now_ms = current_time_millis();
                loop {
                    let Some(entry) = state.queue.peek() else {
                        break LifecycleWait::Notify;
                    };
                    let Some(info) = state.scheduled.get(&entry.key) else {
                        state.queue.pop();
                        continue;
                    };
                    if info.due_at_ms != entry.due_at_ms || info.sequence != entry.sequence {
                        state.queue.pop();
                        continue;
                    }
                    if entry.due_at_ms <= now_ms {
                        let Some(entry) = state.queue.pop() else {
                            break LifecycleWait::Notify;
                        };
                        state.scheduled.remove(&entry.key);
                        break LifecycleWait::Ready(HlsLifecycleEvent { key: entry.key, due_at_ms: entry.due_at_ms });
                    }
                    break LifecycleWait::Sleep(Duration::from_millis(entry.due_at_ms.saturating_sub(now_ms)));
                }
            };

            match wait {
                LifecycleWait::Ready(event) => return Some(event),
                LifecycleWait::Sleep(duration) => {
                    tokio::select! {
                        () = cancel_token.cancelled() => return None,
                        () = self.notify.notified() => {}
                        () = tokio::time::sleep(duration) => {}
                    }
                }
                LifecycleWait::Notify => {
                    tokio::select! {
                        () = cancel_token.cancelled() => return None,
                        () = self.notify.notified() => {}
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub async fn scheduled_len(&self) -> usize { self.state.lock().await.scheduled.len() }
}

enum LifecycleWait {
    Ready(HlsLifecycleEvent),
    Sleep(Duration),
    Notify,
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{HlsLifecycleEventKey, HlsLifecycleManager};
    use crate::api::model::{HlsAccessLeaseId, ProxySessionId};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn rescheduling_same_key_discards_stale_event() {
        let lifecycle = HlsLifecycleManager::new();
        let cancel = CancellationToken::new();
        let key = HlsLifecycleEventKey::AccessLeaseActive {
            lease_id: HlsAccessLeaseId("lease".to_string()),
            proxy_session_id: ProxySessionId("proxy".to_string()),
        };

        lifecycle.schedule(key.clone(), 1).await;
        lifecycle.schedule(key.clone(), 2).await;

        let event = lifecycle.next_event(&cancel).await.expect("event should fire");
        assert_eq!(event.key, key);
        assert_eq!(event.due_at_ms, 2);
        assert_eq!(lifecycle.scheduled_len().await, 0);
    }

    #[tokio::test]
    async fn cancel_removes_pending_key() {
        let lifecycle = HlsLifecycleManager::new();
        let key = HlsLifecycleEventKey::SessionIdle { proxy_session_id: ProxySessionId("proxy".to_string()) };

        lifecycle.schedule(key.clone(), u64::MAX).await;
        lifecycle.cancel(&key).await;

        assert_eq!(lifecycle.scheduled_len().await, 0);
    }
}
