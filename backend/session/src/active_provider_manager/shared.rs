use super::{ActiveProviderManager, ConnectionKind, Connections, PriorityKey, PriorityOwner};
use crate::SharedStreamManager;
use ::shared::utils::sanitize_sensitive_info;
use log::error;
use std::{
    cmp::Reverse,
    collections::HashMap,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{AllocationId, ProviderAllocation, ProviderHandle, SharedSubscriberId},
    utils::debug_if_enabled,
};

pub(super) type SharedConnectionId = AllocationId;

#[derive(Debug, Clone)]
pub(super) struct SharedAllocation {
    pub(super) allocation_id: AllocationId,
    pub(super) origin_subscriber_id: SharedSubscriberId,
    pub(super) allocation: ProviderAllocation,
    /// Keyed by unique subscriber id, never by socket: two external clients behind one
    /// reverse proxy must not collapse into a single entry.
    pub(super) connections: HashMap<SharedSubscriberId, SharedSubscriber>,
    pub(super) priority: i8,
    pub(super) kind: ConnectionKind,
    pub(super) created_at: Instant,
    pub(super) cancel_token: Option<CancellationToken>,
    pub(super) session_owner: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SharedSubscriber {
    pub(super) priority: i8,
    pub(super) kind: ConnectionKind,
    /// Transport metadata and socket-wide close target only; never an identity key.
    pub(super) addr: SocketAddr,
}

#[derive(Debug, Clone, Default)]
pub(super) struct SharedConnections {
    pub(super) by_key: HashMap<Arc<str>, SharedAllocation>,
    pub(super) key_by_subscriber: HashMap<SharedSubscriberId, Arc<str>>,
    pub(super) shared_by_allocation_id: HashMap<AllocationId, Arc<str>>,
}

impl ActiveProviderManager {
    fn shared_effective_priority(
        subscribers: &HashMap<SharedSubscriberId, SharedSubscriber>,
        kind: ConnectionKind,
    ) -> Option<i8> {
        subscribers.values().filter(|subscriber| subscriber.kind == kind).map(|subscriber| subscriber.priority).min()
    }

    fn shared_effective_kind(subscribers: &HashMap<SharedSubscriberId, SharedSubscriber>) -> ConnectionKind {
        if subscribers.values().all(|subscriber| subscriber.kind == ConnectionKind::Soft) {
            ConnectionKind::Soft
        } else {
            ConnectionKind::Normal
        }
    }

    pub fn set_shared_stream_manager(&self, manager: &Arc<SharedStreamManager>) {
        let _ = self.shared_stream_manager.set(Arc::downgrade(manager));
    }

    /// Finds every shared subscription carried by one transport socket.
    ///
    /// This is an explicit socket-wide transport action (connection close, kick), not
    /// the normal playback cleanup path. A single shared subscription is released
    /// through `release_shared_subscriber`, which cannot touch other clients that
    /// happen to arrive through the same reverse-proxy socket.
    pub(super) fn release_shared_by_addr(&self, addr: &SocketAddr) -> Vec<SharedSubscriberId> {
        let connections = self.read_connections();
        connections
            .shared
            .by_key
            .values()
            .flat_map(|shared| {
                shared.connections.iter().filter_map(|(id, subscriber)| (subscriber.addr == *addr).then_some(*id))
            })
            .collect()
    }

