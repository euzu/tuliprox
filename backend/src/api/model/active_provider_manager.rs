use crate::{
    api::model::{
        provider_lineup_manager::{ProviderAllocation, ProviderLineupManager},
        EventManager, ProviderConfig,
    },
    model::{AppConfig, ConfigInput, GracePeriodOptions},
    utils::debug_if_enabled,
};
use log::error;
use shared::utils::sanitize_sensitive_info;
// trace_if_enabled removed
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

const PREEMPTED_PROBE_CANCEL_GRACE: Duration = Duration::from_secs(2);
const PREEMPTED_GRACE_MAX_PENDING: usize = 64;
static DUMMY_ADDR: LazyLock<SocketAddr> = LazyLock::new(|| "127.0.0.1:0".parse::<SocketAddr>().unwrap());

pub type ClientConnectionId = SocketAddr;
type AllocationId = u64;
// Key for BTreeMap priority index: (priority, Reverse<created_at>, AllocationId)
// .last() = highest priority value = lowest importance = best eviction victim
type PriorityKey = (i8, Reverse<Instant>, AllocationId);

#[derive(Debug, Clone)]
pub struct ProviderHandle {
    pub client_id: ClientConnectionId,
    pub allocation_id: AllocationId,
    pub allocation: ProviderAllocation,
    // Token to cancel the background task (e.g. internal probe) if preempted
    pub cancel_token: Option<CancellationToken>,
}

impl ProviderHandle {
    pub fn new(
        client_id: ClientConnectionId,
        allocation_id: AllocationId,
        allocation: ProviderAllocation,
        cancel_token: Option<CancellationToken>,
    ) -> Self {
        Self { client_id, allocation_id, allocation, cancel_token }
    }
}

#[derive(Debug, Clone)]
struct SharedAllocation {
    allocation_id: AllocationId,
    allocation: ProviderAllocation,
    connections: HashSet<ClientConnectionId>,
    priority: i8,
    created_at: Instant,
}

#[derive(Debug, Clone)]
struct ActiveConnectionInfo {
    allocation: ProviderAllocation,
    priority: i8,
    is_probe: bool,
    // Used to signal preemption to the consumer of this connection
    cancel_token: CancellationToken,
    created_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct SharedConnections {
    by_key: HashMap<String, SharedAllocation>,
    key_by_addr: HashMap<ClientConnectionId, String>,
    shared_by_allocation_id: HashMap<AllocationId, String>,
}

#[derive(Debug, Clone, Default)]
struct Connections {
    // Map Addr -> AllocationID -> Allocation Info
    single: HashMap<ClientConnectionId, HashMap<AllocationId, ActiveConnectionInfo>>,
    shared: SharedConnections,
    // Index to quickly find connections by provider name for preemption
    // ProviderName -> Vec<(ClientConnectionId, AllocationId)>
    by_provider: HashMap<Arc<str>, Vec<(ClientConnectionId, AllocationId)>>,
    // Priority index per provider alias for O(log n) victim lookup
    // ProviderName -> BTreeMap<PriorityKey, (ClientConnectionId, is_shared)>
    priority_index: HashMap<Arc<str>, BTreeMap<PriorityKey, (ClientConnectionId, bool)>>,
}

pub struct ActiveProviderManager {
    providers: ProviderLineupManager,
    connections: RwLock<Connections>,
    next_allocation_id: AtomicU64,
    preempted_grace_semaphore: Arc<Semaphore>,
}

impl ActiveProviderManager {
    pub fn new(cfg: &AppConfig, event_manager: &Arc<EventManager>) -> Self {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        Self {
            providers: ProviderLineupManager::new(inputs, grace_period_options, event_manager),
            connections: RwLock::new(Connections::default()),
            next_allocation_id: AtomicU64::new(1),
            preempted_grace_semaphore: Arc::new(Semaphore::new(PREEMPTED_GRACE_MAX_PENDING)),
        }
    }

    fn get_config_inputs(cfg: &AppConfig) -> Vec<Arc<ConfigInput>> {
        cfg.sources.load().inputs.iter().filter(|i| i.enabled).map(Arc::clone).collect()
    }

    fn get_grace_options(cfg: &AppConfig) -> GracePeriodOptions { cfg.config.load().get_grace_options() }

    pub async fn update_config(&self, cfg: &AppConfig) {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        self.providers.update_config(inputs, &grace_period_options);
        self.reconcile_connections().await;
    }

    pub async fn reconcile_connections(&self) {
        let mut counts = HashMap::<Arc<str>, usize>::new();
        {
            let connections = self.connections.read().await;

            // Single connections
            for per_addr in connections.single.values() {
                for info in per_addr.values() {
                    if let Some(name) = info.allocation.get_provider_name() {
                        *counts.entry(name).or_insert(0) += 1;
                    }
                }
            }

            // Shared connections
            for shared in connections.shared.by_key.values() {
                if let Some(name) = shared.allocation.get_provider_name() {
                    *counts.entry(name).or_insert(0) += 1;
                }
            }
        }

        self.providers.reconcile_connections(counts).await;
    }

