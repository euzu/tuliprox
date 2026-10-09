use super::{
    ActiveProviderManager, ActiveProviderManagerCore, Connections, ManagedProviderHandle, ProviderReleaseSnapshot,
    EVICTED_PROVIDER_RELEASE_POLL_INTERVAL,
};
use std::{
    cmp::Reverse,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{
    AllocationId, ConnectionLifecycle, ProviderAllocation, ProviderCloseReason, ProviderHandle,
};

pub(super) struct ProviderAllocationGuard(pub(super) Option<ProviderAllocation>);

impl ProviderAllocationGuard {
    pub(super) fn new(allocation: ProviderAllocation) -> Self { Self(Some(allocation)) }

    pub(super) fn allocation(&self) -> &ProviderAllocation { self.0.as_ref().unwrap_or(&ProviderAllocation::Exhausted) }

    pub(super) fn take(&mut self) -> ProviderAllocation { self.0.take().unwrap_or(ProviderAllocation::Exhausted) }
}

impl Drop for ProviderAllocationGuard {
    fn drop(&mut self) {
        if let Some(allocation) = self.0.take() {
            allocation.release();
        }
    }
}

impl std::fmt::Debug for ManagedProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedProviderHandle").field("handle", &self.handle).finish_non_exhaustive()
    }
}

impl Drop for ManagedProviderHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.manager.release_handle(&handle);
        }
    }
}

impl ActiveProviderManagerCore {
    pub fn complete_release(&self, alloc_id: AllocationId) { self.complete_release_with_generation(alloc_id, None); }

    pub fn complete_release_with_generation(&self, alloc_id: AllocationId, generation: Option<u64>) {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        Self::complete_release_locked(&mut connections, alloc_id, generation);
    }

    pub(super) fn complete_release_locked(
        connections: &mut Connections,
        alloc_id: AllocationId,
        expected_generation: Option<u64>,
    ) -> Option<ProviderAllocation> {
        if let Some(info) = connections.single.get(&alloc_id) {
            if let Some(expected_gen) = expected_generation {
                if info.open_generation != expected_gen {
                    return None;
                }
            }
        }
        if let Some(mut info) = connections.single.remove(&alloc_id) {
            ActiveProviderManager::unindex_owner(connections, alloc_id, info.session_owner.as_ref());
            if let Some(set) = connections.single_by_addr.get_mut(&info.client_addr) {
                set.remove(&alloc_id);
                if set.is_empty() {
                    connections.single_by_addr.remove(&info.client_addr);
                }
            }
            if !info.allocation.is_unlimited_provider() {
                if let Some(name) = info.allocation.get_provider_name() {
                    if let Some(list) = connections.by_provider.get_mut(&name) {
                        list.remove(&alloc_id);
                    }
                    ActiveProviderManager::remove_priority_entry(
                        connections,
                        &name,
                        &(info.priority, Reverse(info.created_at), alloc_id),
                        info.kind,
                    );
                }
            }
            info.lifecycle = ConnectionLifecycle::Closed;
            info.cancel_token.cancel();
            info.allocation.release();
            if let Some(name) = info.allocation.get_provider_name() {
                connections.released_providers.push(name);
            }
            Some(info.allocation)
        } else {
            None
        }
    }