    /// Releases exactly one shared subscription and rebalances the shared allocation.
    ///
    /// Returns the allocation to release when the last subscriber left.
    fn release_shared_subscriber(&self, subscriber_id: SharedSubscriberId) -> Option<ProviderAllocation> {
        let mut connections = self.write_connections();

        let key = connections.shared.key_by_subscriber.remove(&subscriber_id)?;
        let mut shared = connections.shared.by_key.remove(&key)?;
        shared.connections.remove(&subscriber_id);

        let shared_is_unlimited = shared.allocation.is_unlimited_provider();

        if shared.connections.is_empty() {
            connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
            Self::unindex_owner(&mut connections, shared.allocation_id, shared.session_owner.as_ref());
            if !shared_is_unlimited {
                if let Some(name) = shared.allocation.get_provider_name() {
                    if let Some(list) = connections.by_provider.get_mut(&name) {
                        list.remove(&shared.allocation_id);
                    }
                    Self::remove_priority_entry(
                        &mut connections,
                        &name,
                        &(shared.priority, Reverse(shared.created_at), shared.allocation_id),
                        shared.kind,
                    );
                }
            }
            return Some(shared.allocation);
        }

        // Recompute shared priority from remaining subscribers so preemption decisions
        // reflect who is actually still watching the shared stream. Unlimited shared
        // streams are not in the preemption index, so this rebalance is skipped.
        if !shared_is_unlimited {
            let old_priority = shared.priority;
            let old_kind = shared.kind;
            shared.kind = Self::shared_effective_kind(&shared.connections);
            if let Some(new_priority) = Self::shared_effective_priority(&shared.connections, shared.kind) {
                shared.priority = new_priority;
                if (new_priority, shared.kind) != (old_priority, old_kind) {
                    if let Some(name) = shared.allocation.get_provider_name() {
                        Self::remove_priority_entry(
                            &mut connections,
                            &name,
                            &(old_priority, Reverse(shared.created_at), shared.allocation_id),
                            old_kind,
                        );
                        Self::upsert_priority_entry(
                            &mut connections,
                            &name,
                            (new_priority, Reverse(shared.created_at), shared.allocation_id),
                            PriorityOwner::Shared(shared.allocation_id),
                            shared.kind,
                        );
                    }
                }
            }
        }
        connections.shared.by_key.insert(key, shared);
        None
    }

    pub fn release_shared_connection(&self, subscriber_id: SharedSubscriberId) {
        let _transition = self.lock_capacity_transition();
        self.release_shared_connection_inner(subscriber_id);
    }

    pub(super) fn release_shared_connection_inner(&self, subscriber_id: SharedSubscriberId) {
        if let Some(allocation) = self.release_shared_subscriber(subscriber_id) {
            debug_if_enabled!(
                "Released last shared connection for provider {} (subscriber={subscriber_id})",
                allocation.get_provider_name().unwrap_or_default()
            );
            allocation.release();
            if let Some(name) = allocation.get_provider_name() {
                self.capacity.notify(&name);
            }
        }
    }

    pub(super) fn release_shared_handle_locked(
        connections: &mut Connections,
        handle: &ProviderHandle,
    ) -> Option<ProviderAllocation> {
        let mut released = None;
        let mut released_priority_key: Option<(Arc<str>, PriorityKey, ConnectionKind)> = None;
        if let Some(key) = connections.shared.shared_by_allocation_id.remove(&handle.allocation_id) {
            if let Some(shared) = connections.shared.by_key.remove(&key) {
                Self::unindex_owner(connections, handle.allocation_id, shared.session_owner.as_ref());
                let pkey = (shared.priority, Reverse(shared.created_at), handle.allocation_id);
                let shared_kind = shared.kind;
                let shared_is_unlimited = shared.allocation.is_unlimited_provider();
                released = Some(shared.allocation);
                for subscriber_id in shared.connections.keys() {
                    connections.shared.key_by_subscriber.remove(subscriber_id);
                }
                if !shared_is_unlimited {
                    if let Some(name) = released.as_ref().and_then(ProviderAllocation::get_provider_name) {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            list.remove(&handle.allocation_id);
                        }
                        released_priority_key = Some((name, pkey, shared_kind));
                    }
                }
            }
        }
        if let Some((name, pkey, kind)) = &released_priority_key {
            Self::remove_priority_entry(connections, name, pkey, *kind);
        }
        if let Some(allocation) = released.as_ref() {
            allocation.release();
            if let Some(name) = allocation.get_provider_name() {
                connections.released_providers.push(name);
            }
        }
        released
    }

    pub fn reclassify_connection(&self, addr: &SocketAddr, kind: ConnectionKind, priority: i8) -> bool {
        self.reclassify_connection_for_owner(addr, None, kind, priority)
    }