    async fn acquire_connection_inner(
        &self,
        provider_or_input_name: &Arc<str>,
        addr: &SocketAddr,
        force: bool,
        allow_grace_override: Option<bool>,
        priority: i8,
        is_probe: bool,
    ) -> Option<ProviderHandle> {
        // 1. Try to acquire directly
        let (allow_grace, allocation) = if force {
            (true, self.providers.force_exact_acquire_connection(provider_or_input_name).await)
        } else {
            match allow_grace_override {
                Some(allow_grace) => (
                    allow_grace,
                    self.providers.acquire_connection_with_grace_override(provider_or_input_name, allow_grace).await,
                ),
                None => (true, self.providers.acquire_connection(provider_or_input_name).await),
            }
        };

        if !matches!(allocation, ProviderAllocation::Exhausted) {
            if matches!(&allocation, ProviderAllocation::GracePeriod(_)) && !force {
                // Grace allocation received — try to evict a lower-priority victim
                // across the entire input lineup (all aliases) to free capacity.
                if self.evict_lower_priority_on_input(provider_or_input_name, priority).await {
                    // A victim was evicted. If it was on the same alias as our grace
                    // allocation, the provider is back at max (not over limit).
                    // If it was on a different alias, release the grace allocation
                    // and re-acquire on the input to land on the freed alias.
                    let evicted_on_same = !self.providers.is_over_limit(
                        &allocation.get_provider_name().unwrap_or_default(),
                    ).await;
                    if !evicted_on_same {
                        // Victim was on a different alias — release grace, re-acquire
                        allocation.release().await;
                        let new_alloc = self.providers.acquire_connection_with_grace_override(
                            provider_or_input_name, allow_grace,
                        ).await;
                        if !matches!(new_alloc, ProviderAllocation::Exhausted) {
                            return self.register_allocation(new_alloc, addr, priority, is_probe).await;
                        }
                        // Fallback: could not re-acquire (race), fall through to exhausted path
                        return None;
                    }
                }
                // Eviction on same alias succeeded, or no victim found → keep grace allocation
            }
            return self.register_allocation(allocation, addr, priority, is_probe).await;
        }

        // 2. If exhausted, try preemption (kick lower priority connection)
        if !force {
            if let Some(preempted_alloc) =
                self.try_preempt_connection(provider_or_input_name, priority, allow_grace).await
            {
                return self.register_allocation(preempted_alloc, addr, priority, is_probe).await;
            }
        }

        None
    }

    async fn register_allocation(
        &self,
        allocation: ProviderAllocation,
        addr: &SocketAddr,
        priority: i8,
        is_probe: bool,
    ) -> Option<ProviderHandle> {
        let provider_name = allocation.get_provider_name().unwrap_or_default();
        let allocation_id = self.next_allocation_id.fetch_add(1, Ordering::Relaxed);
        let cancel_token = CancellationToken::new();
        let now = Instant::now();

        let mut connections = self.connections.write().await;
        let per_addr = connections.single.entry(*addr).or_default();

        per_addr.insert(
            allocation_id,
            ActiveConnectionInfo {
                allocation: allocation.clone(),
                priority,
                is_probe,
                cancel_token: cancel_token.clone(),
                created_at: now,
            },
        );

        connections.by_provider.entry(provider_name.clone()).or_default().push((*addr, allocation_id));
        connections.priority_index.entry(provider_name.clone()).or_default()
            .insert((priority, Reverse(now), allocation_id), (*addr, false));

        debug_if_enabled!(
            "Added provider connection {provider_name:?} for {} (prio={})",
            sanitize_sensitive_info(&addr.to_string()),
            priority
        );
        Some(ProviderHandle::new(*addr, allocation_id, allocation, Some(cancel_token)))
    }

