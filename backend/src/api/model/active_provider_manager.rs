use crate::api::model::provider_lineup_manager::{ProviderAllocation, ProviderLineupManager};
use crate::api::model::{EventManager, ProviderConfig};
use crate::model::{AppConfig, ConfigInput, GracePeriodOptions};
use crate::utils::debug_if_enabled;
use log::error;
use shared::utils::sanitize_sensitive_info;
// trace_if_enabled removed
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const DEFAULT_FORCE_PRIORITY: i8 = -128;
const DEFAULT_USER_PRIORITY: i8 = -1;
const DEFAULT_PROBE_PRIORITY: i8 = 1;
static DUMMY_ADDR: LazyLock<SocketAddr> = LazyLock::new(|| "127.0.0.1:0".parse::<SocketAddr>().unwrap());

pub type ClientConnectionId = SocketAddr;
type AllocationId = u64;

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
        Self {
            client_id,
            allocation_id,
            allocation,
            cancel_token,
        }
    }
}

#[derive(Debug, Clone)]
struct SharedAllocation {
    allocation_id: AllocationId,
    allocation: ProviderAllocation,
    connections: HashSet<ClientConnectionId>,
    priority: i8,
}

#[derive(Debug, Clone)]
struct ActiveConnectionInfo {
    allocation: ProviderAllocation,
    priority: i8,
    // Used to signal preemption to the consumer of this connection
    cancel_token: CancellationToken,
    created_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct SharedConnections {
    by_key: HashMap<String, SharedAllocation>,
    key_by_addr: HashMap<ClientConnectionId, String>,
}

#[derive(Debug, Clone, Default)]
struct Connections {
    // Map Addr -> AllocationID -> Allocation Info
    single: HashMap<ClientConnectionId, HashMap<AllocationId, ActiveConnectionInfo>>,
    shared: SharedConnections,
    // Index to quickly find connections by provider name for preemption
    // ProviderName -> Vec<(ClientConnectionId, AllocationId)>
    by_provider: HashMap<Arc<str>, Vec<(ClientConnectionId, AllocationId)>>,
}

pub struct ActiveProviderManager {
    providers: ProviderLineupManager,
    connections: RwLock<Connections>,
    next_allocation_id: AtomicU64,
}

impl ActiveProviderManager {
    pub fn new(cfg: &AppConfig, event_manager: &Arc<EventManager>) -> Self {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        Self {
            providers: ProviderLineupManager::new(inputs, grace_period_options, event_manager),
            connections: RwLock::new(Connections::default()),
            next_allocation_id: AtomicU64::new(1),
        }
    }

    fn get_config_inputs(cfg: &AppConfig) -> Vec<Arc<ConfigInput>> {
        cfg.sources.load().inputs.iter().filter(|i| i.enabled).map(Arc::clone).collect()
    }

    fn get_grace_options(cfg: &AppConfig) -> GracePeriodOptions {
        cfg.config.load().get_grace_options()
    }

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
    ) -> Option<ProviderHandle> {
        // 1. Try to acquire directly
        let allocation = if force {
            self.providers.force_exact_acquire_connection(provider_or_input_name).await
        } else {
            match allow_grace_override {
                Some(allow_grace) => {
                    self.providers
                        .acquire_connection_with_grace_override(provider_or_input_name, allow_grace)
                        .await
                }
                None => self.providers.acquire_connection(provider_or_input_name).await,
            }
        };

        if !matches!(allocation, ProviderAllocation::Exhausted) {
            return self.register_allocation(allocation, addr, priority).await;
        }

        // 2. If exhausted, try preemption (kick lower priority connection)
        if !force {
            if let Some(preempted_alloc) = self.try_preempt_connection(provider_or_input_name, priority).await {
                return self.register_allocation(preempted_alloc, addr, priority).await;
            }
        }

        None
    }