    pub fn reclassify_connection_for_owner(
        &self,
        addr: &SocketAddr,
        owner: Option<&str>,
        kind: ConnectionKind,
        priority: i8,
    ) -> bool {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();

        let Some(alloc_ids) = connections.single_by_addr.get(addr).cloned() else {
            return false;
        };
        let mut index_updates = Vec::new();
        for allocation_id in alloc_ids {
            if let Some(info) = connections.single.get_mut(&allocation_id) {
                if info.request_owner.as_deref() != owner {
                    continue;
                }
                if let Some(provider_name) = info.allocation.get_provider_name() {
                    let old_key = (info.priority, Reverse(info.created_at), allocation_id);
                    let p_owner = PriorityOwner::Single(allocation_id);
                    let old_kind = info.kind;
                    info.kind = kind;
                    info.priority = priority;
                    let new_key = (info.priority, Reverse(info.created_at), allocation_id);
                    // Skip preemption-index updates for unlimited providers: they
                    // are intentionally absent from `by_provider` / `priority_index`
                    // / `soft_priority_index`, so the old entry was never inserted.
                    // Performing the upsert here would re-introduce them into the
                    // preemption index and contradict `max_connections: 0`.
                    if !info.allocation.is_unlimited_provider() {
                        index_updates.push((provider_name, old_key, old_kind, new_key, p_owner, info.kind));
                    }
                }
            }
        }
        for (provider_name, old_key, old_kind, new_key, p_owner, new_kind) in index_updates {
            Self::remove_priority_entry(&mut connections, &provider_name, &old_key, old_kind);
            Self::upsert_priority_entry(&mut connections, &provider_name, new_key, p_owner, new_kind);
        }
        true
    }

    pub fn reclassify_shared_connection(
        &self,
        subscriber_id: SharedSubscriberId,
        kind: ConnectionKind,
        priority: i8,
    ) -> bool {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        let shared_key = connections.shared.key_by_subscriber.get(&subscriber_id).cloned();
        let Some(shared_key) = shared_key else {
            return false;
        };
        let Some(shared_allocation) = connections.shared.by_key.get_mut(&shared_key) else {
            return false;
        };
        let shared_is_unlimited = shared_allocation.allocation.is_unlimited_provider();

        let old_priority = shared_allocation.priority;
        let old_kind = shared_allocation.kind;
        let Some(subscriber) = shared_allocation.connections.get_mut(&subscriber_id) else {
            return false;
        };
        subscriber.kind = kind;
        subscriber.priority = priority;
        shared_allocation.kind = Self::shared_effective_kind(&shared_allocation.connections);
        if let Some(new_priority) =
            Self::shared_effective_priority(&shared_allocation.connections, shared_allocation.kind)
        {
            shared_allocation.priority = new_priority;
        }
        let new_priority = shared_allocation.priority;
        let new_kind = shared_allocation.kind;
        let allocation_id = shared_allocation.allocation_id;
        let created_at = shared_allocation.created_at;
        let provider_name = shared_allocation.allocation.get_provider_name();
        let _ = shared_allocation;

        // Mirror the single-connection rule: skip priority-index rebuild for
        // shared allocations backed by an unlimited provider. The kind/priority
        // transition on the per-subscriber state is still applied above.
        if !shared_is_unlimited {
            if let Some(provider_name) = provider_name {
                let owner = PriorityOwner::Shared(allocation_id);
                Self::remove_priority_entry(
                    &mut connections,
                    &provider_name,
                    &(old_priority, Reverse(created_at), allocation_id),
                    old_kind,
                );
                Self::upsert_priority_entry(
                    &mut connections,
                    &provider_name,
                    (new_priority, Reverse(created_at), allocation_id),
                    owner,
                    new_kind,
                );
            }
        }

        true
    }