    pub(super) fn plan_single_release_locked(connections: &mut Connections, alloc_id: AllocationId) -> ReleaseAction {
        let Some(info) = connections.single.get_mut(&alloc_id) else {
            return ReleaseAction::None;
        };
        if info.lifecycle == ConnectionLifecycle::Closed {
            return ReleaseAction::None;
        }

        info.cancel_token.cancel();

        let completed = info.completion_token.is_cancelled();
        let needs_completion_wait = (info.lifecycle == ConnectionLifecycle::Closing
            || info.lifecycle == ConnectionLifecycle::Opening
            || info.has_body_owner)
            && !completed;

        if needs_completion_wait {
            info.lifecycle = ConnectionLifecycle::Closing;
            let client_addr = info.client_addr;
            let prio = info
                .allocation
                .get_provider_name()
                .map(|name| (name, (info.priority, Reverse(info.created_at), alloc_id), info.kind));
            let already_spawned = info.reaper_spawned;
            info.reaper_spawned = true;
            let gen = info.open_generation;
            let completion_token = info.completion_token.clone();

            if let Some((name, key, kind)) = prio {
                ActiveProviderManager::remove_priority_entry(connections, &name, &key, kind);
            }
            if let Some(set) = connections.single_by_addr.get_mut(&client_addr) {
                set.remove(&alloc_id);
                if set.is_empty() {
                    connections.single_by_addr.remove(&client_addr);
                }
            }
            if already_spawned {
                ReleaseAction::None
            } else {
                ReleaseAction::Wait(alloc_id, gen, completion_token)
            }
        } else {
            let gen = info.open_generation;
            Self::complete_release_locked(connections, alloc_id, Some(gen));
            ReleaseAction::Immediate
        }
    }
}

#[derive(Debug)]
pub(super) enum ReleaseAction {
    None,
    Immediate,
    Wait(AllocationId, u64, CancellationToken),
}

impl ActiveProviderManager {
    fn has_connections_for_addr(&self, addr: &SocketAddr) -> bool {
        let _transition = self.lock_capacity_transition();
        let connections = self.read_connections();
        connections.single.values().any(|info| info.client_addr == *addr)
            || connections
                .shared
                .by_key
                .values()
                .any(|shared| shared.connections.values().any(|subscriber| subscriber.addr == *addr))
    }

    /// Waits for every allocation currently using an address to leave the registry.
    /// Admission handoffs use `wait_for_snapshot_release` to ignore later allocations at that address.
    pub async fn wait_for_addr_release(&self, addr: &SocketAddr, timeout: Duration) -> bool {
        let deadline = TokioInstant::now() + timeout;
        loop {
            if !self.has_connections_for_addr(addr) {
                return true;
            }
            let now = TokioInstant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep_until((now + EVICTED_PROVIDER_RELEASE_POLL_INTERVAL).min(deadline)).await;
        }
    }

    pub(crate) fn release_snapshot_for_addr(&self, addr: &SocketAddr) -> ProviderReleaseSnapshot {
        let _transition = self.lock_capacity_transition();
        let connections = self.read_connections();
        let single_allocations = connections
            .single
            .values()
            .filter_map(|info| (info.client_addr == *addr).then_some(info.allocation_id))
            .collect();
        let shared_subscribers = connections
            .shared
            .by_key
            .values()
            .flat_map(|shared| {
                shared.connections.iter().filter_map(|(id, subscriber)| (subscriber.addr == *addr).then_some(*id))
            })
            .collect();
        ProviderReleaseSnapshot { addr: *addr, single_allocations, shared_subscribers }
    }

    fn has_connections_from_snapshot(&self, snapshot: &ProviderReleaseSnapshot) -> bool {
        let _transition = self.lock_capacity_transition();
        let connections = self.read_connections();
        snapshot.single_allocations.iter().any(|id| {
            connections.single.contains_key(id)
                || connections.shared.shared_by_allocation_id.get(id).is_some_and(|key| {
                    connections.shared.by_key.get(key).is_some_and(|shared| {
                        connections.shared.key_by_subscriber.get(&shared.origin_subscriber_id) == Some(key)
                    })
                })
        }) || snapshot.shared_subscribers.iter().any(|id| connections.shared.key_by_subscriber.contains_key(id))
    }