    async fn register_allocation(&self, allocation: ProviderAllocation, addr: &SocketAddr, priority: i8) -> Option<ProviderHandle> {
        let provider_name = allocation.get_provider_name().unwrap_or_default();
        let allocation_id = self.next_allocation_id.fetch_add(1, Ordering::Relaxed);
        let cancel_token = CancellationToken::new();

        let mut connections = self.connections.write().await;
        let per_addr = connections.single.entry(*addr).or_default();

        per_addr.insert(allocation_id, ActiveConnectionInfo {
            allocation: allocation.clone(),
            priority,
            cancel_token: cancel_token.clone(),
            created_at: Instant::now(),
        });

        connections.by_provider.entry(provider_name.clone())
            .or_default()
            .push((*addr, allocation_id));

        debug_if_enabled!("Added provider connection {provider_name:?} for {} (prio={})", sanitize_sensitive_info(&addr.to_string()), priority);
        Some(ProviderHandle::new(*addr, allocation_id, allocation, Some(cancel_token)))
    }

    async fn try_preempt_connection(&self, input_name: &Arc<str>, new_priority: i8) -> Option<ProviderAllocation> {
        // Victim: (addr, alloc_id, priority, created_at, is_shared)
        let mut victim: Option<(ClientConnectionId, AllocationId, i8, Instant, bool)> = None;

        {
            let connections = self.connections.read().await;

            for (prov_name, active_conns) in &connections.by_provider {
                if self.providers.is_provider_for_input(prov_name, input_name) {
                    for (addr, alloc_id) in active_conns {
                        // Check single connections
                        if let Some(conns_map) = connections.single.get(addr) {
                            if let Some(info) = conns_map.get(alloc_id) {
                                if info.priority > new_priority {
                                    let is_better = match victim {
                                        None => true,
                                        Some((_, _, v_prio, v_created, _)) => {
                                            info.priority > v_prio || (info.priority == v_prio && info.created_at < v_created)
                                        }
                                    };
                                    if is_better {
                                        victim = Some((*addr, *alloc_id, info.priority, info.created_at, false));
                                    }
                                }
                            }
                        }

                        // Check shared connections (only if exactly 1 listener)
                        if let Some(key) = connections.shared.key_by_addr.get(addr) {
                            if let Some(shared) = connections.shared.by_key.get(key) {
                                if shared.allocation_id == *alloc_id
                                    && shared.connections.len() == 1
                                    && shared.priority > new_priority
                                {
                                    let is_better = match victim {
                                        None => true,
                                        Some((_, _, v_prio, _, _)) => shared.priority > v_prio,
                                    };
                                    if is_better {
                                        victim = Some((*addr, *alloc_id, shared.priority, Instant::now(), true));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some((addr, alloc_id, v_prio, _, is_shared)) = victim {
            debug_if_enabled!("Preempting {} connection from {} (prio={}) for higher priority request (prio={})", 
                if is_shared { "shared" } else { "single" },
                sanitize_sensitive_info(&addr.to_string()), v_prio, new_priority);

            // Get cancel token (only for single connections)
            let cancel_token = if is_shared {
                None
            } else {
                let connections = self.connections.read().await;
                connections.single.get(&addr)
                    .and_then(|map| map.get(&alloc_id))
                    .map(|info| info.cancel_token.clone())
            };

            if let Some(token) = cancel_token {
                token.cancel();
            }

            // Release the connection
            let handle = ProviderHandle {
                client_id: addr,
                allocation_id: alloc_id,
                allocation: ProviderAllocation::Exhausted,
                cancel_token: None,
            };
            self.release_handle(&handle).await;

            // Now try acquire again
            let allocation = self.providers.acquire_connection(input_name).await;
            if !matches!(allocation, ProviderAllocation::Exhausted) {
                return Some(allocation);
            }
        }

        None
    }

    pub async fn force_exact_acquire_connection(&self, provider_name: &Arc<str>, addr: &SocketAddr) -> Option<ProviderHandle> {
        // Force acquire always uses max priority (DEFAULT_FORCE_PRIORITY) effectively, but here we just pass DEFAULT_FORCE_PRIORITY to be safe,
        // though `force` param in inner overrides checks anyway.
        self.acquire_connection_inner(provider_name, addr, true, None, DEFAULT_FORCE_PRIORITY).await
    }

    // Returns the next available provider connection
    pub async fn acquire_connection(&self, input_name: &Arc<str>, addr: &SocketAddr) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, None, DEFAULT_USER_PRIORITY).await
    }

    /// Acquire a provider connection while optionally disabling provider grace allocations.
    pub async fn acquire_connection_for_probe(
        &self,
        input_name: &Arc<str>,
    ) -> Option<ProviderHandle> {
        // Probe is strictly low-priority and must never consume grace capacity.
        self.acquire_connection_inner(input_name, &DUMMY_ADDR, false, Some(false), DEFAULT_PROBE_PRIORITY).await
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

    pub async fn release_connection(&self, addr: &SocketAddr) {
        // Single connection - all index updates in one lock scope
        let single_allocations = {
            let mut connections = self.connections.write().await;
            if let Some(allocations) = connections.single.remove(addr) {
                // Remove from by_provider index while still holding the lock
                for (id, info) in &allocations {
                    if let Some(name) = info.allocation.get_provider_name() {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            if let Some(idx) = list.iter().position(|(a, i)| *a == *addr && *i == *id) {
                                list.remove(idx);
                            }
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
        {
            let mut connections = self.connections.write().await;

            // Try removing from Single
            if let Some(per_addr) = connections.single.get_mut(&handle.client_id) {
                if let Some(info) = per_addr.remove(&handle.allocation_id) {
                    released = Some(info.allocation);
                    if per_addr.is_empty() {
                        connections.single.remove(&handle.client_id);
                    }

                    // Remove from by_provider index
                    if let Some(name) = released.as_ref().and_then(ProviderAllocation::get_provider_name) {
                        if let Some(list) = connections.by_provider.get_mut(&name) {
                            if let Some(idx) = list.iter().position(|(a, i)| *a == handle.client_id && *i == handle.allocation_id) {
                                list.remove(idx);
                            }
                        }
                    }
                }
            }

            if released.is_none() {
                // Try removing from Shared
                let mut remove_key: Option<String> = None;
                // TODO O(n) over all keys, maybe better approach ist to use a Hashmap shared_by_allocation_id: HashMap<AllocationId, String>
                for (key, shared) in &connections.shared.by_key {
                    if shared.allocation_id == handle.allocation_id {
                        remove_key = Some(key.clone());
                        break;
                    }
                }

                if let Some(key) = remove_key {
                    if let Some(shared) = connections.shared.by_key.remove(&key) {
                        released = Some(shared.allocation);
                        for addr in shared.connections {
                            connections.shared.key_by_addr.remove(&addr);
                        }
                    }
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
                        for (_, extra_info) in iter {
                            extras.push(extra_info.allocation);
                        }

                        // Cleanup indices
                        if let Some(name) = info.allocation.get_provider_name() {
                            if let Some(list) = connections.by_provider.get_mut(&name) {
                                list.retain(|(a, _)| *a != *addr);
                            }
                        }

                        connections.single.remove(addr); // Map is drained/empty now

                        Some(ProviderHandle::new(*addr, id, info.allocation, Some(info.cancel_token)))
                    } else {
                        None
                    }
                }
            } else { None };


            if let Some(handle) = &handle {
                let provider_name = handle.allocation.get_provider_name().unwrap_or_default();
                debug_if_enabled!(
                    "Shared connection: promoted addr {addr} provider={} key={}",
                    sanitize_sensitive_info(&provider_name),
                    sanitize_sensitive_info(key)
                );
                // Get priority from the original single connection info
                let priority = connections.single.get(addr)
                    .and_then(|m| m.values().next())
                    .map_or(DEFAULT_USER_PRIORITY, |info| info.priority);

                connections.shared.by_key.insert(
                    key.to_string(),
                    SharedAllocation {
                        allocation_id: handle.allocation_id,
                        allocation: handle.allocation.clone(),
                        connections: HashSet::from([*addr]),
                        priority,
                    },
                );
                connections.shared.key_by_addr.insert(*addr, key.to_string());
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
            error!(
                "Failed to add shared connection for {addr}: url {} not found",
                sanitize_sensitive_info(key)
            );
        }
    }

    pub async fn get_provider_connections_count(&self) -> usize {
        self.providers.active_connection_count().await
    }
}