    /// Promotes exactly the allocation owned by this handle into a shared origin.
    pub fn make_shared_connection(
        &self,
        handle: &ProviderHandle,
        key: &str,
        subscriber_id: SharedSubscriberId,
    ) -> bool {
        if self.is_shutting_down.load(Ordering::Acquire) {
            return false;
        }
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return false;
        }
        let mut connections = self.write_connections();
        if connections.shared.by_key.contains_key(key) {
            return false;
        }
        let Some(info) = connections.single.remove(&handle.allocation_id) else {
            return false;
        };
        if let Some(set) = connections.single_by_addr.get_mut(&info.client_addr) {
            set.remove(&handle.allocation_id);
            if set.is_empty() {
                connections.single_by_addr.remove(&info.client_addr);
            }
        }
        let provider_name = info.allocation.get_provider_name().unwrap_or_default();
        if !info.allocation.is_unlimited_provider() {
            if let Some(list) = connections.by_provider.get_mut(&provider_name) {
                list.remove(&handle.allocation_id);
            }
            Self::remove_priority_entry(
                &mut connections,
                &provider_name,
                &(info.priority, Reverse(info.created_at), handle.allocation_id),
                info.kind,
            );
            Self::upsert_priority_entry(
                &mut connections,
                &provider_name,
                (info.priority, Reverse(info.created_at), handle.allocation_id),
                PriorityOwner::Shared(handle.allocation_id),
                info.kind,
            );
        }
        let shared_key: Arc<str> = Arc::from(key);
        connections.shared.by_key.insert(
            Arc::clone(&shared_key),
            SharedAllocation {
                allocation_id: handle.allocation_id,
                origin_subscriber_id: subscriber_id,
                allocation: info.allocation,
                connections: HashMap::from([(
                    subscriber_id,
                    SharedSubscriber { priority: info.priority, kind: info.kind, addr: handle.client_id },
                )]),
                priority: info.priority,
                kind: info.kind,
                created_at: info.created_at,
                cancel_token: Some(info.cancel_token),
                session_owner: info.session_owner,
            },
        );
        connections.shared.key_by_subscriber.insert(subscriber_id, Arc::clone(&shared_key));
        connections.shared.shared_by_allocation_id.insert(handle.allocation_id, shared_key);
        true
    }

    pub fn add_shared_connection(
        &self,
        addr: &SocketAddr,
        subscriber_id: SharedSubscriberId,
        key: &str,
        priority: i8,
        kind: ConnectionKind,
    ) -> Result<(), String> {
        if self.is_shutting_down.load(Ordering::Acquire) {
            return Err("Provider manager is shutting down".to_string());
        }
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return Err("Provider manager is shutting down".to_string());
        }
        let mut connections = self.write_connections();

        // Extract metadata before taking a second mutable borrow on `connections`.
        let metadata = connections.shared.by_key.get(key).map(|s| {
            (
                s.allocation_id,
                s.allocation.get_provider_name().unwrap_or_default(),
                s.priority,
                s.kind,
                s.created_at,
                s.allocation.is_unlimited_provider(),
            )
        });

        let Some((alloc_id, provider_name, old_priority, old_kind, created_at, shared_is_unlimited)) = metadata else {
            let err =
                format!("Failed to add shared connection for {addr}: url {} not found", sanitize_sensitive_info(key));
            error!("{err}");
            return Err(err);
        };

        debug_if_enabled!(
            "Shared connection: added addr {addr} provider={} key={}",
            sanitize_sensitive_info(&provider_name),
            sanitize_sensitive_info(key)
        );

        let Some(shared_allocation) = connections.shared.by_key.get_mut(key) else {
            let err = format!(
                "Failed to add shared connection for {addr}: url {} disappeared during update",
                sanitize_sensitive_info(key)
            );
            error!("{err}");
            return Err(err);
        };

        shared_allocation.connections.insert(subscriber_id, SharedSubscriber { priority, kind, addr: *addr });
        let new_kind = Self::shared_effective_kind(&shared_allocation.connections);
        let new_priority = Self::shared_effective_priority(&shared_allocation.connections, new_kind);
        shared_allocation.kind = new_kind;
        if let Some(new_priority) = new_priority {
            shared_allocation.priority = new_priority;
        }
        let updated_kind = shared_allocation.kind;
        let updated_priority = shared_allocation.priority;
        let needs_reindex = (updated_priority, updated_kind) != (old_priority, old_kind);
        let _ = shared_allocation;
        // Skip priority-index rebuild for unlimited providers: they are not
        // subject to preemption and therefore must not be in the index.
        if !shared_is_unlimited && needs_reindex {
            Self::remove_priority_entry(
                &mut connections,
                &provider_name,
                &(old_priority, Reverse(created_at), alloc_id),
                old_kind,
            );
            Self::upsert_priority_entry(
                &mut connections,
                &provider_name,
                (updated_priority, Reverse(created_at), alloc_id),
                PriorityOwner::Shared(alloc_id),
                updated_kind,
            );
        }

        connections.shared.key_by_subscriber.insert(subscriber_id, Arc::from(key));
        Ok(())
    }
}
