use crate::{
    api::model::{
        provider_lineup_manager::{ProviderAllocation, ProviderLineupManager},
        EventManager, ProviderConfig, SharedStreamManager,
    },
    model::{AppConfig, ConfigInput, GracePeriodOptions},
    utils::debug_if_enabled,
};
use log::error;
use shared::utils::sanitize_sensitive_info;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

static DUMMY_ADDR: LazyLock<SocketAddr> = LazyLock::new(|| SocketAddr::from(([127, 0, 0, 1], 0)));
pub type ClientConnectionId = SocketAddr;
type AllocationId = u64;
type SharedConnectionId = AllocationId;
type PreemptionCandidate = (PriorityOwner, AllocationId, i8, Instant);
// Key for BTreeMap priority index: (priority, Reverse<created_at>, AllocationId)
// Semantics: lower numeric priority value = higher importance (0 = highest, 127 = lowest).
// `.last()` on the BTreeMap returns the entry with the highest priority value, which is
// the lowest-importance connection and therefore the best eviction victim.
// Ties are broken by `Reverse<Instant>`: among equal priority values, the oldest connection
// (smallest `created_at`) sorts last and is evicted first.
type PriorityKey = (i8, Reverse<Instant>, AllocationId);

fn is_better_preemption_candidate(
    current: Option<PreemptionCandidate>,
    candidate: PreemptionCandidate,
) -> bool {
    match current {
        None => true,
        Some((_, current_allocation_id, current_priority, current_created_at)) => {
            candidate.2 > current_priority
                || (candidate.2 == current_priority
                    && (candidate.3 < current_created_at
                        || (candidate.3 == current_created_at && candidate.1 < current_allocation_id)))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PriorityOwner {
    Single(ClientConnectionId),
    Shared(SharedConnectionId),
}

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
    connections: HashMap<ClientConnectionId, i8>,
    priority: i8,
    created_at: Instant,
    cancel_token: Option<CancellationToken>,
}

#[derive(Debug, Clone)]
struct ActiveConnectionInfo {
    allocation: ProviderAllocation,
    // Used to signal preemption to the consumer of this connection
    cancel_token: CancellationToken,
    created_at: Instant,
    priority: i8,
}

#[derive(Debug, Clone)]
struct ProviderReservation {
    owner: Arc<str>,
    expires_at: TokioInstant,
}

#[derive(Debug, Clone, Default)]
struct SharedConnections {
    by_key: HashMap<Arc<str>, SharedAllocation>,
    key_by_addr: HashMap<ClientConnectionId, Arc<str>>,
    shared_by_allocation_id: HashMap<AllocationId, Arc<str>>,
}

#[derive(Debug, Clone, Default)]
struct Connections {
    // Map Addr -> AllocationID -> Allocation Info
    single: HashMap<ClientConnectionId, HashMap<AllocationId, ActiveConnectionInfo>>,
    shared: SharedConnections,
    // Index to quickly find connections by provider name for preemption
    // ProviderName -> Set<(ClientConnectionId, AllocationId)>
    by_provider: HashMap<Arc<str>, HashSet<(ClientConnectionId, AllocationId)>>,
    // Priority index per provider alias for O(log n) victim lookup
    // ProviderName -> BTreeMap<PriorityKey, PriorityOwner>
    priority_index: HashMap<Arc<str>, BTreeMap<PriorityKey, PriorityOwner>>,
}

pub struct ActiveProviderManager {
    providers: ProviderLineupManager,
    connections: RwLock<Connections>,
    reservations: RwLock<HashMap<Arc<str>, ProviderReservation>>,
    next_allocation_id: AtomicU64,
    shared_stream_manager: OnceLock<Arc<SharedStreamManager>>,
}

impl ActiveProviderManager {

    pub fn new(cfg: &AppConfig, event_manager: &Arc<EventManager>) -> Self {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        Self {
            providers: ProviderLineupManager::new(inputs, grace_period_options, event_manager),
            connections: RwLock::new(Connections::default()),
            reservations: RwLock::new(HashMap::new()),
            next_allocation_id: AtomicU64::new(1),
            shared_stream_manager: OnceLock::new(),
        }
    }

    pub fn set_shared_stream_manager(&self, manager: Arc<SharedStreamManager>) {
        let _ = self.shared_stream_manager.set(manager);
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

    fn prune_expired_reservations(reservations: &mut HashMap<Arc<str>, ProviderReservation>) {
        let now = TokioInstant::now();
        reservations.retain(|_, reservation| reservation.expires_at > now);
    }

    // Reservation ownership is grouped by the `fingerprint|username` prefix and intentionally drops the
    // trailing session/input suffix. That lets a client switch channels and immediately reuse the same
    // reserved provider account without waiting for the old channel reservation to expire.
    // Tradeoff: a new reservation for one channel clears any other reservation in the same family, so
    // concurrent multi-channel playback for the same client family is not supported by this grouping.
    // If concurrent streams must be supported, reservation matching must use the full owner identifier.
    fn reservation_family(owner: &str) -> &str {
        owner.rsplit_once('|').map_or(owner, |(family, _)| family)
    }

    // Compare reservations at the family level rather than exact owner equality so channel-switching can
    // take over an existing reservation across different `virtual_id`s for the same client family.
    // This shares the same tradeoff as `reservation_family`: same-family reservations replace each other.
    fn is_same_reservation_family(reservation_owner: &str, session_owner: &str) -> bool {
        Self::reservation_family(reservation_owner) == Self::reservation_family(session_owner)
    }

    async fn is_reserved_for_other(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        let mut reservations = self.reservations.write().await;
        Self::prune_expired_reservations(&mut reservations);
        reservations.get(provider_name).is_some_and(|reservation| {
            session_owner.is_none_or(|owner| !Self::is_same_reservation_family(reservation.owner.as_ref(), owner))
        })
    }

    async fn get_reserved_provider_for_owner(&self, input_name: &Arc<str>, session_owner: &str) -> Option<Arc<str>> {
        let mut reservations = self.reservations.write().await;
        Self::prune_expired_reservations(&mut reservations);
        reservations.iter().find_map(|(provider_name, reservation)| {
            if Self::is_same_reservation_family(reservation.owner.as_ref(), session_owner)
                && self.providers.is_provider_for_input(provider_name, input_name)
            {
                Some(provider_name.clone())
            } else {
                None
            }
        })
    }

    async fn reserved_provider_names_for_other(
        &self,
        input_name: &Arc<str>,
        session_owner: Option<&str>,
    ) -> HashSet<Arc<str>> {
        let mut reserved = HashSet::new();
        for provider_name in self.providers.provider_names_for_input(input_name) {
            if self.is_reserved_for_other(&provider_name, session_owner).await {
                reserved.insert(provider_name);
            }
        }
        reserved
    }

    pub async fn refresh_provider_reservation(&self, provider_name: &Arc<str>, session_owner: &str, ttl_secs: u64) {
        let mut reservations = self.reservations.write().await;
        Self::prune_expired_reservations(&mut reservations);
        // Refresh operates on the reservation family, not the exact owner string. This is what allows a
        // same-client channel switch to move the pinned provider account to the new stream immediately.
        // The tradeoff is that other reservations from the same family are cleared here as well.
        reservations.retain(|_, reservation| !Self::is_same_reservation_family(reservation.owner.as_ref(), session_owner));
        if ttl_secs == 0 {
            return;
        }
        reservations.insert(
            provider_name.clone(),
            ProviderReservation {
                owner: Arc::from(session_owner),
                expires_at: TokioInstant::now() + Duration::from_secs(ttl_secs),
            },
        );
    }

    async fn acquire_exact_connection_inner(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        if self.is_reserved_for_other(provider_name, session_owner).await {
            return None;
        }
        let allocation = self.providers.acquire_exact_connection_with_grace_override(provider_name, allow_grace).await;
        if matches!(allocation, ProviderAllocation::Exhausted) {
            return None;
        }
        self.register_allocation(allocation, addr, priority).await
    }

    async fn finalize_lineup_allocation(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        allocation: ProviderAllocation,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        if matches!(&allocation, ProviderAllocation::GracePeriod(_))
            && self.evict_lower_priority_on_input(input_name, priority, session_owner).await
        {
            let evicted_on_same = !self.providers.is_over_limit(&allocation.get_provider_name().unwrap_or_default()).await;
            if !evicted_on_same {
                let new_alloc = self.providers.acquire_connection_with_grace_override(input_name, allow_grace).await;
                if !matches!(new_alloc, ProviderAllocation::Exhausted) {
                    if let Some(provider_name) = new_alloc.get_provider_name() {
                        if !self.is_reserved_for_other(&provider_name, session_owner).await {
                            allocation.release().await;
                            return self.register_allocation(new_alloc, addr, priority).await;
                        }
                    }
                    new_alloc.release().await;
                }
            }
        }

        self.register_allocation(allocation, addr, priority).await
    }

    fn select_preemption_candidate(
        &self,
        connections: &Connections,
        input_name: &Arc<str>,
        new_priority: i8,
        reserved_providers: &HashSet<Arc<str>>,
    ) -> Option<PreemptionCandidate> {
        let mut victim = None;

        for (prov_name, tree) in &connections.priority_index {
            if !self.providers.is_provider_for_input(prov_name, input_name) || reserved_providers.contains(prov_name) {
                continue;
            }

            let Some(((victim_priority, Reverse(created_at), allocation_id), owner)) = tree.iter().next_back() else {
                continue;
            };

            if *victim_priority <= new_priority {
                continue;
            }

            let candidate = (*owner, *allocation_id, *victim_priority, *created_at);
            if is_better_preemption_candidate(victim, candidate) {
                victim = Some(candidate);
            }
        }

        victim
    }

    async fn acquire_connection_inner(
        &self,
        provider_or_input_name: &Arc<str>,
        addr: &SocketAddr,
        force: bool,
        allow_grace_override: Option<bool>,
        priority: i8,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        let allow_grace = if force { true } else { allow_grace_override.unwrap_or(true) };

        if force {
            let allocation = self.providers.force_exact_acquire_connection(provider_or_input_name).await;
            if matches!(allocation, ProviderAllocation::Exhausted) {
                return None;
            }
            return self.register_allocation(allocation, addr, priority).await;
        }

        if let Some(owner) = session_owner {
            if let Some(reserved_provider) = self.get_reserved_provider_for_owner(provider_or_input_name, owner).await {
                return self.acquire_exact_connection_inner(&reserved_provider, addr, allow_grace, priority, Some(owner)).await;
            }
        }

        let candidate_count = self.providers.provider_names_for_input(provider_or_input_name).len();
        let attempts = candidate_count.max(1);
        let mut skipped_reserved = HashSet::new();
        for _ in 0..attempts {
            let allocation = self.providers.acquire_connection_with_grace_override(provider_or_input_name, allow_grace).await;
            if matches!(allocation, ProviderAllocation::Exhausted) {
                break;
            }
            if let Some(provider_name) = allocation.get_provider_name() {
                if self.is_reserved_for_other(&provider_name, session_owner).await {
                    debug_if_enabled!(
                        "Skipping reserved provider {} for {}",
                        sanitize_sensitive_info(&provider_name),
                        sanitize_sensitive_info(&addr.to_string())
                    );
                    skipped_reserved.insert(provider_name);
                    allocation.release().await;
                    if skipped_reserved.len() >= attempts {
                        break;
                    }
                    continue;
                }
            }
            return self
                .finalize_lineup_allocation(provider_or_input_name, addr, allow_grace, priority, allocation, session_owner)
                .await;
        }

        if let Some(preempted_alloc) = self
            .try_preempt_connection(provider_or_input_name, priority, allow_grace, session_owner)
            .await
        {
            return self.register_allocation(preempted_alloc, addr, priority).await;
        }

        None
    }

    async fn register_allocation(
        &self,
        allocation: ProviderAllocation,
        addr: &SocketAddr,
        priority: i8,
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
                cancel_token: cancel_token.clone(),
                created_at: now,
                priority,
            },
        );

        connections.by_provider.entry(provider_name.clone()).or_default().insert((*addr, allocation_id));
        connections.priority_index.entry(provider_name.clone()).or_default()
            .insert((priority, Reverse(now), allocation_id), PriorityOwner::Single(*addr));

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
        session_owner: Option<&str>,
    ) -> bool {
        let reserved_providers = self.reserved_provider_names_for_other(input_name, session_owner).await;

        let victim = {
            let connections = self.connections.read().await;
            self.select_preemption_candidate(&connections, input_name, new_priority, &reserved_providers)
        };

        let Some((owner, alloc_id, v_prio, victim_created_at)) = victim else {
            return false;
        };
        match owner {
            PriorityOwner::Shared(shared_id) => {
                debug_if_enabled!(
                    "Grace-evicting shared connection (allocation_id={shared_id}, prio={v_prio}) on input {} for higher priority request (prio={})",
                    sanitize_sensitive_info(input_name),
                    new_priority
                );

                let released = {
                    let mut connections = self.connections.write().await;
                    let Some(key) = connections.shared.shared_by_allocation_id.get(&shared_id).cloned() else {
                        return false;
                    };

                    let still_match = connections.shared.by_key.get(&key).is_some_and(|shared| {
                        shared.allocation_id == alloc_id
                            && shared.priority == v_prio
                            && shared.created_at == victim_created_at
                    });
                    if !still_match {
                        return false;
                    }

                    if let Some(shared) = connections.shared.by_key.remove(&key) {
                        connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
                        for shared_addr in shared.connections.keys() {
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
                        Some((key, shared.allocation, shared.cancel_token))
                    } else {
                        return false;
                    }
                };
                if let Some((stream_url, allocation, cancel_token)) = released {
                    if let Some(token) = cancel_token {
                        token.cancel();
                    }
                    allocation.release().await;
                    // Stop the shared stream broadcast task to match the released capacity.
                    // Without this, the broadcast keeps running and consuming a provider slot
                    // that was already freed by allocation.release().
                    if let Some(ssm) = self.shared_stream_manager.get() {
                        ssm.teardown_preempted_stream(&stream_url).await;
                    }
                }
            }
            PriorityOwner::Single(addr) => {
                debug_if_enabled!(
                    "Grace-evicting single connection from {} (prio={}) on input {} for higher priority request (prio={})",
                    sanitize_sensitive_info(&addr.to_string()),
                    v_prio,
                    sanitize_sensitive_info(input_name),
                    new_priority
                );

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
                            list.remove(&(addr, alloc_id));
                        }
                        if let Some(tree) = connections.priority_index.get_mut(name) {
                            tree.remove(&(v_prio, Reverse(victim_created_at), alloc_id));
                        }
                    }
                    removed_info
                };

                let Some(info) = removed_info else { return false; };
                // Preempted probes must stop immediately; they must not keep a custom stream alive.
                info.cancel_token.cancel();
                info.allocation.release().await;
            }
        }

        true
    }

    #[allow(clippy::too_many_lines)]
    async fn try_preempt_connection(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
        allow_grace: bool,
        session_owner: Option<&str>,
    ) -> Option<ProviderAllocation> {
        let reserved_providers = self.reserved_provider_names_for_other(input_name, session_owner).await;
        let victim = {
            let connections = self.connections.read().await;
            self.select_preemption_candidate(&connections, input_name, new_priority, &reserved_providers)
        };

        if let Some((owner, alloc_id, v_prio, victim_created_at)) = victim {
            match owner {
                PriorityOwner::Shared(shared_id) => {
                    debug_if_enabled!(
                        "Preempting shared connection (allocation_id={shared_id}, prio={v_prio}) for higher priority request (prio={new_priority})"
                    );
                    let released_shared_allocation = {
                        let mut connections = self.connections.write().await;
                        let key = connections.shared.shared_by_allocation_id.get(&shared_id).cloned()?;

                        // Revalidate the selected shared victim under WRITE lock.
                        let still_match = connections.shared.by_key.get(&key).is_some_and(|shared| {
                            shared.allocation_id == alloc_id
                                && shared.priority == v_prio
                                && shared.created_at == victim_created_at
                        });
                        if !still_match {
                            None
                        } else if let Some(shared) = connections.shared.by_key.remove(&key) {
                            connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
                            for shared_addr in shared.connections.keys() {
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
                            Some((key, shared.allocation, shared.cancel_token))
                        } else {
                            None
                        }
                    };
                    let (stream_url, allocation, cancel_token) = released_shared_allocation?;
                    if let Some(token) = cancel_token {
                        token.cancel();
                    }
                    allocation.release().await;
                    // Shared broadcast has to be torn down explicitly, otherwise the provider
                    // stream may continue after allocation counters were already released.
                    if let Some(ssm) = self.shared_stream_manager.get() {
                        ssm.teardown_preempted_stream(&stream_url).await;
                    } else {
                        error!(
                            "SharedStreamManager not initialised during preemption teardown for {}; \
                             shared stream may linger after allocation release",
                            sanitize_sensitive_info(&stream_url)
                        );
                    }
                }
                PriorityOwner::Single(addr) => {
                    debug_if_enabled!(
                        "Preempting single connection from {} (prio={v_prio}) for higher priority request (prio={new_priority})",
                        sanitize_sensitive_info(&addr.to_string())
                    );

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
                                list.remove(&(addr, alloc_id));
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

                    // Preempted probes must stop immediately; they must not keep a custom stream alive.
                    info.cancel_token.cancel();
                    info.allocation.release().await;
                }
            }

            // Now try acquire again preserving the original grace policy.
            let attempts = self.providers.provider_names_for_input(input_name).len().max(1);
            for _ in 0..attempts {
                let allocation = self.providers.acquire_connection_with_grace_override(input_name, allow_grace).await;
                if matches!(allocation, ProviderAllocation::Exhausted) {
                    break;
                }
                if let Some(provider_name) = allocation.get_provider_name() {
                    if reserved_providers.contains(&provider_name) {
                        allocation.release().await;
                        continue;
                    }
                }
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
        self.acquire_exact_connection_with_grace_for_session(provider_name, addr, allow_grace, priority, None).await
    }

    pub async fn acquire_exact_connection_with_grace_for_session(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        self.acquire_exact_connection_inner(provider_name, addr, allow_grace, priority, session_owner).await
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
        self.acquire_connection_inner(input_name, addr, false, None, priority, None).await
    }

    /// Acquire a provider connection while explicitly controlling provider-side grace allocations.
    pub async fn acquire_connection_with_grace(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_with_grace_for_session(input_name, addr, allow_grace, priority, None).await
    }

    pub async fn acquire_connection_with_grace_for_session(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, addr, false, Some(allow_grace), priority, session_owner).await
    }

    /// Acquire a provider connection for probe tasks with configurable priority.
    /// Probes never consume grace capacity.
    pub async fn acquire_connection_for_probe(&self, input_name: &Arc<str>, priority: i8) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, &DUMMY_ADDR, false, Some(false), priority, None).await
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
                            list.remove(&(*addr, *id));
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
                // Recompute shared priority from remaining subscribers so preemption decisions
                // reflect who is actually still watching the shared stream.
                let old_priority = shared.priority;
                if let Some(new_priority) = shared.connections.values().copied().min() {
                    if new_priority != old_priority {
                        shared.priority = new_priority;
                        if let Some(name) = shared.allocation.get_provider_name() {
                            if let Some(tree) = connections.priority_index.get_mut(&name) {
                                tree.remove(&(old_priority, Reverse(shared.created_at), shared.allocation_id));
                                tree.insert(
                                    (new_priority, Reverse(shared.created_at), shared.allocation_id),
                                    PriorityOwner::Shared(shared.allocation_id),
                                );
                            }
                        }
                    }
                }
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
                            list.remove(&(handle.client_id, handle.allocation_id));
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
                        for addr in shared.connections.keys() {
                            connections.shared.key_by_addr.remove(addr);
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
            let shared_key: Arc<str> = Arc::from(key);

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
                                list.remove(&(*addr, id));
                            }
                            // Remove old single entry from priority_index
                            if let Some(tree) = connections.priority_index.get_mut(&name) {
                                tree.remove(&(info.priority, Reverse(info.created_at), id));
                            }
                        }
                        // Remove extras from provider-specific indexes.
                        for (extra_id, extra_info) in &extra_entries {
                            if let Some(extra_provider_name) = extra_info.allocation.get_provider_name() {
                                if let Some(list) = connections.by_provider.get_mut(&extra_provider_name) {
                                    list.remove(&(*addr, *extra_id));
                                }
                                if let Some(tree) = connections.priority_index.get_mut(&extra_provider_name) {
                                    tree.remove(&(extra_info.priority, Reverse(extra_info.created_at), *extra_id));
                                }
                            }
                        }

                        for (_, extra_info) in extra_entries {
                            extra_info.cancel_token.cancel();
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
                    Arc::clone(&shared_key),
                    SharedAllocation {
                        allocation_id: handle.0.allocation_id,
                        allocation: handle.0.allocation.clone(),
                        connections: HashMap::from([(*addr, handle.1)]),
                        priority: handle.1,
                        created_at: handle.2,
                        cancel_token: handle.0.cancel_token.clone(),
                    },
                );
                connections.shared.key_by_addr.insert(*addr, Arc::clone(&shared_key));
                connections
                    .shared
                    .shared_by_allocation_id
                    .insert(handle.0.allocation_id, shared_key);

                // Insert new shared entry into priority_index
                connections.priority_index.entry(provider_name.clone()).or_default()
                    .insert(
                        (handle.1, Reverse(handle.2), handle.0.allocation_id),
                        PriorityOwner::Shared(handle.0.allocation_id),
                    );
            }
            extras
        };

        for allocation in extras {
            allocation.release().await;
        }
    }

    pub async fn add_shared_connection(&self, addr: &SocketAddr, key: &str, priority: i8) -> Result<(), String> {
        let mut connections = self.connections.write().await;

        // Extract metadata before taking a second mutable borrow on `connections`.
        let metadata = connections.shared.by_key.get(key).map(|s| {
            (s.allocation_id, s.allocation.get_provider_name().unwrap_or_default(), s.priority, s.created_at)
        });

        let Some((alloc_id, provider_name, old_priority, created_at)) = metadata else {
            let err = format!(
                "Failed to add shared connection for {addr}: url {} not found",
                sanitize_sensitive_info(key)
            );
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

        shared_allocation.connections.insert(*addr, priority);

        // If the joining subscriber has higher importance (lower numeric priority),
        // update the shared allocation's priority and refresh the priority index.
        if priority < old_priority {
            shared_allocation.priority = priority;
            if let Some(tree) = connections.priority_index.get_mut(&provider_name) {
                tree.remove(&(old_priority, Reverse(created_at), alloc_id));
                tree.insert(
                    (priority, Reverse(created_at), alloc_id),
                    PriorityOwner::Shared(alloc_id),
                );
            }
        }

        connections.shared.key_by_addr.insert(*addr, Arc::from(key));
        Ok(())
    }

    pub async fn get_provider_connections_count(&self) -> usize { self.providers.active_connection_count().await }

}