    /// Waits for the kicked transport's original provider allocations to leave the registry.
    /// The socket close may be signalled before its response bodies and provider handles are dropped.
    pub(crate) async fn wait_for_snapshot_release(
        &self,
        snapshot: &ProviderReleaseSnapshot,
        timeout: Duration,
    ) -> bool {
        let deadline = TokioInstant::now() + timeout;
        loop {
            if !self.has_connections_from_snapshot(snapshot) {
                return true;
            }
            let now = TokioInstant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep_until((now + EVICTED_PROVIDER_RELEASE_POLL_INTERVAL).min(deadline)).await;
        }
    }

    pub fn mark_closing(&self, alloc_id: AllocationId) {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        let prio = if let Some(info) = connections.single.get_mut(&alloc_id) {
            info.lifecycle = ConnectionLifecycle::Closing;
            info.cancel_token.cancel();
            info.allocation
                .get_provider_name()
                .map(|name| (name, (info.priority, Reverse(info.created_at), alloc_id), info.kind))
        } else {
            None
        };
        if let Some((name, key, kind)) = prio {
            Self::remove_priority_entry(&mut connections, &name, &key, kind);
        }
    }

    pub fn release_connection(&self, addr: &SocketAddr) {
        let _transition = self.lock_capacity_transition();
        let single_alloc_ids = {
            let mut connections = self.write_connections();
            connections.single_by_addr.remove(addr)
        };

        if let Some(alloc_ids) = single_alloc_ids {
            let mut to_reap = Vec::new();
            {
                let mut connections = self.write_connections();
                for id in alloc_ids {
                    if let ReleaseAction::Wait(alloc_id, gen, token) =
                        ActiveProviderManagerCore::plan_single_release_locked(&mut connections, id)
                    {
                        to_reap.push((alloc_id, gen, token));
                    }
                }
            }
            for (id, gen, completion_token) in to_reap {
                let core = Arc::clone(&self.core);
                let shutdown_token = self.shutdown_token.clone();
                if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
                    rt_handle.spawn(async move {
                        tokio::select! {
                            () = shutdown_token.cancelled() => {},
                            () = completion_token.cancelled() => {
                                core.complete_release_with_generation(id, Some(gen));
                            }
                        }
                    });
                }
            }
        }