    #[allow(clippy::too_many_lines)]
    /// Evict a single lower-priority connection across the entire input lineup
    /// (all provider aliases). Used when a `GracePeriod` allocation was granted.
    /// Returns true if a victim was successfully evicted.
    async fn evict_lower_priority_on_input(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
    ) -> bool {
        // Find best victim across all aliases under read lock
        let victim: Option<(ClientConnectionId, AllocationId, i8, Instant, bool)> = {
            let connections = self.connections.read().await;
            let mut found: Option<(ClientConnectionId, AllocationId, i8, Instant, bool)> = None;
            for (prov_name, tree) in &connections.priority_index {
                if !self.providers.is_provider_for_input(prov_name, input_name) {
                    continue;
                }
                for ((prio, Reverse(created_at), alloc_id), (addr, is_shared)) in tree.iter().rev() {
                    if *prio <= new_priority {
                        break; // No more victims on this alias
                    }
                    if *is_shared {
                        let evictable = connections.shared.by_key.values()
                            .any(|s| s.allocation_id == *alloc_id && s.connections.len() == 1);
                        if !evictable { continue; }
                    }
                    let is_better = match found {
                        None => true,
                        Some((_, _, v_prio, v_created, _)) => {
                            *prio > v_prio || (*prio == v_prio && *created_at < v_created)
                        }
                    };
                    if is_better {
                        found = Some((*addr, *alloc_id, *prio, *created_at, *is_shared));
                    }
                    break; // Only best candidate per alias
                }
            }
            found
        };

        let Some((addr, alloc_id, v_prio, victim_created_at, is_shared)) = victim else {
            return false;
        };

        debug_if_enabled!(
            "Grace-evicting {} connection from {} (prio={}) on input {} for higher priority request (prio={})",
            if is_shared { "shared" } else { "single" },
            sanitize_sensitive_info(&addr.to_string()),
            v_prio,
            sanitize_sensitive_info(input_name),
            new_priority
        );

        if is_shared {
            let released = {
                let mut connections = self.connections.write().await;
                let Some(key) = connections.shared.key_by_addr.get(&addr).cloned() else {
                    return false;
                };

                let still_single = connections.shared.by_key.get(&key).is_some_and(|shared| {
                    shared.allocation_id == alloc_id
                        && shared.connections.len() == 1
                        && shared.priority == v_prio
                        && shared.created_at == victim_created_at
                });
                if !still_single {
                    return false;
                }

                if let Some(shared) = connections.shared.by_key.remove(&key) {
                    connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
                    for shared_addr in &shared.connections {
                        connections.shared.key_by_addr.remove(shared_addr);
                    }
                    if let Some(name) = shared.allocation.get_provider_name() {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            list.retain(|(_, i)| *i != shared.allocation_id);
                        }
                        if let Some(tree) = connections.priority_index.get_mut(&name) {
                            tree.remove(&(v_prio, Reverse(victim_created_at), alloc_id));
                        }
                    }
                    Some(shared.allocation)
                } else {
                    return false;
                }
            };
            if let Some(allocation) = released {
                allocation.release().await;
            }
        } else {
            let removed_info = {
                let mut connections = self.connections.write().await;
                let mut removed_info = None;
                let mut removed_provider_name = None;
                let mut remove_addr_entry = false;
                if let Some(per_addr) = connections.single.get_mut(&addr) {
                    if let Some(info) = per_addr.get(&alloc_id) {
                        if info.priority != v_prio || info.created_at != victim_created_at {
                            return false;
                        }
                    }
                    if let Some(info) = per_addr.remove(&alloc_id) {
                        removed_provider_name = info.allocation.get_provider_name();
                        remove_addr_entry = per_addr.is_empty();
                        removed_info = Some(info);
                    }
                }
                if remove_addr_entry {
                    connections.single.remove(&addr);
                }
                if let Some(ref name) = removed_provider_name {
                    if let Some(list) = connections.by_provider.get_mut(name) {
                        if let Some(idx) = list.iter().position(|(a, i)| *a == addr && *i == alloc_id) {
                            list.remove(idx);
                        }
                    }
                    if let Some(tree) = connections.priority_index.get_mut(name) {
                        tree.remove(&(v_prio, Reverse(victim_created_at), alloc_id));
                    }
                }
                removed_info
            };

            let Some(info) = removed_info else { return false; };
            let token = info.cancel_token;
            if info.is_probe {
                if let Ok(permit) = Arc::clone(&self.preempted_grace_semaphore).try_acquire_owned() {
                    tokio::spawn(async move {
                        let _permit = permit;
                        tokio::time::sleep(PREEMPTED_PROBE_CANCEL_GRACE).await;
                        token.cancel();
                    });
                } else {
                    token.cancel();
                }
            } else {
                token.cancel();
            }
            info.allocation.release().await;
        }

        true
    }