#[cfg(test)]
mod tests {
    use super::ActiveProviderManager;
    use crate::{
        api::model::{EventManager, ProviderAllocation},
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

    #[tokio::test]
    async fn test_probe_preemption_releases_capacity_and_cancels_immediately() {
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

        // Cancellation happens inline during preemption; yield once to observe any deferred work.
        tokio::task::yield_now().await;
        assert!(probe_token.is_cancelled(), "probe token should be cancelled immediately after preemption");

        manager.release_connection(&user_addr).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_session_provider_reservation_blocks_other_sessions_until_ttl_expires() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let owner_1 = "session-owner-1";
        let owner_2 = "session-owner-2";
        let addr_1: SocketAddr = "127.0.0.1:43101".parse().unwrap();
        let addr_2: SocketAddr = "127.0.0.1:43102".parse().unwrap();

        manager.refresh_provider_reservation(&input_name, owner_1, 15).await;

        let first = manager
            .acquire_connection_with_grace_for_session(&input_name, &addr_1, false, default_user_priority(), Some(owner_1))
            .await
            .expect("reserved owner should reacquire its provider");
        assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        manager.release_connection(&addr_1).await;

        let blocked = manager
            .acquire_connection_with_grace_for_session(&input_name, &addr_2, false, default_user_priority(), Some(owner_2))
            .await;
        assert!(blocked.is_none(), "other sessions must not take a reserved provider before TTL expiry");

        tokio::time::advance(Duration::from_secs(16)).await;

        let second = manager
            .acquire_connection_with_grace_for_session(&input_name, &addr_2, false, default_user_priority(), Some(owner_2))
            .await
            .expect("reservation should expire after TTL");
        assert_eq!(second.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        manager.release_connection(&addr_2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_same_client_family_can_reuse_reserved_provider_after_channel_switch() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let first_addr: SocketAddr = "127.0.0.1:43111".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:43112".parse().unwrap();
        let owner_channel_1 = "client|ua|user|100";
        let owner_channel_2 = "client|ua|user|200";

        manager.refresh_provider_reservation(&input_name, owner_channel_1, 15).await;

        let reacquired = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &second_addr,
                false,
                default_user_priority(),
                Some(owner_channel_2),
            )
            .await
            .expect("same client family should be able to reuse the reserved provider after a channel switch");

        assert_eq!(reacquired.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        manager.release_connection(&first_addr).await;
        manager.release_connection(&second_addr).await;
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
    async fn test_equal_priority_user_gets_grace_without_preempting_existing_stream() {
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

        // User 2 arrives with the same priority and should be granted grace instead of being rejected.
        let alloc2 = manager
            .acquire_connection(&input_name, &user_2_addr, default_user_priority())
            .await
            .expect("same-priority user should get grace allocation");
        assert!(matches!(alloc2.allocation, ProviderAllocation::GracePeriod(_)));
        assert_eq!(alloc2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // User 1 should NOT be cancelled, and the provider should be temporarily over limit.
        assert!(!token1.is_cancelled(), "same-prio user should not be evicted");
        assert!(manager.is_over_limit(&input_name).await, "provider should be temporarily over limit during grace");

        manager.release_connection(&user_1_addr).await;
        manager.release_connection(&user_2_addr).await;
    }

    #[tokio::test]
    async fn test_higher_priority_user_preempts_first_inserted_low_priority_victim_on_exact_tie() {
        let app_cfg = create_test_app_config_with_dual_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let old_low_addr: SocketAddr = "127.0.0.1:48101".parse().unwrap();
        let new_low_addr: SocketAddr = "127.0.0.1:48102".parse().unwrap();
        let high_prio_addr: SocketAddr = "127.0.0.1:48103".parse().unwrap();

        // Place two equal-priority low-priority victims on different provider aliases.
        // The test then normalizes their created_at timestamps to an exact tie so the
        // selector must fall back to the final stable tie-break instead of clock order.
        let old_alloc = manager
            .acquire_exact_connection_with_grace(&"provider_2".intern(), &old_low_addr, false, 20)
            .await
            .expect("old low-priority allocation should succeed");
        let old_token = old_alloc.cancel_token.clone().expect("old allocation should have cancel token");
        assert_eq!(old_alloc.allocation.get_provider_name().as_deref(), Some("provider_2"));

        let new_alloc = manager
            .acquire_exact_connection_with_grace(&"provider_1".intern(), &new_low_addr, false, 20)
            .await
            .expect("new low-priority allocation should succeed");
        let new_token = new_alloc.cancel_token.clone().expect("new allocation should have cancel token");
        assert_eq!(new_alloc.allocation.get_provider_name().as_deref(), Some("provider_1"));

        {
            let mut connections = manager.connections.write().await;
            let old_created_at = connections
                .single
                .get(&old_low_addr)
                .and_then(|per_addr| per_addr.get(&old_alloc.allocation_id))
                .map(|info| info.created_at)
                .expect("old allocation should still be registered");

            let (new_created_at, new_priority) = {
                let per_addr = connections
                    .single
                    .get_mut(&new_low_addr)
                    .expect("new allocation address should still be registered");
                let info = per_addr
                    .get_mut(&new_alloc.allocation_id)
                    .expect("new allocation should still be registered");
                let original_created_at = info.created_at;
                info.created_at = old_created_at;
                (original_created_at, info.priority)
            };

            let provider_name = "provider_1".intern();
            let tree = connections
                .priority_index
                .get_mut(&provider_name)
                .expect("priority index for provider_1 should exist");
            let owner = tree
                .remove(&(new_priority, std::cmp::Reverse(new_created_at), new_alloc.allocation_id))
                .expect("new allocation should still be indexed");
            tree.insert((new_priority, std::cmp::Reverse(old_created_at), new_alloc.allocation_id), owner);
        }

        assert!(manager.is_exhausted(&input_name).await);

        // Higher-priority request should now select the first-inserted victim because
        // priority and created_at are exactly tied across provider aliases.
        let high_alloc = manager
            .acquire_connection(&input_name, &high_prio_addr, 0)
            .await
            .expect("higher-priority request should preempt the first-inserted low-priority victim on exact tie");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some("provider_2"));

        assert!(old_token.is_cancelled(), "first-inserted low-priority victim should be canceled first on exact tie");
        assert!(!new_token.is_cancelled(), "later allocation should remain active after the stable tie-break");

        manager.release_connection(&high_prio_addr).await;
        manager.release_connection(&old_low_addr).await;
        manager.release_connection(&new_low_addr).await;
    }

    #[tokio::test]
    async fn test_shared_priority_downgrades_after_high_priority_user_leaves() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let stream_key = "http://example.com/shared/live";
        let addr_a: SocketAddr = "127.0.0.1:48501".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:48502".parse().unwrap();

        // A starts shared stream with high importance (priority 0).
        let alloc_a = manager
            .acquire_connection(&input_name, &addr_a, 0)
            .await
            .expect("A should get initial connection");
        let shared_token = alloc_a.cancel_token.clone().expect("shared allocation should have cancel token");
        manager.make_shared_connection(&addr_a, stream_key).await;

        // B joins the same shared stream with lower importance (priority 1).
        let join_result = manager.add_shared_connection(&addr_b, stream_key, 1).await;
        assert!(
            join_result.is_ok(),
            "B should join existing shared stream, got: {join_result:?}"
        );

        // A leaves shared stream. Shared allocation should now inherit B's lower priority.
        manager.release_connection(&addr_a).await;
        {
            let connections = manager.connections.read().await;
            let shared = connections.shared.by_key.get(stream_key).expect("shared entry should remain for B");
            assert_eq!(shared.priority, 1, "shared priority must downgrade to remaining subscriber priority");
        }

        // A starts another stream with higher importance and should preempt B's shared stream.
        let alloc_a2 = manager
            .acquire_connection(&input_name, &addr_a, 0)
            .await
            .expect("A should preempt lower-priority shared stream");
        assert_eq!(alloc_a2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        assert!(shared_token.is_cancelled(), "shared stream should be cancelled when preempted");
        assert!(!manager.is_over_limit(&input_name).await, "provider should not remain over limit after preemption");

        manager.release_connection(&addr_a).await;
        manager.release_connection(&addr_b).await;
    }

    #[tokio::test]
    async fn test_higher_priority_user_preempts_shared_stream_with_multiple_subscribers() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let stream_key = "http://example.com/shared/live/multi";
        let addr_a: SocketAddr = "127.0.0.1:48601".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:48602".parse().unwrap();
        let addr_high: SocketAddr = "127.0.0.1:48603".parse().unwrap();

        let shared_alloc = manager
            .acquire_connection(&input_name, &addr_a, 5)
            .await
            .expect("low-priority shared stream should get initial connection");
        let shared_token = shared_alloc.cancel_token.clone().expect("shared allocation should have cancel token");
        manager.make_shared_connection(&addr_a, stream_key).await;

        manager
            .add_shared_connection(&addr_b, stream_key, 6)
            .await
            .expect("second subscriber should join shared stream");

        let high_alloc = manager
            .acquire_connection(&input_name, &addr_high, 0)
            .await
            .expect("higher-priority user should preempt lower-priority shared stream");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        assert!(shared_token.is_cancelled(), "shared stream should be cancelled when preempted");

        {
            let connections = manager.connections.read().await;
            assert!(
                !connections.shared.by_key.contains_key(stream_key),
                "preempted shared stream must be removed even with multiple subscribers"
            );
        }

        manager.release_connection(&addr_high).await;
        manager.release_connection(&addr_a).await;
        manager.release_connection(&addr_b).await;
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