        // Shared connections carried by this transport socket. Resolve the subscriber
        // ids first, then release each one precisely; two external clients behind one
        // reverse proxy hold distinct subscriber ids and never overwrite each other.
        for subscriber_id in self.release_shared_by_addr(addr) {
            self.release_shared_connection_inner(subscriber_id);
        }
    }

    pub fn release_handle(&self, handle: &ProviderHandle) {
        let _transition = self.lock_capacity_transition();
        let wait_action = {
            let mut connections = self.write_connections();
            if connections.single.contains_key(&handle.allocation_id) {
                if handle.completion_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                    if let Some(info) = connections.single.get_mut(&handle.allocation_id) {
                        if info.open_generation == handle.open_generation {
                            info.completion_token.cancel();
                        }
                    }
                }
                ActiveProviderManagerCore::plan_single_release_locked(&mut connections, handle.allocation_id)
            } else {
                Self::release_shared_handle_locked(&mut connections, handle);
                ReleaseAction::None
            }
        };

        if let ReleaseAction::Wait(alloc_id, gen, completion_token) = wait_action {
            let core = Arc::clone(&self.core);
            let shutdown_token = self.shutdown_token.clone();
            if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
                rt_handle.spawn(async move {
                    tokio::select! {
                        () = shutdown_token.cancelled() => {},
                        () = completion_token.cancelled() => {
                            core.complete_release_with_generation(alloc_id, Some(gen));
                        }
                    }
                });
            }
        }
    }

    /// Stops only stale requests of this playback, even on a multiplexed proxy socket.
    pub fn release_playback_connections(&self, owner: &str, addrs: &[SocketAddr]) {
        let handles = {
            let connections = self.read_connections();
            let mut handles = Vec::new();
            for addr in addrs {
                if let Some(alloc_ids) = connections.single_by_addr.get(addr) {
                    for id in alloc_ids {
                        if let Some(info) = connections.single.get(id) {
                            if info.request_owner.as_deref() == Some(owner) {
                                handles.push(ProviderHandle {
                                    playback_request_id: info.playback_request_id,
                                    binding_tag: None,
                                    client_id: info.client_addr,
                                    allocation_id: info.allocation_id,
                                    allocation: info.allocation.clone(),
                                    cancel_token: Some(info.cancel_token.clone()),
                                    completion_token: Some(info.completion_token.clone()),
                                    close_reason: Arc::clone(&info.close_reason),
                                    open_generation: info.open_generation,
                                });
                            }
                        }
                    }
                }
            }
            handles
        };
        for handle in handles {
            handle.set_close_reason(ProviderCloseReason::Superseded);
            if let Some(token) = &handle.cancel_token {
                token.cancel();
            }
            self.release_handle(&handle);
        }
    }

    /// Stops stale requests of this playback and awaits upstream body closure before releasing capacity.
    pub async fn release_playback_connections_await(&self, owner: &str, addrs: &[SocketAddr]) {
        let targets = {
            let mut connections = self.write_connections();
            let mut targets = Vec::new();
            for addr in addrs {
                if let Some(alloc_ids) = connections.single_by_addr.get(addr).cloned() {
                    for id in alloc_ids {
                        if let Some(info) = connections.single.get_mut(&id) {
                            if info.request_owner.as_deref() == Some(owner)
                                && info.lifecycle != ConnectionLifecycle::Closed
                            {
                                info.lifecycle = ConnectionLifecycle::Closing;
                                info.close_reason.store(ProviderCloseReason::Superseded as u8, Ordering::Release);
                                targets.push((
                                    info.allocation_id,
                                    info.cancel_token.clone(),
                                    info.completion_token.clone(),
                                    info.priority,
                                    info.created_at,
                                    info.kind,
                                    info.allocation.get_provider_name(),
                                    info.open_generation,
                                ));
                            }
                        }
                    }
                }
            }
            for (alloc_id, _, _, priority, created_at, kind, provider_name, _) in &targets {
                if let Some(name) = provider_name {
                    Self::remove_priority_entry(
                        &mut connections,
                        name,
                        &(*priority, Reverse(*created_at), *alloc_id),
                        *kind,
                    );
                }
            }
            targets
        };

        if targets.is_empty() {
            return;
        }

        for (_, cancel_token, _, _, _, _, _, _) in &targets {
            cancel_token.cancel();
        }

        let futures: Vec<_> = targets.iter().map(|(_, _, token, _, _, _, _, _)| token.cancelled()).collect();
        let _ = tokio::time::timeout(Duration::from_secs(1), futures::future::join_all(futures)).await;

        for (alloc_id, _, completion_token, _, _, _, _, gen) in targets {
            if completion_token.is_cancelled() {
                self.core.complete_release_with_generation(alloc_id, Some(gen));
            } else {
                log::warn!("Superseded provider connection {alloc_id} did not close within deadline, retaining slot as Closing until reaped");
                // Atomically claim the reaper role under the lock before spawning, so that
                // if this future was dropped between collection and here, release_handle can
                // still install its own reaper (reaper_spawned remains false until now).
                let should_spawn = {
                    let _transition = self.lock_capacity_transition();
                    let mut connections = self.write_connections();
                    connections.single.get_mut(&alloc_id).is_some_and(|info| {
                        if info.reaper_spawned {
                            false
                        } else {
                            info.reaper_spawned = true;
                            true
                        }
                    })
                };
                if should_spawn {
                    let core = Arc::clone(&self.core);
                    let shutdown_token = self.shutdown_token.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            () = shutdown_token.cancelled() => {},
                            () = completion_token.cancelled() => {
                                core.complete_release_with_generation(alloc_id, Some(gen));
                            }
                        }
                    });
                }
            }
        }
    }
}