    #[allow(clippy::too_many_lines)]
    async fn try_preempt_connection(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
        allow_grace: bool,
    ) -> Option<ProviderAllocation> {
        // Victim: (addr, alloc_id, priority, created_at, is_shared)
        let mut victim: Option<(ClientConnectionId, AllocationId, i8, Instant, bool)> = None;

        {
            let connections = self.connections.read().await;

            // Search across ALL provider aliases of this input using the priority index
            for (prov_name, tree) in &connections.priority_index {
                if !self.providers.is_provider_for_input(prov_name, input_name) {
                    continue;
                }
                // Iterate from highest priority value (lowest importance) = best victim
                for ((prio, Reverse(created_at), alloc_id), (addr, is_shared)) in tree.iter().rev() {
                    if *prio <= new_priority {
                        break; // No more victims on this alias
                    }
                    if *is_shared {
                        let evictable = connections.shared.by_key.values()
                            .any(|s| s.allocation_id == *alloc_id && s.connections.len() == 1);
                        if !evictable { continue; }
                    }
                    let is_better = match victim {
                        None => true,
                        Some((_, _, v_prio, v_created, _)) => {
                            *prio > v_prio || (*prio == v_prio && *created_at < v_created)
                        }
                    };
                    if is_better {
                        victim = Some((*addr, *alloc_id, *prio, *created_at, *is_shared));
                    }
                    break; // Only need the best candidate per alias
                }
            }
        }

        if let Some((addr, alloc_id, v_prio, victim_created_at, is_shared)) = victim {
            debug_if_enabled!(
                "Preempting {} connection from {} (prio={}) for higher priority request (prio={})",
                if is_shared { "shared" } else { "single" },
                sanitize_sensitive_info(&addr.to_string()),
                v_prio,
                new_priority
            );

            if is_shared {
                let released_shared_allocation = {
                    let mut connections = self.connections.write().await;
                    let key = connections.shared.key_by_addr.get(&addr).cloned()?;

                    // Revalidate the selected shared victim under WRITE lock to avoid evicting
                    // a connection that gained additional listeners concurrently.
                    let still_single = connections.shared.by_key.get(&key).is_some_and(|shared| {
                        shared.allocation_id == alloc_id
                            && shared.connections.len() == 1
                            && shared.priority == v_prio
                            && shared.created_at == victim_created_at
                    });
                    if !still_single {
                        None
                    } else if let Some(shared) = connections.shared.by_key.remove(&key) {
                        connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
                        for shared_addr in &shared.connections {
                            connections.shared.key_by_addr.remove(shared_addr);
                        }

                        if let Some(name) = shared.allocation.get_provider_name() {
                            if let Some(list) = connections.by_provider.get_mut(&name) {
                                list.retain(|(_, i)| *i != shared.allocation_id);
                            }
                            if let Some(tree) = connections.priority_index.get_mut(&name) {
                                tree.remove(&(v_prio, Reverse(victim_created_at), alloc_id));
                            }
                        }
                        Some(shared.allocation)
                    } else {
                        None
                    }
                };
                let allocation = released_shared_allocation?;
                allocation.release().await;
            } else {
                // Atomically remove the victim single connection and take ownership of the token.
                // This guarantees only one concurrent preemptor can schedule delayed cancellation.
                let removed_info = {
                    let mut connections = self.connections.write().await;

                    let mut removed_info = None;
                    let mut removed_provider_name = None;
                    let mut remove_addr_entry = false;
                    if let Some(per_addr) = connections.single.get_mut(&addr) {
                        if let Some(info) = per_addr.get(&alloc_id) {
                            // Revalidate victim selection under write lock.
                            if info.priority != v_prio || info.created_at != victim_created_at {
                                return None;
                            }
                        }
                        if let Some(info) = per_addr.remove(&alloc_id) {
                            removed_provider_name = info.allocation.get_provider_name();
                            remove_addr_entry = per_addr.is_empty();
                            removed_info = Some(info);
                        }
                    }

                    if remove_addr_entry {
                        connections.single.remove(&addr);
                    }
                    if let Some(name) = removed_provider_name {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            if let Some(idx) = list.iter().position(|(a, i)| *a == addr && *i == alloc_id) {
                                list.remove(idx);
                            }
                        }
                        if let Some(tree) = connections.priority_index.get_mut(&name) {
                            tree.remove(&(v_prio, Reverse(victim_created_at), alloc_id));
                        }
                    }
                    removed_info
                };

                let Some(info) = removed_info else {
                    // Another preemptor already removed this victim.
                    return None;
                };

                let token = info.cancel_token;
                if info.is_probe {
                    // Probe preemption gets a short grace window, but cap detached sleep/cancel
                    // tasks so bursts cannot spawn unbounded background work.
                    if let Ok(permit) = Arc::clone(&self.preempted_grace_semaphore).try_acquire_owned() {
                        tokio::spawn(async move {
                            let _permit = permit;
                            tokio::time::sleep(PREEMPTED_PROBE_CANCEL_GRACE).await;
                            token.cancel();
                        });
                    } else {
                        token.cancel();
                    }
                } else {
                    token.cancel();
                }

                info.allocation.release().await;
            }

            // Now try acquire again preserving the original grace policy.
            let allocation = self.providers.acquire_connection_with_grace_override(input_name, allow_grace).await;
            if !matches!(allocation, ProviderAllocation::Exhausted) {
                return Some(allocation);
            }
        }

