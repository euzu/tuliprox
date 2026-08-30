use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::Mutex;

type StalkerResolveGuardKey = (u16, u32);

/// Instance-scoped serialization for Stalker portal URL resolution.
///
/// The map keeps only weak references so cancellation cannot retain an unused
/// per-provider lock. Expired entries are pruned opportunistically whenever a
/// new guard is requested.
#[derive(Default)]
pub(crate) struct StalkerResolveCoordinator {
    guards: Mutex<HashMap<StalkerResolveGuardKey, Weak<Mutex<()>>>>,
}

impl StalkerResolveCoordinator {
    pub(crate) async fn guard_for(&self, input_id: u16, provider_id: u32) -> Arc<Mutex<()>> {
        let key = (input_id, provider_id);
        let mut guards = self.guards.lock().await;
        guards.retain(|_, guard| guard.strong_count() > 0);
        if let Some(guard) = guards.get(&key).and_then(Weak::upgrade) {
            return guard;
        }
        let guard = Arc::new(Mutex::new(()));
        guards.insert(key, Arc::downgrade(&guard));
        guard
    }
}

#[cfg(test)]
mod tests {
    use super::StalkerResolveCoordinator;
    use std::sync::Arc;

    #[tokio::test]
    async fn same_instance_reuses_only_live_guard_for_the_same_key() {
        let coordinator = StalkerResolveCoordinator::default();
        let first = coordinator.guard_for(1, 2).await;
        let concurrent = coordinator.guard_for(1, 2).await;
        assert!(Arc::ptr_eq(&first, &concurrent));

        drop(first);
        drop(concurrent);
        let replacement = coordinator.guard_for(1, 2).await;
        assert_eq!(Arc::strong_count(&replacement), 1);
    }

    #[tokio::test]
    async fn separate_instances_do_not_share_guards() {
        let first = StalkerResolveCoordinator::default().guard_for(1, 2).await;
        let second = StalkerResolveCoordinator::default().guard_for(1, 2).await;

        assert!(!Arc::ptr_eq(&first, &second));
    }
}
