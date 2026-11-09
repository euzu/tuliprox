use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::{mpsc, oneshot};

pub(crate) struct ManualSwitchCommand {
    pub target_member_vid: u32,
    pub respondent: oneshot::Sender<Result<(), String>>,
}

pub(crate) type SwitchSender = mpsc::UnboundedSender<ManualSwitchCommand>;
pub(crate) type SwitchReceiver = mpsc::UnboundedReceiver<ManualSwitchCommand>;

pub(crate) struct DirectSwitchGuard {
    registry: Arc<FailoverSwitchRegistry>,
    stream_uid: u32,
    sender: SwitchSender,
}

impl Drop for DirectSwitchGuard {
    fn drop(&mut self) { self.registry.unregister_direct(self.stream_uid, &self.sender); }
}

pub(crate) struct SharedSwitchGuard {
    registry: Arc<FailoverSwitchRegistry>,
    target_id: u16,
    group_vid: u32,
    sender: SwitchSender,
}

impl Drop for SharedSwitchGuard {
    fn drop(&mut self) { self.registry.unregister_shared(self.target_id, self.group_vid, &self.sender); }
}

#[derive(Default)]
pub struct FailoverSwitchRegistry {
    direct: RwLock<HashMap<u32, SwitchSender>>,
    shared: RwLock<HashMap<(u16, u32), SwitchSender>>,
}

impl FailoverSwitchRegistry {
    pub(crate) fn register_direct(self: &Arc<Self>, stream_uid: u32, sender: SwitchSender) -> DirectSwitchGuard {
        if let Ok(mut lock) = self.direct.write() {
            lock.insert(stream_uid, sender.clone());
        }
        DirectSwitchGuard { registry: Arc::clone(self), stream_uid, sender }
    }

    pub(crate) fn unregister_direct(&self, stream_uid: u32, sender: &SwitchSender) {
        if let Ok(mut lock) = self.direct.write() {
            if lock.get(&stream_uid).is_some_and(|stored| stored.same_channel(sender)) {
                lock.remove(&stream_uid);
            }
        }
    }

    pub(crate) fn register_shared(
        self: &Arc<Self>,
        target_id: u16,
        group_vid: u32,
        sender: SwitchSender,
    ) -> SharedSwitchGuard {
        if let Ok(mut lock) = self.shared.write() {
            lock.insert((target_id, group_vid), sender.clone());
        }
        SharedSwitchGuard { registry: Arc::clone(self), target_id, group_vid, sender }
    }

    pub(crate) fn unregister_shared(&self, target_id: u16, group_vid: u32, sender: &SwitchSender) {
        if let Ok(mut lock) = self.shared.write() {
            if lock.get(&(target_id, group_vid)).is_some_and(|stored| stored.same_channel(sender)) {
                lock.remove(&(target_id, group_vid));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn contains_shared(&self, target_id: u16, group_vid: u32) -> bool {
        self.shared.read().is_ok_and(|lock| lock.contains_key(&(target_id, group_vid)))
    }

    #[cfg(test)]
    pub(crate) fn contains_direct(&self, stream_uid: u32) -> bool {
        self.direct.read().is_ok_and(|lock| lock.contains_key(&stream_uid))
    }

    pub async fn switch_stream(
        &self,
        stream_uid: u32,
        is_shared: bool,
        target_id: u16,
        group_vid: u32,
        target_member_vid: u32,
    ) -> Result<(), String> {
        let sender = if is_shared {
            self.shared
                .read()
                .ok()
                .and_then(|lock| lock.get(&(target_id, group_vid)).cloned())
                .or_else(|| self.direct.read().ok().and_then(|lock| lock.get(&stream_uid).cloned()))
        } else {
            self.direct
                .read()
                .ok()
                .and_then(|lock| lock.get(&stream_uid).cloned())
                .or_else(|| self.shared.read().ok().and_then(|lock| lock.get(&(target_id, group_vid)).cloned()))
        };

        let Some(sender) = sender else {
            return Err("Stream runtime failover driver not active or not found".to_string());
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        sender
            .send(ManualSwitchCommand { target_member_vid, respondent: resp_tx })
            .map_err(|_| "Stream driver channel closed".to_string())?;

        tokio::time::timeout(std::time::Duration::from_secs(10), resp_rx)
            .await
            .map_err(|_| "Switch operation timed out".to_string())?
            .map_err(|_| "Switch respondent dropped".to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_switch_guard_unregisters_on_drop() {
        let registry = Arc::new(FailoverSwitchRegistry::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let guard = registry.register_direct(100, tx);
        assert!(registry.contains_direct(100));
        drop(guard);
        assert!(!registry.contains_direct(100));
    }

    #[test]
    fn direct_switch_guard_does_not_unregister_newer_stream_on_drop() {
        let registry = Arc::new(FailoverSwitchRegistry::default());
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let guard1 = registry.register_direct(100, tx1);
        let guard2 = registry.register_direct(100, tx2);
        assert!(registry.contains_direct(100));
        drop(guard1);
        assert!(registry.contains_direct(100));
        drop(guard2);
        assert!(!registry.contains_direct(100));
    }

    #[test]
    fn shared_switch_guard_unregisters_on_drop() {
        let registry = Arc::new(FailoverSwitchRegistry::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let guard = registry.register_shared(1, 42, tx);
        assert!(registry.contains_shared(1, 42));
        drop(guard);
        assert!(!registry.contains_shared(1, 42));
    }

    #[test]
    fn shared_switch_guard_does_not_unregister_newer_stream_on_drop() {
        let registry = Arc::new(FailoverSwitchRegistry::default());
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let guard1 = registry.register_shared(1, 42, tx1);
        let guard2 = registry.register_shared(1, 42, tx2);
        assert!(registry.contains_shared(1, 42));
        drop(guard1);
        assert!(registry.contains_shared(1, 42));
        drop(guard2);
        assert!(!registry.contains_shared(1, 42));
    }
}