        None
    }

    pub async fn acquire_exact_connection_with_grace(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
    ) -> Option<ProviderHandle> {
        let allocation = self.providers.acquire_exact_connection_with_grace_override(provider_name, allow_grace).await;
        if matches!(allocation, ProviderAllocation::Exhausted) {
            return None;
        }
        self.register_allocation(allocation, addr, priority, false).await
    }

    pub async fn force_exact_acquire_connection(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        priority: i8,
    ) -> Option<ProviderHandle> {
        // Compatibility wrapper: keep the exact-provider behavior but do not over-allocate exhausted accounts.
        self.acquire_exact_connection_with_grace(provider_name, addr, false, priority).await
    }

    // Returns the next available provider connection
    pub async fn acquire_connection(&self, input_name: &Arc<str>, addr: &SocketAddr, priority: i8) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, None, priority, false).await
    }

    /// Acquire a provider connection while explicitly controlling provider-side grace allocations.
    pub async fn acquire_connection_with_grace(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, Some(allow_grace), priority, false).await
    }

    /// Acquire a provider connection for probe tasks with configurable priority.
    /// Probes never consume grace capacity.
    pub async fn acquire_connection_for_probe(&self, input_name: &Arc<str>, priority: i8) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, &DUMMY_ADDR, false, Some(false), priority, true).await
    }

    // This method is used for redirects to cycle through the provider
    pub async fn get_next_provider(&self, provider_name: &Arc<str>) -> Option<Arc<ProviderConfig>> {
        self.providers.get_next_provider(provider_name).await
    }

    pub async fn active_connections(&self) -> Option<HashMap<Arc<str>, usize>> {
        self.providers.active_connections().await
    }

    pub async fn is_over_limit(&self, provider_name: &Arc<str>) -> bool {
        self.providers.is_over_limit(provider_name).await
    }

    pub async fn is_exhausted(&self, provider_name: &Arc<str>) -> bool {
        self.providers.is_exhausted(provider_name).await
    }

    pub async fn release_connection(&self, addr: &SocketAddr) {
        // Single connection - all index updates in one lock scope
        let single_allocations = {
            let mut connections = self.connections.write().await;
            if let Some(allocations) = connections.single.remove(addr) {
                // Remove from by_provider and priority_index while still holding the lock
                for (id, info) in &allocations {
                    if let Some(name) = info.allocation.get_provider_name() {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            if let Some(idx) = list.iter().position(|(a, i)| *a == *addr && *i == *id) {
                                list.remove(idx);
                            }
                        }
                        if let Some(tree) = connections.priority_index.get_mut(&name) {
                            tree.remove(&(info.priority, Reverse(info.created_at), *id));
                        }
                    }
                }
                Some(allocations)
            } else {
                None
            }
        };

        if let Some(allocations) = single_allocations {
            for (_, info) in allocations {
                debug_if_enabled!(
                    "Released provider connection {:?} for {}",
                    info.allocation.get_provider_name().unwrap_or_default(),
                    sanitize_sensitive_info(&addr.to_string())
                );
                info.allocation.release().await;
            }
            return;
        }

        // Shared connection
        let shared_allocation = {
            let mut connections = self.connections.write().await;

            let key = match connections.shared.key_by_addr.get(addr) {
                Some(k) => k.clone(),
                None => return, // no shared connection
            };

            // Clone the SharedAllocation to avoid double mutable borrow
            let mut shared = match connections.shared.by_key.get(&key) {
                Some(s) => s.clone(),
                None => return,
            };

            // Remove this address from the shared connection set
            shared.connections.remove(addr);
            // Always remove stale key-by-addr entry
            connections.shared.key_by_addr.remove(addr);

            if shared.connections.is_empty() {
                // If this was the last user of the shared allocation:
                connections.shared.by_key.remove(&key);
                connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
                if let Some(name) = shared.allocation.get_provider_name() {
                    if let Some(list) = connections.by_provider.get_mut(&name) {
                        list.retain(|(_, i)| *i != shared.allocation_id);
                    }
                    if let Some(tree) = connections.priority_index.get_mut(&name) {
                        tree.remove(&(shared.priority, Reverse(shared.created_at), shared.allocation_id));
                    }
                }
                Some(shared.allocation)
            } else {
                // Update the entry back with the remaining connections
                connections.shared.by_key.insert(key, shared);
                None
            }
        };

        // release allocation
        if let Some(allocation) = shared_allocation {
            allocation.release().await;
            debug_if_enabled!(
                "Released last shared connection for provider {}, releasing allocation {}",
                allocation.get_provider_name().unwrap_or_default(),
                sanitize_sensitive_info(&addr.to_string())
            );
        }
    }

    pub async fn release_handle(&self, handle: &ProviderHandle) {
        let mut released = None;
        let mut released_priority_key: Option<(Arc<str>, PriorityKey)> = None;
        {
            let mut connections = self.connections.write().await;

            // Try removing from Single
            if let Some(per_addr) = connections.single.get_mut(&handle.client_id) {
                if let Some(info) = per_addr.remove(&handle.allocation_id) {
                    let pkey = (info.priority, Reverse(info.created_at), handle.allocation_id);
                    released = Some(info.allocation);
                    if per_addr.is_empty() {
                        connections.single.remove(&handle.client_id);
                    }

                    // Remove from by_provider index
                    if let Some(name) = released.as_ref().and_then(ProviderAllocation::get_provider_name) {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            if let Some(idx) =
                                list.iter().position(|(a, i)| *a == handle.client_id && *i == handle.allocation_id)
                            {
                                list.remove(idx);
                            }
                        }
                        released_priority_key = Some((name, pkey));
                    }
                }
            }

            if released.is_none() {
                // Try removing from Shared
                if let Some(key) = connections.shared.shared_by_allocation_id.remove(&handle.allocation_id) {
                    if let Some(shared) = connections.shared.by_key.remove(&key) {
                        let pkey = (shared.priority, Reverse(shared.created_at), handle.allocation_id);
                        released = Some(shared.allocation);
                        for addr in shared.connections {
                            connections.shared.key_by_addr.remove(&addr);
                        }
                        if let Some(name) = released.as_ref().and_then(ProviderAllocation::get_provider_name) {
                            if let Some(list) = connections.by_provider.get_mut(&name) {
                                list.retain(|(_, i)| *i != handle.allocation_id);
                            }
                            released_priority_key = Some((name, pkey));
                        }
                    }
                }
            }

            // Remove from priority_index
            if let Some((name, pkey)) = &released_priority_key {
                if let Some(tree) = connections.priority_index.get_mut(name) {
                    tree.remove(pkey);
                }
            }
        }

        if let Some(allocation) = released {
            allocation.release().await;
        }
    }

    pub async fn make_shared_connection(&self, addr: &SocketAddr, key: &str) {
        let extras = {
            let mut connections = self.connections.write().await;
            let mut extras = Vec::new();

            // Find the allocation to promote (must be single)
            // Logic change: we must find the specific allocation if multiple exist, but usually per client only 1 active?
            // Existing logic assumes one.
            let handle = if let Some(m) = connections.single.get_mut(addr) {
                if m.is_empty() {
                    None
                } else {
                    let mut iter = m.drain();
                    if let Some((id, info)) = iter.next() {
                        // Collect others as extras to release
                        let extra_entries: Vec<_> = iter.collect();

                        // Cleanup indices
                        if let Some(name) = info.allocation.get_provider_name() {
                            if let Some(list) = connections.by_provider.get_mut(&name) {
                                list.retain(|(a, _)| *a != *addr);
                            }
                            // Remove old single entry from priority_index
                            if let Some(tree) = connections.priority_index.get_mut(&name) {
                                tree.remove(&(info.priority, Reverse(info.created_at), id));
                                // Remove extras from priority_index
                                for (extra_id, extra_info) in &extra_entries {
                                    tree.remove(&(extra_info.priority, Reverse(extra_info.created_at), *extra_id));
                                }
                            }
                        }

                        for (_, extra_info) in extra_entries {
                            extras.push(extra_info.allocation);
                        }

                        connections.single.remove(addr); // Map is drained/empty now

                        Some((
                            ProviderHandle::new(*addr, id, info.allocation, Some(info.cancel_token)),
                            info.priority,
                            info.created_at,
                        ))
                    } else {
                        None
                    }
                }
            } else {
                None
            };

            if let Some(handle) = &handle {
                let provider_name = handle.0.allocation.get_provider_name().unwrap_or_default();
                debug_if_enabled!(
                    "Shared connection: promoted addr {addr} provider={} key={}",
                    sanitize_sensitive_info(&provider_name),
                    sanitize_sensitive_info(key)
                );

                connections.shared.by_key.insert(
                    key.to_string(),
                    SharedAllocation {
                        allocation_id: handle.0.allocation_id,
                        allocation: handle.0.allocation.clone(),
                        connections: HashSet::from([*addr]),
                        priority: handle.1,
                        created_at: handle.2,
                    },
                );
                connections.shared.key_by_addr.insert(*addr, key.to_string());
                connections.shared.shared_by_allocation_id.insert(handle.0.allocation_id, key.to_string());

                // Insert new shared entry into priority_index
                connections.priority_index.entry(provider_name.clone()).or_default()
                    .insert((handle.1, Reverse(handle.2), handle.0.allocation_id), (*addr, true));
            }
            extras
        };

        for allocation in extras {
            allocation.release().await;
        }
    }

    pub async fn add_shared_connection(&self, addr: &SocketAddr, key: &str) {
        let mut connections = self.connections.write().await;
        if let Some(shared_allocation) = connections.shared.by_key.get_mut(key) {
            let provider_name = shared_allocation.allocation.get_provider_name().unwrap_or_default();
            debug_if_enabled!(
                "Shared connection: added addr {addr} provider={} key={}",
                sanitize_sensitive_info(&provider_name),
                sanitize_sensitive_info(key)
            );
            shared_allocation.connections.insert(*addr);
            connections.shared.key_by_addr.insert(*addr, key.to_string());
        } else {
            error!("Failed to add shared connection for {addr}: url {} not found", sanitize_sensitive_info(key));
        }
    }

    pub async fn get_provider_connections_count(&self) -> usize { self.providers.active_connection_count().await }
}

