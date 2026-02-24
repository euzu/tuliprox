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
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

const DEFAULT_USER_PRIORITY: i8 = -1;
const DEFAULT_PROBE_PRIORITY: i8 = 1;
const PREEMPTED_PROBE_CANCEL_GRACE: Duration = Duration::from_secs(2);
const PREEMPTED_GRACE_MAX_PENDING: usize = 64;
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
    is_probe: bool,
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
        is_probe: bool,
    ) -> Option<ProviderHandle> {
        // 1. Try to acquire directly
        let (allow_grace, allocation) = if force {
            (
                true,
                self.providers
                    .force_exact_acquire_connection(provider_or_input_name)
                    .await,
            )
        } else {
            match allow_grace_override {
                Some(allow_grace) => (
                    allow_grace,
                    self.providers
                        .acquire_connection_with_grace_override(
                            provider_or_input_name,
                            allow_grace,
                        )
                        .await,
                ),
                None => (
                    true,
                    self.providers.acquire_connection(provider_or_input_name).await,
                ),
            }
        };

        if !matches!(allocation, ProviderAllocation::Exhausted) {
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

        let mut connections = self.connections.write().await;
        let per_addr = connections.single.entry(*addr).or_default();

        per_addr.insert(allocation_id, ActiveConnectionInfo {
            allocation: allocation.clone(),
            priority,
            is_probe,
            cancel_token: cancel_token.clone(),
            created_at: Instant::now(),
        });

        connections.by_provider.entry(provider_name.clone())
            .or_default()
            .push((*addr, allocation_id));

        debug_if_enabled!("Added provider connection {provider_name:?} for {} (prio={})", sanitize_sensitive_info(&addr.to_string()), priority);
        Some(ProviderHandle::new(*addr, allocation_id, allocation, Some(cancel_token)))
    }

    async fn try_preempt_connection(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
        allow_grace: bool,
    ) -> Option<ProviderAllocation> {
        // Victim: (addr, alloc_id, priority, created_at, is_shared, is_probe_connection)
        let mut victim: Option<(ClientConnectionId, AllocationId, i8, Instant, bool, bool)> = None;

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
                                        Some((_, _, v_prio, v_created, _, _)) => {
                                            info.priority > v_prio || (info.priority == v_prio && info.created_at < v_created)
                                        }
                                    };
                                    if is_better {
                                        victim = Some((
                                            *addr,
                                            *alloc_id,
                                            info.priority,
                                            info.created_at,
                                            false,
                                            info.is_probe,
                                        ));
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
                                        Some((_, _, v_prio, _, _, _)) => shared.priority > v_prio,
                                    };
                                    if is_better {
                                        victim = Some((*addr, *alloc_id, shared.priority, Instant::now(), true, false));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some((addr, alloc_id, v_prio, _, is_shared, is_probe_connection)) = victim {
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
                if is_probe_connection {
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
            }

            // Release the connection
            let handle = ProviderHandle {
                client_id: addr,
                allocation_id: alloc_id,
                allocation: ProviderAllocation::Exhausted,
                cancel_token: None,
            };
            self.release_handle(&handle).await;

            // Now try acquire again preserving the original grace policy.
            let allocation = self
                .providers
                .acquire_connection_with_grace_override(input_name, allow_grace)
                .await;
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
    ) -> Option<ProviderHandle> {
        let allocation = self
            .providers
            .acquire_exact_connection_with_grace_override(provider_name, allow_grace)
            .await;
        if matches!(allocation, ProviderAllocation::Exhausted) {
            return None;
        }
        self.register_allocation(allocation, addr, DEFAULT_USER_PRIORITY, false).await
    }

    pub async fn force_exact_acquire_connection(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
    ) -> Option<ProviderHandle> {
        // Compatibility wrapper: keep the exact-provider behavior but do not over-allocate exhausted accounts.
        self.acquire_exact_connection_with_grace(provider_name, addr, false).await
    }

    // Returns the next available provider connection
    pub async fn acquire_connection(&self, input_name: &Arc<str>, addr: &SocketAddr) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, None, DEFAULT_USER_PRIORITY, false).await
    }

    /// Acquire a provider connection while explicitly controlling provider-side grace allocations.
    pub async fn acquire_connection_with_grace(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, Some(allow_grace), DEFAULT_USER_PRIORITY, false)
            .await
    }

    /// Acquire a provider connection while optionally disabling provider grace allocations.
    pub async fn acquire_connection_for_probe(
        &self,
        input_name: &Arc<str>,
    ) -> Option<ProviderHandle> {
        // Probe is strictly low-priority and must never consume grace capacity.
        self.acquire_connection_inner(input_name, &DUMMY_ADDR, false, Some(false), DEFAULT_PROBE_PRIORITY, true)
            .await
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

#[cfg(test)]
mod tests {
    use super::ActiveProviderManager;
    use crate::api::model::EventManager;
    use crate::model::{AppConfig, Config, ConfigInput, ConfigInputAlias, SourcesConfig};
    use crate::utils::FileLockManager;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{ConfigPaths, InputFetchMethod, InputType};
    use shared::utils::Internable;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

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

        let sources = SourcesConfig {
            inputs: vec![input],
            ..SourcesConfig::default()
        };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                config_path: String::new(),
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

    fn create_test_app_config_single_provider_pool() -> AppConfig {
        build_test_app_config(None, 1)
    }

    #[tokio::test]
    async fn test_force_exact_acquire_does_not_overallocate_busy_provider() {
        let app_cfg = create_test_app_config_with_dual_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let client_1_addr: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        let client_2_addr: SocketAddr = "127.0.0.1:40002".parse().unwrap();

        let first_alloc = manager
            .acquire_connection(&input_name, &client_1_addr)
            .await
            .expect("client1 initial allocation");
        let pinned_provider = first_alloc
            .allocation
            .get_provider_name()
            .expect("provider name expected");
        assert_eq!(pinned_provider.as_ref(), "provider_1");

        // provider_1 has max_connections=1 and is already in use by client1
        let forced = manager
            .force_exact_acquire_connection(&pinned_provider, &client_2_addr)
            .await;
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
        let first_alloc = manager.acquire_connection(&input_name, &client_1_addr).await.expect("client1 initial allocation");
        assert_eq!(
            first_alloc.allocation.get_provider_name().as_deref(),
            Some(input_name.as_ref())
        );

        // Step 2: Client1 stops -> release provider_1
        manager.release_connection(&client_1_addr).await;

        // Step 3: Client2 starts live -> provider_1
        let live_alloc = manager.acquire_connection(&input_name, &client_2_addr).await.expect("client2 live allocation");
        let busy_provider = live_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(busy_provider.as_ref(), input_name.as_ref());
        assert!(manager.is_exhausted(&busy_provider).await);

        // Step 4: Client1 restarts same movie.
        // This emulates force-session fallback path by acquiring without provider grace.
        let fallback_alloc = manager
            .acquire_connection_with_grace(&input_name, &client_1_addr, false)
            .await
            .expect("client1 fallback allocation without grace");
        let fallback_provider = fallback_alloc
            .allocation
            .get_provider_name()
            .expect("fallback provider expected");

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
        let first_alloc = manager
            .acquire_connection(&input_name, &client_1_addr)
            .await
            .expect("client1 initial allocation");
        let pinned_provider = first_alloc
            .allocation
            .get_provider_name()
            .expect("provider name expected");
        assert_eq!(pinned_provider.as_ref(), "provider_1");

        // Another client occupies the alternate account while client1 keeps seeking.
        let second_alloc = manager
            .acquire_connection(&input_name, &client_2_addr)
            .await
            .expect("client2 allocation");
        let second_provider = second_alloc
            .allocation
            .get_provider_name()
            .expect("provider name expected");
        assert_eq!(second_provider.as_ref(), "provider_2");

        // Simulate repeated seek/range reconnects for client1:
        // release old connection for the same client, then force exact pinned provider.
        for _ in 0..3 {
            manager.release_connection(&client_1_addr).await;
            let seek_alloc = manager
                .force_exact_acquire_connection(&pinned_provider, &client_1_addr)
                .await
                .expect("seek reacquire should stay on pinned provider");
            let seek_provider = seek_alloc
                .allocation
                .get_provider_name()
                .expect("provider name expected");
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

        let probe_handle = manager
            .acquire_connection_for_probe(&input_name)
            .await
            .expect("probe allocation should succeed");
        let probe_token = probe_handle
            .cancel_token
            .clone()
            .expect("probe handle must carry cancel token");

        // User request should preempt probe and immediately acquire released capacity.
        let user_alloc = manager
            .acquire_connection_with_grace(&input_name, &user_addr, false)
            .await
            .expect("user allocation should preempt probe");
        assert_eq!(
            user_alloc.allocation.get_provider_name().as_deref(),
            Some(input_name.as_ref())
        );

        // Probe cancellation is intentionally delayed by a small grace window.
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(!probe_token.is_cancelled(), "probe token should not be cancelled immediately");

        tokio::time::advance(super::PREEMPTED_PROBE_CANCEL_GRACE + Duration::from_millis(500)).await;
        probe_token.cancelled().await;

        manager.release_connection(&user_addr).await;
    }
}
