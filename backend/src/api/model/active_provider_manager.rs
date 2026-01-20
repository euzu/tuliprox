use crate::api::model::provider_lineup_manager::{ProviderAllocation, ProviderLineupManager};
use crate::api::model::{EventManager, ProviderConfig};
use crate::model::{AppConfig, ConfigInput};
use shared::utils::{default_grace_period_millis, default_grace_period_timeout_secs, sanitize_sensitive_info};
use log::{error};
use crate::utils::{debug_if_enabled, trace_if_enabled};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

pub type ClientConnectionId = SocketAddr;
type AllocationId = u64;

#[derive(Debug, Clone)]
pub struct ProviderHandle {
    pub client_id: ClientConnectionId,
    pub allocation_id: AllocationId,
    pub allocation: ProviderAllocation,
}

impl ProviderHandle {
    pub fn new(client_id: ClientConnectionId, allocation_id: AllocationId, allocation: ProviderAllocation) -> Self {
        Self {
            client_id,
            allocation_id,
            allocation,
        }
    }
}

#[derive(Debug, Clone)]
struct SharedAllocation {
    allocation_id: AllocationId,
    allocation: ProviderAllocation,
    connections: HashSet<ClientConnectionId>,
}

#[derive(Debug, Clone, Default)]
struct SharedConnections {
    by_key: HashMap<String, SharedAllocation>,
    key_by_addr: HashMap<ClientConnectionId, String>,
}

#[derive(Debug, Clone, Default)]
struct Connections {
    single: HashMap<ClientConnectionId, HashMap<AllocationId, ProviderAllocation>>,
    shared: SharedConnections,
}

pub struct ActiveProviderManager {
    providers: ProviderLineupManager,
    connections: RwLock<Connections>,
    next_allocation_id: AtomicU64,
}

impl ActiveProviderManager {
    pub fn new(cfg: &AppConfig, event_manager: &Arc<EventManager>) -> Self {
        let (grace_period_millis, grace_period_timeout_secs) = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        Self {
            providers: ProviderLineupManager::new(inputs, grace_period_millis, grace_period_timeout_secs, event_manager),
            connections: RwLock::new(Connections::default()),
            next_allocation_id: AtomicU64::new(1),
        }
    }

    fn get_config_inputs(cfg: &AppConfig) -> Vec<Arc<ConfigInput>> {
        cfg.sources.load().inputs.iter().map(Arc::clone).collect()
    }

    fn get_grace_options(cfg: &AppConfig) -> (u64, u64) {
        let (grace_period_millis, grace_period_timeout_secs) = cfg.config.load().reverse_proxy.as_ref()
            .and_then(|r| r.stream.as_ref())
            .map_or_else(|| (default_grace_period_millis(), default_grace_period_timeout_secs()), |s| (s.grace_period_millis, s.grace_period_timeout_secs));
        (grace_period_millis, grace_period_timeout_secs)
    }

    pub async fn update_config(&self, cfg: &AppConfig) {
        let (grace_period_millis, grace_period_timeout_secs) = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        self.providers.update_config(inputs, grace_period_millis, grace_period_timeout_secs);
        self.reconcile_connections().await;
    }

    pub async fn reconcile_connections(&self) {
        let mut counts = HashMap::<Arc<str>, usize>::new();
        {
            let connections = self.connections.read().await;

            // Single connections
            for per_addr in connections.single.values() {
                for allocation in per_addr.values() {
                    if let Some(name) = allocation.get_provider_name() {
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
    ) -> Option<ProviderHandle> {
        // Call the specific acquisition function
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

        match &allocation {
            ProviderAllocation::Exhausted => {}
            ProviderAllocation::Available(_) | ProviderAllocation::GracePeriod(_) => {
                let provider_name = allocation.get_provider_name().unwrap_or_default();
                let allocation_id = self.next_allocation_id.fetch_add(1, Ordering::Relaxed);
                let mut connections = self.connections.write().await;
                let per_addr = connections.single.entry(*addr).or_default();
                if !per_addr.is_empty() {
                    trace_if_enabled!(
                        "register_connection: address {addr} already has {} provider allocations",
                        per_addr.len()
                    );
                }
                per_addr.insert(allocation_id, allocation.clone());
                 debug_if_enabled!("Added provider connection {provider_name:?} for {}", sanitize_sensitive_info(&addr.to_string()));
                return Some(ProviderHandle::new(*addr, allocation_id, allocation));
            }
        }

        None
    }

    pub async fn force_exact_acquire_connection(&self, provider_name: &Arc<str>, addr: &SocketAddr) -> Option<ProviderHandle> {
        self.acquire_connection_inner(provider_name, addr, true, None).await
    }

    // Returns the next available provider connection
    pub async fn acquire_connection(&self, input_name: &Arc<str>, addr: &SocketAddr) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, None).await
    }

    /// Acquire a provider connection while optionally disabling provider grace allocations.
    pub async fn acquire_connection_with_grace_override(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, Some(allow_grace)).await
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
        // Single connection
        let single_allocations = {
            let mut connections = self.connections.write().await;
            connections.single.remove(addr)
        };

        if let Some(allocations) = single_allocations {
            for (_id, allocation) in allocations {
              debug_if_enabled!(
                  "Released provider connection {:?} for {}",
                  allocation.get_provider_name().unwrap_or_default(),
                  sanitize_sensitive_info(&addr.to_string())
                );
                allocation.release().await;
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
            if let Some(per_addr) = connections.single.get_mut(&handle.client_id) {
                released = per_addr.remove(&handle.allocation_id);
                if per_addr.is_empty() {
                    connections.single.remove(&handle.client_id);
                }
            }

            if released.is_none() {
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
            let handle = connections.single.remove(addr).and_then(|m| {
                if m.is_empty() {
                    return None;
                }
                let mut iter = m.into_iter();
                let (id, allocation) = iter.next().expect("non-empty map");
                for (_extra_id, extra_alloc) in iter {
                    extras.push(extra_alloc);
                }
                if !extras.is_empty() {
                    trace_if_enabled!(
                        "Shared connection promotion expects a single allocation for {addr}, found {}",
                        extras.len() + 1
                    );
                }
                Some(ProviderHandle::new(*addr, id, allocation))
            });

            if let Some(handle) = &handle {
                let provider_name = handle.allocation.get_provider_name().unwrap_or_default();
                debug_if_enabled!(
                    "Shared connection: promoted addr {addr} provider={} key={}",
                    sanitize_sensitive_info(&provider_name),
                    sanitize_sensitive_info(key)
                );
                connections.shared.by_key.insert(
                    key.to_string(),
                    SharedAllocation {
                        allocation_id: handle.allocation_id,
                        allocation: handle.allocation.clone(),
                        connections: HashSet::from([*addr]),
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