#[cfg(test)]
mod tests {
    use super::ActiveProviderManager;
    use crate::{
        api::model::EventManager,
        model::{AppConfig, Config, ConfigInput, ConfigInputAlias, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType},
        utils::Internable,
    };
    use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
    use shared::utils::{default_probe_user_priority, default_user_priority};

    fn build_test_app_config(aliases: Option<Vec<ConfigInputAlias>>, max_connections: u16) -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections,
            method: InputFetchMethod::default(),
            aliases,
            ..ConfigInput::default()
        });

        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            ffprobe_available: Arc::default(),
        }
    }

    fn create_test_app_config_with_dual_provider_pool() -> AppConfig {
        build_test_app_config(
            Some(vec![ConfigInputAlias {
                id: 2,
                name: "provider_2".intern(),
                url: "http://provider-2.example".to_string(),
                username: Some("user2".to_string()),
                password: Some("pass2".to_string()),
                priority: 1,
                max_connections: 1,
                exp_date: None,
                enabled: true,
            }]),
            1,
        )
    }

    fn create_test_app_config_single_provider_pool() -> AppConfig { build_test_app_config(None, 1) }

    #[tokio::test]
    async fn test_force_exact_acquire_does_not_overallocate_busy_provider() {
        let app_cfg = create_test_app_config_with_dual_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let client_1_addr: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        let client_2_addr: SocketAddr = "127.0.0.1:40002".parse().unwrap();

        let first_alloc =
            manager.acquire_connection(&input_name, &client_1_addr, default_user_priority()).await.expect("client1 initial allocation");
        let pinned_provider = first_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(pinned_provider.as_ref(), "provider_1");

        // provider_1 has max_connections=1 and is already in use by client1
        let forced = manager.force_exact_acquire_connection(&pinned_provider, &client_2_addr, default_user_priority()).await;
        assert!(forced.is_none(), "forced exact acquire must not over-allocate busy provider");

        manager.release_connection(&client_1_addr).await;
        manager.release_connection(&client_2_addr).await;
    }

    #[tokio::test]
    async fn test_force_session_fallback_uses_different_provider_when_current_is_busy() {
        let app_cfg = create_test_app_config_with_dual_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let client_1_addr: SocketAddr = "127.0.0.1:41001".parse().unwrap();
        let client_2_addr: SocketAddr = "127.0.0.1:41002".parse().unwrap();

        // Step 1: Client1 starts movie -> provider_1
        let first_alloc =
            manager.acquire_connection(&input_name, &client_1_addr, default_user_priority()).await.expect("client1 initial allocation");
        assert_eq!(first_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Step 2: Client1 stops -> release provider_1
        manager.release_connection(&client_1_addr).await;

        // Step 3: Client2 starts live -> provider_1
        let live_alloc =
            manager.acquire_connection(&input_name, &client_2_addr, default_user_priority()).await.expect("client2 live allocation");
        let busy_provider = live_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(busy_provider.as_ref(), input_name.as_ref());
        assert!(manager.is_exhausted(&busy_provider).await);

        // Step 4: Client1 restarts same movie.
        // This emulates force-session fallback path by acquiring without provider grace.
        let fallback_alloc = manager
            .acquire_connection_with_grace(&input_name, &client_1_addr, false, 0)
            .await
            .expect("client1 fallback allocation without grace");
        let fallback_provider = fallback_alloc.allocation.get_provider_name().expect("fallback provider expected");

        assert_ne!(fallback_provider.as_ref(), busy_provider.as_ref());
        assert_eq!(fallback_provider.as_ref(), "provider_2");

        manager.release_connection(&client_1_addr).await;
        manager.release_connection(&client_2_addr).await;
    }

    #[tokio::test]
    async fn test_seek_reacquire_stays_on_same_provider_account_until_stop() {
        let app_cfg = create_test_app_config_with_dual_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let client_1_addr: SocketAddr = "127.0.0.1:42001".parse().unwrap();
        let client_2_addr: SocketAddr = "127.0.0.1:42002".parse().unwrap();

        // Initial playback for client1.
        let first_alloc =
            manager.acquire_connection(&input_name, &client_1_addr, default_user_priority()).await.expect("client1 initial allocation");
        let pinned_provider = first_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(pinned_provider.as_ref(), "provider_1");

        // Another client occupies the alternate account while client1 keeps seeking.
        let second_alloc = manager.acquire_connection(&input_name, &client_2_addr, default_user_priority()).await.expect("client2 allocation");
        let second_provider = second_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(second_provider.as_ref(), "provider_2");

        // Simulate repeated seek/range reconnects for client1:
        // release old connection for the same client, then force exact pinned provider.
        for _ in 0..3 {
            manager.release_connection(&client_1_addr).await;
            let seek_alloc = manager
                .force_exact_acquire_connection(&pinned_provider, &client_1_addr, default_user_priority())
                .await
                .expect("seek reacquire should stay on pinned provider");
            let seek_provider = seek_alloc.allocation.get_provider_name().expect("provider name expected");
            assert_eq!(seek_provider.as_ref(), pinned_provider.as_ref());
        }

        // Stream stop / cleanup.
        manager.release_connection(&client_1_addr).await;
        manager.release_connection(&client_2_addr).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_probe_preemption_releases_capacity_immediately_and_cancels_after_grace() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let user_addr: SocketAddr = "127.0.0.1:43001".parse().unwrap();

        let probe_handle =
            manager.acquire_connection_for_probe(&input_name, default_probe_user_priority()).await.expect("probe allocation should succeed");
        let probe_token = probe_handle.cancel_token.clone().expect("probe handle must carry cancel token");

        // User request should preempt probe and immediately acquire released capacity.
        let user_alloc = manager
            .acquire_connection_with_grace(&input_name, &user_addr, false, default_user_priority())
            .await
            .expect("user allocation should preempt probe");
        assert_eq!(user_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Let the detached cancellation task start and arm its sleep timer on the paused clock.
        tokio::task::yield_now().await;

        // Probe cancellation is intentionally delayed by a small grace window.
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(!probe_token.is_cancelled(), "probe token should not be cancelled immediately");

        let cancel_wait_timeout = super::PREEMPTED_PROBE_CANCEL_GRACE + Duration::from_millis(500);
        let wait_token = probe_token.clone();
        let cancel_wait =
            tokio::spawn(async move { tokio::time::timeout(cancel_wait_timeout, wait_token.cancelled()).await });
        tokio::task::yield_now().await;
        tokio::time::advance(cancel_wait_timeout).await;
        assert!(
            cancel_wait.await.expect("cancel wait task should join").is_ok(),
            "probe token should be cancelled before timeout after grace"
        );

        manager.release_connection(&user_addr).await;
    }

    #[tokio::test]
    async fn test_higher_priority_user_preempts_lower_priority_user() {
        // User with priority 5 (low) is connected; user with priority -1 (high) arrives.
        // The low-priority user should be preempted.
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let low_prio_addr: SocketAddr = "127.0.0.1:44001".parse().unwrap();
        let high_prio_addr: SocketAddr = "127.0.0.1:44002".parse().unwrap();

        // Low-priority user connects (priority 5 = lower importance)
        let low_alloc = manager
            .acquire_connection(&input_name, &low_prio_addr, 5)
            .await
            .expect("low-priority user should get connection");
        assert_eq!(low_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name).await);

        // High-priority user arrives (priority -1 = higher importance), should preempt low-priority user
        let high_alloc = manager
            .acquire_connection_with_grace(&input_name, &high_prio_addr, false, -1)
            .await
            .expect("high-priority user should preempt low-priority user and get connection");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        manager.release_connection(&high_prio_addr).await;
    }

    #[tokio::test]
    async fn test_same_priority_user_does_not_preempt() {
        // Two users with the same priority — new one should NOT preempt the existing one.
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let user_1_addr: SocketAddr = "127.0.0.1:45001".parse().unwrap();
        let user_2_addr: SocketAddr = "127.0.0.1:45002".parse().unwrap();

        // User 1 connects with priority 0
        let alloc1 = manager
            .acquire_connection(&input_name, &user_1_addr, default_user_priority())
            .await
            .expect("user1 should get connection");
        assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name).await);

        // User 2 arrives with the same priority 0 — should NOT preempt user 1
        let alloc2 = manager.acquire_connection_with_grace(&input_name, &user_2_addr, false, default_user_priority()).await;
        assert!(alloc2.is_none(), "same-priority user should not preempt existing user");

        manager.release_connection(&user_1_addr).await;
    }

    #[tokio::test]
    async fn test_lower_priority_user_does_not_preempt_higher_priority_user() {
        // User with high priority is connected; user with low priority arrives — should NOT preempt.
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let high_prio_addr: SocketAddr = "127.0.0.1:46001".parse().unwrap();
        let low_prio_addr: SocketAddr = "127.0.0.1:46002".parse().unwrap();

        // High-priority user connects (priority -10)
        let alloc1 = manager
            .acquire_connection(&input_name, &high_prio_addr, -10)
            .await
            .expect("high-priority user should get connection");
        assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name).await);

        // Low-priority user arrives (priority 10) — should NOT preempt high-priority user
        let alloc2 = manager.acquire_connection_with_grace(&input_name, &low_prio_addr, false, 10).await;
        assert!(alloc2.is_none(), "low-priority user should not preempt high-priority user");

        manager.release_connection(&high_prio_addr).await;
    }

    #[tokio::test]
    async fn test_grace_period_triggers_preemption_of_lower_priority() {
        // Provider full, high-prio user arrives with grace allowed,
        // low-prio victim should be evicted and provider should not be over limit.
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let low_prio_addr: SocketAddr = "127.0.0.1:47001".parse().unwrap();
        let high_prio_addr: SocketAddr = "127.0.0.1:47002".parse().unwrap();

        // Low-priority user connects (priority 20 = low importance)
        let low_alloc = manager
            .acquire_connection(&input_name, &low_prio_addr, 20)
            .await
            .expect("low-priority user should get connection");
        assert_eq!(low_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        let low_token = low_alloc.cancel_token.clone().expect("must have cancel token");

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name).await);

        // High-priority user arrives WITH grace allowed (default streaming path)
        // This should get a GracePeriod allocation and then evict the low-prio user
        let high_alloc = manager
            .acquire_connection(&input_name, &high_prio_addr, 0)
            .await
            .expect("high-priority user should get grace allocation and evict low-prio");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Low-priority user's cancel token should be cancelled
        assert!(low_token.is_cancelled(), "low-prio user should be cancelled after eviction");

        // Provider should not be over limit (eviction freed a slot)
        assert!(!manager.is_over_limit(&input_name).await, "provider should not be over limit after eviction");

        manager.release_connection(&high_prio_addr).await;
    }

    #[tokio::test]
    async fn test_grace_period_no_victim_keeps_grace_behavior() {
        // Provider full, same-prio user arrives with grace allowed,
        // no victim available → normal grace behavior (allocation still returned).
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let user_1_addr: SocketAddr = "127.0.0.1:48001".parse().unwrap();
        let user_2_addr: SocketAddr = "127.0.0.1:48002".parse().unwrap();

        // User 1 connects with priority 0
        let alloc1 = manager
            .acquire_connection(&input_name, &user_1_addr, default_user_priority())
            .await
            .expect("user1 should get connection");
        assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        let token1 = alloc1.cancel_token.clone().expect("must have cancel token");

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name).await);

        // User 2 arrives with same priority and grace allowed
        // Should get a grace allocation but NOT evict user 1
        let alloc2 = manager
            .acquire_connection(&input_name, &user_2_addr, default_user_priority())
            .await;

        // Grace allocation should be returned (provider allows grace)
        if let Some(alloc2) = &alloc2 {
            assert_eq!(alloc2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        }
        // User 1 should NOT be cancelled
        assert!(!token1.is_cancelled(), "same-prio user should not be evicted");

        manager.release_connection(&user_1_addr).await;
        manager.release_connection(&user_2_addr).await;
    }

    #[tokio::test]
    async fn test_btree_index_consistent_after_lifecycle() {
        // Verify that the priority_index stays consistent through add, evict, release.
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let addr_a: SocketAddr = "127.0.0.1:49001".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:49002".parse().unwrap();

        // Add connection A (low priority)
        let alloc_a = manager
            .acquire_connection(&input_name, &addr_a, 10)
            .await
            .expect("alloc_a");

        // Check index has 1 entry
        {
            let connections = manager.connections.read().await;
            let tree = connections.priority_index.get(&input_name).expect("index for provider_1");
            assert_eq!(tree.len(), 1, "index should have 1 entry after first allocation");
        }

        // High-priority user evicts low-priority via grace path
        let alloc_b = manager
            .acquire_connection(&input_name, &addr_b, -5)
            .await
            .expect("alloc_b should evict alloc_a");

        // Check index: should have 1 entry (alloc_a evicted, alloc_b added)
        {
            let connections = manager.connections.read().await;
            let tree = connections.priority_index.get(&input_name).expect("index for provider_1");
            assert_eq!(tree.len(), 1, "index should have 1 entry after eviction + new allocation");
            // The remaining entry should be alloc_b
            let ((prio, _, _), _) = tree.iter().next().expect("one entry");
            assert_eq!(*prio, -5, "remaining entry should be the high-prio connection");
        }

        // Release alloc_b
        manager.release_handle(&alloc_b).await;

        // Check index: should be empty
        {
            let connections = manager.connections.read().await;
            let tree = connections.priority_index.get(&input_name);
            let is_empty = tree.is_none_or(|t| t.is_empty());
            assert!(is_empty, "index should be empty after releasing all connections");
        }

        // Verify alloc_a handle can be safely released (already evicted - no-op)
        manager.release_handle(&alloc_a).await;
    }
}
