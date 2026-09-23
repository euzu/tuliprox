use crate::{
    provider_leases::{ProviderLeaseTable, ProviderLeaseUsage},
    provider_lineup_manager::ProviderLineupManager,
    EventManager, SharedStreamManager,
};
use log::error;
use shared::utils::sanitize_sensitive_info;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, LazyLock, OnceLock, RwLockReadGuard, RwLockWriteGuard, Weak,
    },
    time::{Duration, Instant},
};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{
        AllocationId, AppConfig, ConfigInput, ConnectionLifecycle, GracePeriodOptions, PlaybackKind, PlaybackLeaseId,
        PlaybackRequestId, PlaybackRequestOutcome, PlaybackSelectionReason, ProviderAllocation, ProviderBindingTag,
        ProviderCloseReason, ProviderConfig, ProviderHandle, SharedSubscriberId,
    },
    utils::debug_if_enabled,
};

static DUMMY_ADDR: LazyLock<SocketAddr> = LazyLock::new(|| SocketAddr::from(([127, 0, 0, 1], 0)));
type SharedConnectionId = AllocationId;
type PreemptionCandidate = (PriorityOwner, AllocationId, i8, Instant);
// Key for BTreeMap priority index: (priority, Reverse<created_at>, AllocationId)
// Semantics: lower numeric priority value = higher importance (0 = highest, 127 = lowest).
// `.last()` on the BTreeMap returns the entry with the highest priority value, which is
// the lowest-importance connection and therefore the best eviction victim.
// Ties are broken by `Reverse<Instant>`: among equal priority values, the oldest connection
// (smallest `created_at`) sorts last and is evicted first.
type PriorityKey = (i8, Reverse<Instant>, AllocationId);

const PREEMPTION_COMPLETION_TIMEOUT: Duration = Duration::from_millis(1500);
const EVICTED_PROVIDER_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProviderReleaseSnapshot {
    pub addr: SocketAddr,
    single_allocations: Vec<AllocationId>,
    shared_subscribers: Vec<SharedSubscriberId>,
}

impl ProviderReleaseSnapshot {
    pub fn is_empty(&self) -> bool { self.single_allocations.is_empty() && self.shared_subscribers.is_empty() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    Normal,
    Soft,
}

fn is_better_preemption_candidate(current: Option<PreemptionCandidate>, candidate: PreemptionCandidate) -> bool {
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
    Single(AllocationId),
    Shared(SharedConnectionId),
}

/// Playback identity of one provider acquisition.
///
/// `owner` is a stable playback identity (usually the user session token), never a
/// socket address and never a per-attempt random suffix in a shared family. Retries
/// of one playback resolve to the same owner and therefore to one capacity slot,
/// while two players behind a reverse proxy keep independent owners.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackLeaseRef<'a> {
    pub owner: &'a str,
    pub kind: PlaybackKind,
    pub request_id: PlaybackRequestId,
}

impl<'a> PlaybackLeaseRef<'a> {
    pub fn new(owner: &'a str, kind: PlaybackKind) -> Self {
        Self { owner, kind, request_id: PlaybackRequestId::next() }
    }

    /// Compatibility view of a bare session owner: one request, live TS semantics.
    pub fn for_session_owner(owner: &'a str) -> Self { Self::new(owner, PlaybackKind::LiveTs) }
}

struct AcquireProviderParams<'a> {
    addr: &'a SocketAddr,
    priority: i8,
    kind: ConnectionKind,
    lease: Option<PlaybackLeaseRef<'a>>,
}

struct ProviderAllocationGuard(Option<ProviderAllocation>);

impl ProviderAllocationGuard {
    fn new(allocation: ProviderAllocation) -> Self { Self(Some(allocation)) }

    fn allocation(&self) -> &ProviderAllocation { self.0.as_ref().unwrap_or(&ProviderAllocation::Exhausted) }

    fn take(&mut self) -> ProviderAllocation { self.0.take().unwrap_or(ProviderAllocation::Exhausted) }
}

impl Drop for ProviderAllocationGuard {
    fn drop(&mut self) {
        if let Some(allocation) = self.0.take() {
            allocation.release();
        }
    }
}

impl AcquireProviderParams<'_> {
    #[inline]
    fn session_owner(&self) -> Option<&str> { self.lease.map(|lease| lease.owner) }
}

#[derive(Debug, Clone)]
struct SharedAllocation {
    allocation_id: AllocationId,
    allocation: ProviderAllocation,
    /// Keyed by unique subscriber id, never by socket: two external clients behind one
    /// reverse proxy must not collapse into a single entry.
    connections: HashMap<SharedSubscriberId, SharedSubscriber>,
    priority: i8,
    kind: ConnectionKind,
    created_at: Instant,
    cancel_token: Option<CancellationToken>,
    session_owner: Option<Arc<str>>,
}

#[derive(Debug, Clone)]
struct ActiveConnectionInfo {
    allocation_id: AllocationId,
    client_addr: SocketAddr,
    allocation: ProviderAllocation,
    // Used to signal preemption to the consumer of this connection
    cancel_token: CancellationToken,
    completion_token: CancellationToken,
    close_reason: Arc<AtomicU8>,
    lifecycle: ConnectionLifecycle,
    has_body_owner: bool,
    reaper_spawned: bool,
    open_generation: u64,
    created_at: Instant,
    priority: i8,
    kind: ConnectionKind,
    session_owner: Option<Arc<str>>,
    playback_request_id: Option<PlaybackRequestId>,
}

#[derive(Debug, Clone, Copy)]
struct SharedSubscriber {
    priority: i8,
    kind: ConnectionKind,
    /// Transport metadata and socket-wide close target only; never an identity key.
    addr: SocketAddr,
}

#[derive(Debug, Clone, Default)]
struct SharedConnections {
    by_key: HashMap<Arc<str>, SharedAllocation>,
    key_by_subscriber: HashMap<SharedSubscriberId, Arc<str>>,
    shared_by_allocation_id: HashMap<AllocationId, Arc<str>>,
}

#[derive(Debug, Clone, Default)]
struct Connections {
    // Primary map keyed by AllocationId, never by socket address.
    single: HashMap<AllocationId, ActiveConnectionInfo>,
    // Secondary index for socket-wide transport close/kick actions.
    single_by_addr: HashMap<SocketAddr, HashSet<AllocationId>>,
    shared: SharedConnections,
    // Index to quickly find connections by provider name for preemption
    // ProviderName -> Set<AllocationId>
    by_provider: HashMap<Arc<str>, HashSet<AllocationId>>,
    // Index to find every allocation (single and shared) owned by a playback session.
    // SessionOwner -> Set<AllocationId>
    by_owner: HashMap<Arc<str>, HashSet<AllocationId>>,
    // Priority index per provider alias for O(log n) victim lookup
    // ProviderName -> BTreeMap<PriorityKey, PriorityOwner>
    priority_index: HashMap<Arc<str>, BTreeMap<PriorityKey, PriorityOwner>>,
    soft_priority_index: HashMap<Arc<str>, BTreeMap<PriorityKey, PriorityOwner>>,
}

pub struct ManagedProviderHandle {
    manager: Arc<ActiveProviderManager>,
    handle: Option<ProviderHandle>,
}

impl std::fmt::Debug for ManagedProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedProviderHandle").field("handle", &self.handle).finish_non_exhaustive()
    }
}

impl ManagedProviderHandle {
    #[inline]
    pub fn manager(&self) -> &Arc<ActiveProviderManager> { &self.manager }

    #[inline]
    pub fn new(manager: Arc<ActiveProviderManager>, handle: ProviderHandle) -> Self {
        Self { manager, handle: Some(handle) }
    }

    #[inline]
    pub fn handle(&self) -> Option<&ProviderHandle> { self.handle.as_ref() }

    #[inline]
    pub fn take(&mut self) -> Option<ProviderHandle> { self.handle.take() }

    #[inline]
    pub fn disarm(&mut self) -> Option<ProviderHandle> { self.handle.take() }

    pub fn mark_opening(&self) {
        if let Some(handle) = self.handle.as_ref() {
            self.manager.mark_opening(handle.allocation_id);
        }
    }

    pub fn register_body_owner(&self) -> bool {
        if let Some(handle) = self.handle.as_ref() {
            self.manager.register_body_owner(handle.allocation_id)
        } else {
            false
        }
    }

    pub fn renew_opening_tokens(&mut self) -> Option<(CancellationToken, CancellationToken)> {
        if let Some(ref mut handle) = self.handle {
            if let Some((cancel, completion, gen)) = self.manager.renew_opening_tokens(handle.allocation_id) {
                handle.cancel_token = Some(cancel.clone());
                handle.completion_token = Some(completion.clone());
                handle.open_generation = gen;
                return Some((cancel, completion));
            }
        }
        None
    }
}

impl Drop for ManagedProviderHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.manager.release_handle(&handle);
        }
    }
}

pub struct ActiveProviderManagerCore {
    // Serializes capacity transitions across counters, allocation indices and leases.
    capacity_transition: std::sync::Mutex<()>,
    is_shutting_down: AtomicBool,
    shutdown_token: CancellationToken,
    providers: ProviderLineupManager,
    connections: std::sync::RwLock<Connections>,
    leases: std::sync::RwLock<ProviderLeaseTable>,
    next_allocation_id: AtomicU64,
}

impl ActiveProviderManagerCore {
    /// Serialises capacity transitions (acquire/release/reclassify) so a reclassify
    /// cannot race an acquire or release.
    ///
    /// Lock-order invariant: this lock must be acquired **before** the connections
    /// lock (`read_connections`/`write_connections`). No path may hold the
    /// connections lock and then call this; doing so can deadlock.
    fn lock_capacity_transition(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.capacity_transition.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned active-provider capacity transition lock");
                poisoned.into_inner()
            }
        }
    }

    fn read_connections(&self) -> RwLockReadGuard<'_, Connections> {
        match self.connections.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned active-provider connection lock");
                poisoned.into_inner()
            }
        }
    }

    fn write_connections(&self) -> RwLockWriteGuard<'_, Connections> {
        match self.connections.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned active-provider connection lock");
                poisoned.into_inner()
            }
        }
    }

    fn write_leases(&self) -> RwLockWriteGuard<'_, ProviderLeaseTable> {
        match self.leases.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned provider-lease lock");
                poisoned.into_inner()
            }
        }
    }

    #[cfg(test)]
    fn read_leases(&self) -> RwLockReadGuard<'_, ProviderLeaseTable> {
        match self.leases.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn complete_release(&self, alloc_id: AllocationId) { self.complete_release_with_generation(alloc_id, None); }

    pub fn complete_release_with_generation(&self, alloc_id: AllocationId, generation: Option<u64>) {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        Self::complete_release_locked(&mut connections, alloc_id, generation);
    }

    fn complete_release_locked(
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
            Some(info.allocation)
        } else {
            None
        }
    }

    fn plan_single_release_locked(connections: &mut Connections, alloc_id: AllocationId) -> ReleaseAction {
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
enum ReleaseAction {
    None,
    Immediate,
    Wait(AllocationId, u64, CancellationToken),
}

#[derive(Debug)]
pub(crate) enum PreemptionOutcome {
    Acquired(ProviderAllocation),
    /// Victim body is still draining. The optional pair is `(alloc_id, open_generation)` of the
    /// victim for an idempotent self-release attempt once the token fires; `None` for shared-stream
    /// teardowns where release happens inside the spawned task and no `complete_release_locked` call
    /// is needed.
    PendingCompletion(Option<(AllocationId, u64)>, CancellationToken),
    Exhausted,
}

pub struct ActiveProviderManager {
    core: Arc<ActiveProviderManagerCore>,
    shared_stream_manager: OnceLock<Weak<SharedStreamManager>>,
}

impl std::ops::Deref for ActiveProviderManager {
    type Target = ActiveProviderManagerCore;

    #[inline]
    fn deref(&self) -> &Self::Target { &self.core }
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
        snapshot.single_allocations.iter().any(|id| connections.single.contains_key(id))
            || snapshot.shared_subscribers.iter().any(|id| connections.shared.key_by_subscriber.contains_key(id))
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

    fn upsert_priority_entry(
        connections: &mut Connections,
        provider_name: &Arc<str>,
        key: PriorityKey,
        owner: PriorityOwner,
        kind: ConnectionKind,
    ) {
        connections.priority_index.entry(provider_name.clone()).or_default().insert(key, owner);
        if kind == ConnectionKind::Soft {
            connections.soft_priority_index.entry(provider_name.clone()).or_default().insert(key, owner);
        }
    }

    fn remove_priority_entry(
        connections: &mut Connections,
        provider_name: &Arc<str>,
        key: &PriorityKey,
        kind: ConnectionKind,
    ) {
        if let Some(tree) = connections.priority_index.get_mut(provider_name) {
            tree.remove(key);
        }
        if kind == ConnectionKind::Soft {
            if let Some(tree) = connections.soft_priority_index.get_mut(provider_name) {
                tree.remove(key);
            }
        }
    }

    fn index_owner(connections: &mut Connections, allocation_id: AllocationId, owner: Option<&Arc<str>>) {
        if let Some(owner) = owner {
            connections.by_owner.entry(Arc::clone(owner)).or_default().insert(allocation_id);
        }
    }

    fn unindex_owner(connections: &mut Connections, allocation_id: AllocationId, owner: Option<&Arc<str>>) {
        if let Some(owner) = owner {
            let remove = connections.by_owner.get_mut(owner).is_some_and(|set| {
                set.remove(&allocation_id);
                set.is_empty()
            });
            if remove {
                connections.by_owner.remove(owner);
            }
        }
    }

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

    pub fn new(cfg: &AppConfig, event_manager: &Arc<EventManager>) -> Self {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        Self {
            core: Arc::new(ActiveProviderManagerCore {
                capacity_transition: std::sync::Mutex::new(()),
                is_shutting_down: AtomicBool::new(false),
                shutdown_token: CancellationToken::new(),
                providers: ProviderLineupManager::new(inputs, grace_period_options, event_manager),
                connections: std::sync::RwLock::new(Connections::default()),
                leases: std::sync::RwLock::new(ProviderLeaseTable::default()),
                next_allocation_id: AtomicU64::new(1),
            }),
            shared_stream_manager: OnceLock::new(),
        }
    }

    pub fn set_shared_stream_manager(&self, manager: &Arc<SharedStreamManager>) {
        let _ = self.shared_stream_manager.set(Arc::downgrade(manager));
    }

    /// Closes provider admission and releases all physical allocations and leases.
    pub fn shutdown(&self) {
        self.shutdown_token.cancel();
        self.is_shutting_down.store(true, Ordering::Release);
        let _transition = self.lock_capacity_transition();
        let connections = std::mem::take(&mut *self.write_connections());
        self.write_leases().clear();

        for info in connections.single.into_values() {
            info.cancel_token.cancel();
            info.allocation.release();
        }
        for shared in connections.shared.by_key.into_values() {
            if let Some(cancel_token) = shared.cancel_token {
                cancel_token.cancel();
            }
            shared.allocation.release();
        }
        self.providers.reconcile_connections(HashMap::new());
    }

    fn get_config_inputs(cfg: &AppConfig) -> Vec<Arc<ConfigInput>> {
        cfg.sources.load().inputs.iter().filter(|i| i.enabled).map(Arc::clone).collect()
    }

    fn get_grace_options(cfg: &AppConfig) -> GracePeriodOptions { cfg.config.load().get_grace_options() }

    pub fn update_config(&self, cfg: &AppConfig) {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        self.providers.update_config(inputs, &grace_period_options);
        self.reconcile_connections();
    }

    pub fn reconcile_connections(&self) {
        let _transition = self.lock_capacity_transition();
        let mut counts = HashMap::<Arc<str>, usize>::new();
        {
            let connections = self.read_connections();

            // Single connections
            for info in connections.single.values() {
                if let Some(name) = info.allocation.get_provider_name() {
                    *counts.entry(name).or_insert(0) += 1;
                }
            }

            // Shared connections
            for shared in connections.shared.by_key.values() {
                if let Some(name) = shared.allocation.get_provider_name() {
                    *counts.entry(name).or_insert(0) += 1;
                }
            }
        }

        self.providers.reconcile_connections(counts);
    }

    fn prune_expired_leases(leases: &mut ProviderLeaseTable) { leases.prune(TokioInstant::now()); }

    fn has_foreign_reservation(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        if !self.providers.reservation_blocks_other_sessions(provider_name) {
            return false;
        }
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.has_foreign_reserved_lease(provider_name, session_owner)
    }

    fn get_reserved_provider_for_owner(&self, input_name: &Arc<str>, session_owner: &str) -> Option<Arc<str>> {
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        let provider_name = leases.provider_for_owner(session_owner)?;
        self.providers.is_provider_for_input(&provider_name, input_name).then_some(provider_name)
    }

    /// True when `session_owner` already backs a live allocation on `provider_name`,
    /// without materialising the full owner set for a membership test.
    fn has_active_owner_for_provider(&self, provider_name: &Arc<str>, session_owner: &str) -> bool {
        let connections = self.read_connections();
        connections.by_owner.get(session_owner).is_some_and(|alloc_ids| {
            alloc_ids.iter().any(|id| {
                if let Some(info) = connections.single.get(id) {
                    info.allocation.get_provider_name().as_ref() == Some(provider_name)
                } else if let Some(key) = connections.shared.shared_by_allocation_id.get(id) {
                    connections
                        .shared
                        .by_key
                        .get(key)
                        .is_some_and(|shared| shared.allocation.get_provider_name().as_ref() == Some(provider_name))
                } else {
                    false
                }
            })
        })
    }

    fn active_reservation_owners(&self, provider_name: &Arc<str>) -> HashSet<Arc<str>> {
        let connections = self.read_connections();
        let mut owners = HashSet::new();
        if let Some(alloc_ids) = connections.by_provider.get(provider_name) {
            for id in alloc_ids {
                if let Some(info) = connections.single.get(id) {
                    if let Some(owner) = info.session_owner.as_ref() {
                        owners.insert(Arc::clone(owner));
                    }
                } else if let Some(key) = connections.shared.shared_by_allocation_id.get(id) {
                    if let Some(shared) = connections.shared.by_key.get(key) {
                        if let Some(owner) = shared.session_owner.as_ref() {
                            owners.insert(Arc::clone(owner));
                        }
                    }
                }
            }
        } else {
            for info in connections.single.values() {
                if info.allocation.get_provider_name().as_ref() == Some(provider_name) {
                    if let Some(owner) = info.session_owner.as_ref() {
                        owners.insert(Arc::clone(owner));
                    }
                }
            }
            for shared in connections.shared.by_key.values() {
                if shared.allocation.get_provider_name().as_ref() == Some(provider_name) {
                    if let Some(owner) = shared.session_owner.as_ref() {
                        owners.insert(Arc::clone(owner));
                    }
                }
            }
        }
        owners
    }

    fn reservation_capacity_usage(
        &self,
        provider_name: &Arc<str>,
        session_owner: Option<&str>,
    ) -> Option<(usize, usize, usize)> {
        let (current_connections, max_connections) = self.providers.provider_capacity(provider_name)?;
        if max_connections == 0 {
            return Some((current_connections, max_connections, 0));
        }
        let counted_owners = self.active_reservation_owners(provider_name);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        let idle_foreign_reservations = leases.foreign_reserved_slots(provider_name, session_owner, &counted_owners);

        Some((current_connections, max_connections, idle_foreign_reservations))
    }

    /// Returns true when the next allocation would consume a slot kept for an idle reservation.
    fn reserved_capacity_blocks_next(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        let Some((current_connections, max_connections, idle_foreign_reservations)) =
            self.reservation_capacity_usage(provider_name, session_owner)
        else {
            return true;
        };
        max_connections > 0
            && idle_foreign_reservations > 0
            && current_connections.saturating_add(idle_foreign_reservations) >= max_connections
    }

    /// Returns true when an already-counted candidate allocation consumed a slot kept for an idle reservation.
    fn exceeds_reserved_capacity(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        let Some((current_connections, max_connections, idle_foreign_reservations)) =
            self.reservation_capacity_usage(provider_name, session_owner)
        else {
            return true;
        };
        max_connections > 0
            && idle_foreign_reservations > 0
            && current_connections.saturating_add(idle_foreign_reservations) > max_connections
    }

    /// Explains a reservation skip with the authoritative slot breakdown instead of
    /// only the transport socket, so the decision can be audited from the logs.
    fn log_reserved_capacity_skip(&self, provider_name: &Arc<str>, params: &AcquireProviderParams<'_>) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let Some((current_connections, max_connections, foreign_reserved)) =
            self.reservation_capacity_usage(provider_name, params.session_owner())
        else {
            debug_if_enabled!(
                "Skipping reserved provider {} (reason={}, capacity=unknown, peer_addr={}, request_id={}, playback_kind={})",
                sanitize_sensitive_info(provider_name),
                PlaybackSelectionReason::ReservedCapacity,
                sanitize_sensitive_info(&params.addr.to_string()),
                params.lease.map_or_else(String::new, |lease| lease.request_id.to_string()),
                params.lease.map_or_else(|| "-".to_string(), |lease| lease.kind.to_string())
            );
            return;
        };
        let usage = self.provider_lease_usage(provider_name);
        debug_if_enabled!(
            "Skipping reserved provider {} (reason={}, current={}, max={}, foreign_reserved={}, active_slots={}, starting_slots={}, idle_slots={}, peer_addr={}, request_id={}, playback_kind={})",
            sanitize_sensitive_info(provider_name),
            PlaybackSelectionReason::ReservedCapacity,
            current_connections,
            max_connections,
            foreign_reserved,
            usage.active,
            usage.starting,
            usage.idle,
            sanitize_sensitive_info(&params.addr.to_string()),
            params.lease.map_or_else(String::new, |lease| lease.request_id.to_string()),
            params.lease.map_or_else(|| "-".to_string(), |lease| lease.kind.to_string())
        );
    }

    fn reserved_provider_names_for_other(
        &self,
        input_name: &Arc<str>,
        session_owner: Option<&str>,
    ) -> HashSet<Arc<str>> {
        let mut reserved = HashSet::new();
        for provider_name in self.providers.provider_names_for_input(input_name) {
            if self.has_foreign_reservation(&provider_name, session_owner) {
                reserved.insert(provider_name);
            }
        }
        reserved
    }

    pub fn refresh_provider_reservation(&self, provider_name: &Arc<str>, session_owner: &str, ttl_secs: u64) {
        self.refresh_identified_provider_reservation(provider_name, session_owner, PlaybackKind::LiveHls, ttl_secs);
    }

    /// Recreates or renews a provider reservation for an owner with an explicit kind.
    ///
    /// Unlike [`Self::refresh_adaptive_playback_lease`] this recreates the lease when it
    /// no longer exists, which is what a failed preemption needs to restore a victim
    /// reservation that [`Self::clear_identified_provider_reservation`] already removed.
    pub fn refresh_identified_provider_reservation(
        &self,
        provider_name: &Arc<str>,
        session_owner: &str,
        kind: PlaybackKind,
        ttl_secs: u64,
    ) {
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return;
        }
        let granted_ttl = self.renewal_granted_ttl(provider_name, session_owner, ttl_secs);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        if ttl_secs == 0 {
            leases.release_owner(session_owner);
            return;
        }
        if leases.renew_current_owner(session_owner, provider_name, kind, granted_ttl).is_none() {
            let req_id = PlaybackRequestId::next();
            let _ = leases.begin_owner(session_owner, provider_name, kind, req_id);
            leases.renew_identified_owner(session_owner, provider_name, kind, req_id, granted_ttl);
        }
    }

    /// Renews the lease of an adaptive (HLS/DASH/Catchup) playback.
    ///
    /// Adaptive playback consists of many short requests, so the lease must stay
    /// reconnect-capable: between two segment requests the slot is kept as an idle
    /// lease on the same provider instead of being released and reselected.
    pub fn refresh_adaptive_playback_lease(
        &self,
        provider_name: &Arc<str>,
        session_owner: &str,
        kind: PlaybackKind,
        ttl_secs: u64,
    ) {
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return;
        }
        let granted_ttl = self.renewal_granted_ttl(provider_name, session_owner, ttl_secs);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.renew_current_owner(session_owner, provider_name, kind, granted_ttl);
    }

    /// Renews the lease of a playback. A zero TTL clears it.
    pub fn refresh_playback_lease(&self, provider_name: &Arc<str>, lease_ref: &PlaybackLeaseRef<'_>, ttl_secs: u64) {
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return;
        }
        let granted_ttl = self.renewal_granted_ttl(provider_name, lease_ref.owner, ttl_secs);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        if ttl_secs == 0 {
            leases.release_identified_owner(lease_ref.owner, lease_ref.request_id);
            return;
        }
        leases.renew_identified_owner(
            lease_ref.owner,
            provider_name,
            lease_ref.kind,
            lease_ref.request_id,
            granted_ttl,
        );
    }

    /// The reconnect window a renewal may actually grant for `owner`.
    ///
    /// A lease that currently holds no reservation right must not gain one through a
    /// mere activity refresh: the grant is re-checked against provider capacity with
    /// the same admission rule as a confirmation, so a previously denied reservation
    /// cannot be silently restored by a later refresh.
    fn renewal_granted_ttl(&self, provider_name: &Arc<str>, owner: &str, requested_ttl_secs: u64) -> u64 {
        if requested_ttl_secs == 0 {
            return 0;
        }
        let already_reserves = {
            let mut leases = self.write_leases();
            Self::prune_expired_leases(&mut leases);
            leases.lease_of_owner(owner).is_some_and(|lease| lease.state.is_confirmed() && lease.idle_ttl_secs > 0)
        };
        if already_reserves || self.confirmation_reserve_allowed(provider_name, owner) {
            requested_ttl_secs
        } else {
            0
        }
    }

    pub fn clear_provider_reservation(&self, session_owner: &str) {
        let _transition = self.lock_capacity_transition();
        let mut leases = self.write_leases();
        leases.release_owner(session_owner);
    }

    pub fn clear_identified_provider_reservation(
        &self,
        session_owner: &str,
        provider_name: &Arc<str>,
        binding_tag: Option<ProviderBindingTag>,
    ) {
        // A delayed clear without the exact binding tag has no delete right: it must
        // not fall back to an owner-wide release and erase a successor lease.
        let Some(binding_tag) = binding_tag else {
            return;
        };
        let _transition = self.lock_capacity_transition();
        let mut leases = self.write_leases();
        leases.release_matching_owner(session_owner, provider_name, binding_tag);
    }

    /// Confirms real media activity for a playback lease. Only a confirmed lease may
    /// outlive its request as a reconnect slot.
    pub fn confirm_playback_activity(&self, owner: &str) -> Option<PlaybackLeaseId> { self.confirm_owner(owner, None) }

    /// Confirms real media activity for a specific request ID of a playback lease.
    pub fn confirm_identified_playback_activity(
        &self,
        owner: &str,
        request_id: PlaybackRequestId,
    ) -> Option<PlaybackLeaseId> {
        self.confirm_owner(owner, Some(request_id))
    }

    /// Records media activity under the capacity transition. The confirmation only
    /// grants a reservation right when the lease already backs a live allocation or a
    /// free slot remains; otherwise the media bytes are recorded without a reservation
    /// so a late cache confirmation cannot over-commit a provider at its limit.
    fn confirm_owner(&self, owner: &str, request_id: Option<PlaybackRequestId>) -> Option<PlaybackLeaseId> {
        let _transition = self.lock_capacity_transition();
        let (provider_name, wants_reservation) = {
            let mut leases = self.write_leases();
            Self::prune_expired_leases(&mut leases);
            let provider_name = leases.provider_for_owner(owner)?;
            let wants_reservation = leases.lease_of_owner(owner).is_some_and(|lease| lease.idle_ttl_secs > 0);
            (provider_name, wants_reservation)
        };
        let reserve = !wants_reservation || self.confirmation_reserve_allowed(&provider_name, owner);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        match (request_id, reserve) {
            (Some(request_id), true) => leases.confirm_identified_owner(owner, request_id),
            (Some(request_id), false) => leases.confirm_identified_owner_without_reservation(owner, request_id),
            (None, true) => leases.confirm_owner(owner),
            (None, false) => leases.confirm_owner_without_reservation(owner),
        }
    }

    /// True when confirming `session_owner`'s lease may reserve a provider slot: the
    /// lease either already backs a live allocation, or a free slot remains below the
    /// configured maximum after foreign reservations are counted.
    fn confirmation_reserve_allowed(&self, provider_name: &Arc<str>, session_owner: &str) -> bool {
        if self.has_active_owner_for_provider(provider_name, session_owner) {
            return true;
        }
        let Some((current, max, foreign)) = self.reservation_capacity_usage(provider_name, Some(session_owner)) else {
            return false;
        };
        max == 0 || current.saturating_add(foreign) < max
    }

    /// Ends a playback request and applies the outcome-specific lease policy.
    ///
    /// `idle_ttl_secs` overrides the reconnect window stored on the lease; `None`
    /// keeps the window the endpoint configured when it renewed the lease.
    pub fn finish_playback_request(&self, owner: &str, outcome: PlaybackRequestOutcome, idle_ttl_secs: Option<u64>) {
        self.finish_playback_request_inner(owner, None, outcome, idle_ttl_secs);
    }

    pub fn finish_identified_playback_request(
        &self,
        owner: &str,
        request_id: PlaybackRequestId,
        outcome: PlaybackRequestOutcome,
    ) {
        self.finish_playback_request_inner(owner, Some(request_id), outcome, None);
    }

    fn finish_playback_request_inner(
        &self,
        owner: &str,
        request_id: Option<PlaybackRequestId>,
        outcome: PlaybackRequestOutcome,
        idle_ttl_secs: Option<u64>,
    ) {
        let _transition = self.lock_capacity_transition();
        // A delayed completion must not end a newer request of this playback.
        // Only active connections from a DIFFERENT request ID (or shared streams) prevent lease finish.
        let connections = self.read_connections();
        let has_other_active_connection = connections.by_owner.get(owner).is_some_and(|alloc_ids| {
            alloc_ids.iter().any(|id| {
                connections
                    .single
                    .get(id)
                    .is_some_and(|info| request_id.is_none() || info.playback_request_id != request_id)
                    || connections.shared.shared_by_allocation_id.contains_key(id)
            })
        });
        if has_other_active_connection {
            if let Some(req_id) = request_id {
                let mut leases = self.write_leases();
                leases.detach_identified_request(owner, req_id);
            }
            return;
        }
        drop(connections);
        let mut leases = self.write_leases();
        if let Some(req_id) = request_id {
            leases.finish_identified_owner(owner, req_id, outcome, idle_ttl_secs);
        } else {
            leases.finish_owner(owner, outcome, idle_ttl_secs);
        }
    }

    /// Snapshot of the logical slots a provider holds, for logs and UI metrics.
    pub fn provider_lease_usage(&self, provider_name: &Arc<str>) -> ProviderLeaseUsage {
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.usage(provider_name)
    }

    /// Removes every expired starting/idle lease. Runs from the session GC task.
    pub fn prune_expired_leases_now(&self) {
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
    }

    /// The provider lease's binding tag for an owner. The HLS layer captures this
    /// from the acquired handle (via `ProviderHandle::binding_tag`) and passes it
    /// back to [`Self::clear_identified_provider_reservation`] so a stale detach
    /// cannot delete a successor lease on the same account.
    pub fn binding_tag_for_owner(&self, owner: &str) -> Option<ProviderBindingTag> {
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.lease_of_owner(owner).map(|lease| ProviderBindingTag::new(lease.id, lease.binding_generation))
    }

    fn acquire_exact_connection_inner_no_preempt(
        &self,
        provider_name: &Arc<str>,
        allow_grace: bool,
        params: &AcquireProviderParams<'_>,
    ) -> Option<ProviderHandle> {
        let mut allocation = ProviderAllocationGuard::new(
            self.providers.acquire_exact_connection_with_grace_override(provider_name, allow_grace),
        );
        if matches!(allocation.allocation(), ProviderAllocation::Exhausted) {
            return None;
        }
        if self.exceeds_reserved_capacity(provider_name, params.session_owner()) {
            return None;
        }
        Some(self.register_allocation(allocation.take(), params))
    }

    fn acquire_exact_connection_inner(
        &self,
        provider_name: &Arc<str>,
        allow_grace: bool,
        params: &AcquireProviderParams<'_>,
    ) -> Option<ProviderHandle> {
        if let Some(handle) = self.acquire_exact_connection_inner_no_preempt(provider_name, allow_grace, params) {
            return Some(handle);
        }
        if let Some(preempted_alloc) = self.try_preempt_connection(
            provider_name,
            params.priority,
            allow_grace,
            params.kind,
            params.session_owner(),
        ) {
            return Some(self.register_allocation(preempted_alloc, params));
        }
        None
    }

    fn finalize_lineup_allocation(
        &self,
        input_name: &Arc<str>,
        allow_grace: bool,
        mut allocation: ProviderAllocationGuard,
        params: &AcquireProviderParams<'_>,
    ) -> ProviderHandle {
        if matches!(allocation.allocation(), ProviderAllocation::GracePeriod(_))
            && self.evict_lower_priority_on_input(input_name, params.priority, params.kind, params.session_owner())
        {
            let evicted_on_same =
                !self.providers.is_over_limit(&allocation.allocation().get_provider_name().unwrap_or_default());
            if !evicted_on_same {
                let mut new_alloc = ProviderAllocationGuard::new(
                    self.providers.acquire_connection_with_grace_override(input_name, allow_grace),
                );
                if !matches!(new_alloc.allocation(), ProviderAllocation::Exhausted) {
                    if let Some(provider_name) = new_alloc.allocation().get_provider_name() {
                        if !self.exceeds_reserved_capacity(&provider_name, params.session_owner()) {
                            return self.register_allocation(new_alloc.take(), params);
                        }
                    }
                }
            }
        }

        self.register_allocation(allocation.take(), params)
    }

    fn select_victim_from_index(
        &self,
        index: &HashMap<Arc<str>, BTreeMap<PriorityKey, PriorityOwner>>,
        input_name: &Arc<str>,
        requester_priority: Option<i8>,
        reserved_providers: &HashSet<Arc<str>>,
    ) -> Option<PreemptionCandidate> {
        let mut victim = None;

        for (prov_name, tree) in index {
            if (!self.providers.is_provider_for_input(prov_name, input_name) && prov_name != input_name)
                || reserved_providers.contains(prov_name)
            {
                continue;
            }

            let Some(((victim_priority, Reverse(created_at), allocation_id), owner)) = tree.iter().next_back() else {
                continue;
            };

            if let Some(req_prio) = requester_priority {
                if *victim_priority <= req_prio {
                    continue;
                }
            }

            let candidate = (*owner, *allocation_id, *victim_priority, *created_at);
            if is_better_preemption_candidate(victim, candidate) {
                victim = Some(candidate);
            }
        }

        victim
    }

    fn is_normal_preemption_candidate(
        connections: &Connections,
        provider_name: &Arc<str>,
        candidate: PreemptionCandidate,
    ) -> bool {
        match candidate.0 {
            PriorityOwner::Single(alloc_id) => connections.single.get(&alloc_id).is_some_and(|info| {
                info.kind == ConnectionKind::Normal
                    && info.lifecycle != ConnectionLifecycle::Closing
                    && info.allocation.get_provider_name().as_ref() == Some(provider_name)
            }),
            PriorityOwner::Shared(shared_id) => connections
                .shared
                .shared_by_allocation_id
                .get(&shared_id)
                .and_then(|key| connections.shared.by_key.get(key))
                .is_some_and(|shared| {
                    shared.allocation_id == candidate.1
                        && shared.kind == ConnectionKind::Normal
                        && shared.allocation.get_provider_name().as_ref() == Some(provider_name)
                }),
        }
    }

    fn select_preemption_candidate(
        &self,
        connections: &Connections,
        input_name: &Arc<str>,
        new_priority: i8,
        kind_needed: ConnectionKind,
        reserved_providers: &HashSet<Arc<str>>,
    ) -> Option<PreemptionCandidate> {
        match kind_needed {
            ConnectionKind::Normal => {
                let soft_victim = self.select_victim_from_index(
                    &connections.soft_priority_index,
                    input_name,
                    None,
                    reserved_providers,
                );
                if soft_victim.is_some() {
                    return soft_victim;
                }

                let mut victim = None;
                for (prov_name, tree) in &connections.priority_index {
                    if (!self.providers.is_provider_for_input(prov_name, input_name) && prov_name != input_name)
                        || reserved_providers.contains(prov_name)
                    {
                        continue;
                    }

                    let Some(((victim_priority, Reverse(created_at), allocation_id), owner)) = tree.iter().next_back()
                    else {
                        continue;
                    };
                    if *victim_priority <= new_priority {
                        continue;
                    }
                    let candidate = (*owner, *allocation_id, *victim_priority, *created_at);
                    if !ActiveProviderManager::is_normal_preemption_candidate(connections, prov_name, candidate) {
                        continue;
                    }
                    if is_better_preemption_candidate(victim, candidate) {
                        victim = Some(candidate);
                    }
                }
                victim
            }
            ConnectionKind::Soft => self.select_victim_from_index(
                &connections.soft_priority_index,
                input_name,
                Some(new_priority),
                reserved_providers,
            ),
        }
    }

    fn acquire_connection_inner_no_preempt(
        &self,
        provider_or_input_name: &Arc<str>,
        allow_grace: bool,
        params: &AcquireProviderParams<'_>,
    ) -> Option<ProviderHandle> {
        if let Some(owner) = params.session_owner() {
            if let Some(reserved_provider) = self.get_reserved_provider_for_owner(provider_or_input_name, owner) {
                return self.acquire_exact_connection_inner_no_preempt(&reserved_provider, allow_grace, params);
            }
        }

        let candidate_count = self.providers.provider_names_for_input(provider_or_input_name).len();
        let attempts = candidate_count.max(1);
        let mut skipped_reserved = HashSet::new();
        for _ in 0..attempts {
            let allocation =
                ProviderAllocationGuard::new(self.providers.acquire_connection_with_grace_override_excluding(
                    provider_or_input_name,
                    allow_grace,
                    &skipped_reserved,
                ));
            if matches!(allocation.allocation(), ProviderAllocation::Exhausted) {
                break;
            }
            if let Some(provider_name) = allocation.allocation().get_provider_name() {
                if self.exceeds_reserved_capacity(&provider_name, params.session_owner()) {
                    self.log_reserved_capacity_skip(&provider_name, params);
                    skipped_reserved.insert(provider_name);
                    if skipped_reserved.len() >= attempts {
                        break;
                    }
                    continue;
                }
            }
            return Some(self.finalize_lineup_allocation(provider_or_input_name, allow_grace, allocation, params));
        }

        None
    }

    fn acquire_connection_inner(
        &self,
        provider_or_input_name: &Arc<str>,
        allow_grace: bool,
        params: &AcquireProviderParams<'_>,
    ) -> Option<ProviderHandle> {
        if self.is_shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return None;
        }
        if let Some(handle) = self.acquire_connection_inner_no_preempt(provider_or_input_name, allow_grace, params) {
            return Some(handle);
        }

        if let Some(preempted_alloc) = self.try_preempt_connection(
            provider_or_input_name,
            params.priority,
            allow_grace,
            params.kind,
            params.session_owner(),
        ) {
            return Some(self.register_allocation(preempted_alloc, params));
        }

        None
    }

    fn register_allocation(
        &self,
        allocation: ProviderAllocation,
        params: &AcquireProviderParams<'_>,
    ) -> ProviderHandle {
        let AcquireProviderParams { addr, priority, kind, lease } = *params;
        let session_owner = lease.map(|lease| lease.owner);
        let provider_name = allocation.get_provider_name().unwrap_or_default();
        let allocation_id = self.next_allocation_id.fetch_add(1, Ordering::Relaxed);
        let cancel_token = CancellationToken::new();
        let completion_token = CancellationToken::new();
        let close_reason = Arc::new(AtomicU8::new(ProviderCloseReason::Unspecified as u8));
        let now = Instant::now();
        let is_unlimited = allocation.is_unlimited_provider();

        // Claim the playback's capacity slot lease in its own lock scope so the
        // connection and lease locks are never nested. The lease starts unconfirmed;
        // it only reserves capacity against other playbacks once real media activity
        // confirms it, which is what keeps abandoned manifest starts from blocking.
        let binding_tag = if let Some(lease) = lease {
            let mut leases = self.write_leases();
            Self::prune_expired_leases(&mut leases);
            let id = leases.begin_owner(lease.owner, &provider_name, lease.kind, lease.request_id);
            let generation = leases.lease(id).map_or(1, |lease| lease.binding_generation);
            debug_if_enabled!(
                "Playback lease began: provider={} owner={} kind={} request_id={} lease_id={} generation={} state=starting",
                sanitize_sensitive_info(&provider_name),
                sanitize_sensitive_info(lease.owner),
                lease.kind,
                lease.request_id,
                id,
                generation
            );
            Some(ProviderBindingTag::new(id, generation))
        } else {
            None
        };

        let session_owner_arc = session_owner.map(Arc::from);
        let mut connections = self.write_connections();
        connections.single.insert(
            allocation_id,
            ActiveConnectionInfo {
                allocation_id,
                client_addr: *addr,
                allocation: allocation.clone(),
                cancel_token: cancel_token.clone(),
                completion_token: completion_token.clone(),
                close_reason: Arc::clone(&close_reason),
                lifecycle: ConnectionLifecycle::Active,
                has_body_owner: false,
                reaper_spawned: false,
                open_generation: 0,
                created_at: now,
                priority,
                kind,
                session_owner: session_owner_arc.clone(),
                playback_request_id: lease.map(|lease| lease.request_id),
            },
        );
        connections.single_by_addr.entry(*addr).or_default().insert(allocation_id);
        Self::index_owner(&mut connections, allocation_id, session_owner_arc.as_ref());

        // Unlimited providers are not subject to preemption, so we deliberately
        // skip populating the by_provider / priority_index / soft_priority_index
        // indices. The connection itself is still tracked via `single`, so all
        // capacity and lifecycle invariants hold. Without this skip, an
        // unlimited provider's connection can be selected as a preemption victim
        // even though it cannot be exhausted, contradicting the configured
        // `max_connections: 0` semantics.
        if !is_unlimited {
            connections.by_provider.entry(provider_name.clone()).or_default().insert(allocation_id);
            Self::upsert_priority_entry(
                &mut connections,
                &provider_name,
                (priority, Reverse(now), allocation_id),
                PriorityOwner::Single(allocation_id),
                kind,
            );
        }

        debug_if_enabled!(
            "Added provider connection {provider_name:?} (reason={}, prio={priority}, kind={kind:?}, unlimited={is_unlimited}, allocation_id={allocation_id}, lease_id={}, request_id={}, playback_kind={}, peer_addr={})",
            PlaybackSelectionReason::NewPriorityAllocation,
            binding_tag.map_or_else(String::new, |tag| tag.lease_id.to_string()),
            lease.map_or_else(String::new, |lease| lease.request_id.to_string()),
            lease.map_or_else(|| "-".to_string(), |lease| lease.kind.to_string()),
            sanitize_sensitive_info(&addr.to_string())
        );
        let mut handle = ProviderHandle::new(*addr, allocation_id, allocation, Some(cancel_token));
        handle.completion_token = Some(completion_token);
        handle.close_reason = close_reason;
        handle.playback_request_id = lease.map(|lease| lease.request_id);
        handle.binding_tag = binding_tag;
        handle
    }

    #[allow(clippy::too_many_lines)]
    /// Evict a single lower-priority connection across the entire input lineup
    /// (all provider aliases). Used when a `GracePeriod` allocation was granted.
    /// Returns true if a victim was successfully evicted.
    fn evict_lower_priority_on_input(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
        kind_needed: ConnectionKind,
        session_owner: Option<&str>,
    ) -> bool {
        let reserved_providers = self.reserved_provider_names_for_other(input_name, session_owner);

        let victim = {
            let connections = self.read_connections();
            self.select_preemption_candidate(&connections, input_name, new_priority, kind_needed, &reserved_providers)
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
                    let mut connections = self.write_connections();
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
                        Self::unindex_owner(&mut connections, shared.allocation_id, shared.session_owner.as_ref());
                        for subscriber_id in shared.connections.keys() {
                            connections.shared.key_by_subscriber.remove(subscriber_id);
                        }
                        if let Some(name) = shared.allocation.get_provider_name() {
                            if let Some(list) = connections.by_provider.get_mut(&name) {
                                list.remove(&shared.allocation_id);
                            }
                            Self::remove_priority_entry(
                                &mut connections,
                                &name,
                                &(v_prio, Reverse(victim_created_at), alloc_id),
                                shared.kind,
                            );
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
                    if let Some(ssm) = self.shared_stream_manager.get().and_then(Weak::upgrade) {
                        tokio::spawn(async move {
                            ssm.teardown_preempted_stream(&stream_url, alloc_id).await;
                            allocation.release();
                        });
                        return false;
                    }
                    allocation.release();
                }
            }
            PriorityOwner::Single(victim_alloc_id) => {
                let mut connections = self.write_connections();
                if let Some(info) = connections.single.get(&victim_alloc_id) {
                    if info.priority != v_prio || info.created_at != victim_created_at {
                        return false;
                    }
                }
                if let Some(info) = connections.single.get_mut(&victim_alloc_id) {
                    debug_if_enabled!(
                        "Grace-evicting single connection from {} (prio={}) on input {} for higher priority request (prio={})",
                        sanitize_sensitive_info(&info.client_addr.to_string()),
                        v_prio,
                        sanitize_sensitive_info(input_name),
                        new_priority
                    );
                    info.close_reason.store(ProviderCloseReason::PriorityPreempted as u8, Ordering::Release);
                }
                let action = ActiveProviderManagerCore::plan_single_release_locked(&mut connections, victim_alloc_id);
                if let ReleaseAction::Wait(alloc_id, gen, completion_token) = action {
                    let shutdown_token = self.shutdown_token.clone();
                    let core = Arc::clone(&self.core);
                    tokio::spawn(async move {
                        tokio::select! {
                            () = shutdown_token.cancelled() => {},
                            () = completion_token.cancelled() => {
                                core.complete_release_with_generation(alloc_id, Some(gen));
                            }
                        }
                    });
                    return false;
                }
            }
        }

        true
    }

    fn try_acquire_allocation_after_freed(
        &self,
        input_name: &Arc<str>,
        allow_grace: bool,
        reserved_providers: &HashSet<Arc<str>>,
        session_owner: Option<&str>,
    ) -> Option<ProviderAllocation> {
        let attempts = self.providers.provider_names_for_input(input_name).len().max(1);
        let mut excluded_providers = reserved_providers.clone();
        for _ in 0..attempts {
            let allocation = self.providers.acquire_connection_with_grace_override_excluding(
                input_name,
                allow_grace,
                &excluded_providers,
            );
            if matches!(allocation, ProviderAllocation::Exhausted) {
                break;
            }
            if let Some(provider_name) = allocation.get_provider_name() {
                if self.exceeds_reserved_capacity(&provider_name, session_owner) {
                    excluded_providers.insert(provider_name);
                    allocation.release();
                    continue;
                }
            }
            return Some(allocation);
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    fn try_preempt_connection_outcome(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
        allow_grace: bool,
        kind_needed: ConnectionKind,
        session_owner: Option<&str>,
    ) -> PreemptionOutcome {
        let reserved_providers = self.reserved_provider_names_for_other(input_name, session_owner);
        let victim = {
            let connections = self.read_connections();
            self.select_preemption_candidate(&connections, input_name, new_priority, kind_needed, &reserved_providers)
        };

        let Some((owner, alloc_id, v_prio, victim_created_at)) = victim else {
            return PreemptionOutcome::Exhausted;
        };

        match owner {
            PriorityOwner::Shared(shared_id) => {
                debug_if_enabled!(
                    "Preempting shared connection (allocation_id={shared_id}, prio={v_prio}) for higher priority request (prio={new_priority})"
                );
                let released_shared_allocation = {
                    let mut connections = self.write_connections();
                    let Some(key) = connections.shared.shared_by_allocation_id.get(&shared_id).cloned() else {
                        return PreemptionOutcome::Exhausted;
                    };

                    let still_match = connections.shared.by_key.get(&key).is_some_and(|shared| {
                        shared.allocation_id == alloc_id
                            && shared.priority == v_prio
                            && shared.created_at == victim_created_at
                    });
                    if !still_match {
                        return PreemptionOutcome::Exhausted;
                    }

                    if let Some(shared) = connections.shared.by_key.remove(&key) {
                        connections.shared.shared_by_allocation_id.remove(&shared.allocation_id);
                        Self::unindex_owner(&mut connections, shared.allocation_id, shared.session_owner.as_ref());
                        for subscriber_id in shared.connections.keys() {
                            connections.shared.key_by_subscriber.remove(subscriber_id);
                        }

                        if let Some(name) = shared.allocation.get_provider_name() {
                            if let Some(list) = connections.by_provider.get_mut(&name) {
                                list.remove(&shared.allocation_id);
                            }
                            Self::remove_priority_entry(
                                &mut connections,
                                &name,
                                &(v_prio, Reverse(victim_created_at), alloc_id),
                                shared.kind,
                            );
                        }
                        Some((key, shared.allocation, shared.cancel_token))
                    } else {
                        None
                    }
                };

                let Some((stream_url, allocation, cancel_token)) = released_shared_allocation else {
                    return PreemptionOutcome::Exhausted;
                };

                if let Some(token) = cancel_token {
                    token.cancel();
                }
                if let Some(ssm) = self.shared_stream_manager.get().and_then(Weak::upgrade) {
                    // Signal completion after teardown finishes so async acquirers can wait for the
                    // actual upstream shutdown rather than receiving Exhausted immediately.
                    let done_token = CancellationToken::new();
                    let done_signal = done_token.clone();
                    tokio::spawn(async move {
                        ssm.teardown_preempted_stream(&stream_url, alloc_id).await;
                        allocation.release();
                        done_signal.cancel();
                    });
                    PreemptionOutcome::PendingCompletion(None, done_token)
                } else {
                    allocation.release();
                    if let Some(alloc) = self.try_acquire_allocation_after_freed(
                        input_name,
                        allow_grace,
                        &reserved_providers,
                        session_owner,
                    ) {
                        PreemptionOutcome::Acquired(alloc)
                    } else {
                        PreemptionOutcome::Exhausted
                    }
                }
            }
            PriorityOwner::Single(victim_alloc_id) => {
                let (action, completion_token) = {
                    let mut connections = self.write_connections();
                    if let Some(info) = connections.single.get(&victim_alloc_id) {
                        if info.priority != v_prio || info.created_at != victim_created_at {
                            return PreemptionOutcome::Exhausted;
                        }
                    } else {
                        return PreemptionOutcome::Exhausted;
                    }
                    if let Some(info) = connections.single.get_mut(&victim_alloc_id) {
                        debug_if_enabled!(
                            "Preempting single connection from {} (prio={v_prio}) for higher priority request (prio={new_priority})",
                            sanitize_sensitive_info(&info.client_addr.to_string())
                        );
                        info.close_reason.store(ProviderCloseReason::PriorityPreempted as u8, Ordering::Release);
                    }
                    let completion_token =
                        connections.single.get(&victim_alloc_id).map(|info| info.completion_token.clone());
                    let action =
                        ActiveProviderManagerCore::plan_single_release_locked(&mut connections, victim_alloc_id);
                    (action, completion_token)
                };

                match action {
                    ReleaseAction::Wait(victim_id, gen, comp_token) => {
                        let shutdown_token = self.shutdown_token.clone();
                        let core = Arc::clone(&self.core);
                        let reaper_token = comp_token.clone();
                        tokio::spawn(async move {
                            tokio::select! {
                                () = shutdown_token.cancelled() => {},
                                () = reaper_token.cancelled() => {
                                    core.complete_release_with_generation(victim_id, Some(gen));
                                }
                            }
                        });
                        PreemptionOutcome::PendingCompletion(Some((victim_id, gen)), comp_token)
                    }
                    ReleaseAction::None => {
                        if let Some(token) = completion_token {
                            // Retrieve the current generation for the idempotent self-release path.
                            let victim_identity = {
                                let connections = self.read_connections();
                                connections
                                    .single
                                    .get(&victim_alloc_id)
                                    .map(|info| (victim_alloc_id, info.open_generation))
                            };
                            PreemptionOutcome::PendingCompletion(victim_identity, token)
                        } else {
                            PreemptionOutcome::Exhausted
                        }
                    }
                    ReleaseAction::Immediate => {
                        if let Some(alloc) = self.try_acquire_allocation_after_freed(
                            input_name,
                            allow_grace,
                            &reserved_providers,
                            session_owner,
                        ) {
                            PreemptionOutcome::Acquired(alloc)
                        } else {
                            PreemptionOutcome::Exhausted
                        }
                    }
                }
            }
        }
    }

    fn try_preempt_connection(
        &self,
        input_name: &Arc<str>,
        new_priority: i8,
        allow_grace: bool,
        kind_needed: ConnectionKind,
        session_owner: Option<&str>,
    ) -> Option<ProviderAllocation> {
        match self.try_preempt_connection_outcome(input_name, new_priority, allow_grace, kind_needed, session_owner) {
            PreemptionOutcome::Acquired(alloc) => Some(alloc),
            _ => None,
        }
    }

    pub fn acquire_exact_connection_with_grace(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
    ) -> Option<ProviderHandle> {
        self.acquire_exact_connection_with_grace_for_session(provider_name, addr, allow_grace, priority, kind, None)
    }

    pub fn acquire_exact_connection_with_grace_for_session(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        if self.is_shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let lease = session_owner.map(PlaybackLeaseRef::for_session_owner);
        self.acquire_exact_connection_inner(
            provider_name,
            allow_grace,
            &AcquireProviderParams { addr, priority, kind, lease: lease.as_ref().copied() },
        )
    }

    pub fn force_exact_acquire_connection(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        priority: i8,
        kind: ConnectionKind,
    ) -> Option<ProviderHandle> {
        // Compatibility wrapper: keep the exact-provider behavior but do not over-allocate exhausted accounts.
        self.acquire_exact_connection_with_grace(provider_name, addr, false, priority, kind)
    }

    // Returns the next available provider connection
    pub fn acquire_connection(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        priority: i8,
        kind: ConnectionKind,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, true, &AcquireProviderParams { addr, priority, kind, lease: None })
    }

    /// Acquire a provider connection while explicitly controlling provider-side grace allocations.
    pub fn acquire_connection_with_grace(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_with_grace_for_session(input_name, addr, allow_grace, priority, kind, None)
    }

    pub fn acquire_connection_with_grace_for_session(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        session_owner: Option<&str>,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_with_lease_for_session(
            input_name,
            addr,
            allow_grace,
            priority,
            kind,
            session_owner.map(PlaybackLeaseRef::for_session_owner),
        )
    }

    /// Lineup acquisition with an explicit playback lease identity.
    pub fn acquire_connection_with_lease_for_session(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        lease: Option<PlaybackLeaseRef<'_>>,
    ) -> Option<ProviderHandle> {
        self.acquire_connection_inner(input_name, allow_grace, &AcquireProviderParams { addr, priority, kind, lease })
    }

    /// Lineup acquisition with an explicit playback lease identity, awaiting preemption completion if needed.
    pub async fn acquire_connection_with_lease_for_session_await(
        &self,
        input_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        lease: Option<PlaybackLeaseRef<'_>>,
    ) -> Option<ProviderHandle> {
        let params = AcquireProviderParams { addr, priority, kind, lease };
        let outcome = {
            if self.is_shutting_down.load(Ordering::Acquire) {
                return None;
            }
            let _transition = self.lock_capacity_transition();
            if self.is_shutting_down.load(Ordering::Acquire) {
                return None;
            }
            if let Some(handle) = self.acquire_connection_inner_no_preempt(input_name, allow_grace, &params) {
                return Some(handle);
            }
            self.try_preempt_connection_outcome(
                input_name,
                params.priority,
                allow_grace,
                params.kind,
                params.session_owner(),
            )
        };

        match outcome {
            PreemptionOutcome::Acquired(alloc) => {
                let _transition = self.lock_capacity_transition();
                Some(self.register_allocation(alloc, &params))
            }
            PreemptionOutcome::PendingCompletion(victim_identity, completion_token) => {
                let _ = tokio::time::timeout(PREEMPTION_COMPLETION_TIMEOUT, completion_token.cancelled()).await;
                if self.is_shutting_down.load(Ordering::Acquire) {
                    return None;
                }
                let _transition = self.lock_capacity_transition();
                // Idempotently free the victim's slot in case the background reaper has not yet run.
                // complete_release_locked is a no-op if the slot was already freed or the generation
                // no longer matches.
                if let Some((victim_alloc_id, victim_gen)) = victim_identity {
                    let mut connections = self.write_connections();
                    ActiveProviderManagerCore::complete_release_locked(
                        &mut connections,
                        victim_alloc_id,
                        Some(victim_gen),
                    );
                }
                self.acquire_connection_inner_no_preempt(input_name, allow_grace, &params)
            }
            PreemptionOutcome::Exhausted => None,
        }
    }

    /// Exact-provider acquisition with an explicit playback lease identity.
    pub fn acquire_exact_connection_with_lease_for_session(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        lease: Option<PlaybackLeaseRef<'_>>,
    ) -> Option<ProviderHandle> {
        let _transition = self.lock_capacity_transition();
        self.acquire_exact_connection_inner(
            provider_name,
            allow_grace,
            &AcquireProviderParams { addr, priority, kind, lease },
        )
    }

    /// Exact-provider acquisition with an explicit playback lease identity, awaiting preemption completion if needed.
    pub async fn acquire_exact_connection_with_lease_for_session_await(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        lease: Option<PlaybackLeaseRef<'_>>,
    ) -> Option<ProviderHandle> {
        let params = AcquireProviderParams { addr, priority, kind, lease };
        let outcome = {
            if self.is_shutting_down.load(Ordering::Acquire) {
                return None;
            }
            let _transition = self.lock_capacity_transition();
            if self.is_shutting_down.load(Ordering::Acquire) {
                return None;
            }
            if let Some(handle) = self.acquire_exact_connection_inner_no_preempt(provider_name, allow_grace, &params) {
                return Some(handle);
            }
            self.try_preempt_connection_outcome(
                provider_name,
                params.priority,
                allow_grace,
                params.kind,
                params.session_owner(),
            )
        };

        match outcome {
            PreemptionOutcome::Acquired(alloc) => {
                let _transition = self.lock_capacity_transition();
                Some(self.register_allocation(alloc, &params))
            }
            PreemptionOutcome::PendingCompletion(victim_identity, completion_token) => {
                let _ = tokio::time::timeout(PREEMPTION_COMPLETION_TIMEOUT, completion_token.cancelled()).await;
                if self.is_shutting_down.load(Ordering::Acquire) {
                    return None;
                }
                let _transition = self.lock_capacity_transition();
                if let Some((victim_alloc_id, victim_gen)) = victim_identity {
                    let mut connections = self.write_connections();
                    ActiveProviderManagerCore::complete_release_locked(
                        &mut connections,
                        victim_alloc_id,
                        Some(victim_gen),
                    );
                }
                self.acquire_exact_connection_inner_no_preempt(provider_name, allow_grace, &params)
            }
            PreemptionOutcome::Exhausted => None,
        }
    }

    /// Acquire a provider connection for probe tasks with configurable priority.
    /// Probes never consume grace capacity.
    pub fn acquire_connection_for_probe(&self, input_name: &Arc<str>, priority: i8) -> Option<ProviderHandle> {
        self.acquire_connection_inner(
            input_name,
            false,
            &AcquireProviderParams { addr: &DUMMY_ADDR, priority, kind: ConnectionKind::Normal, lease: None },
        )
    }

    /// Acquire a provider connection for background transfers (downloads/recordings).
    /// Transfers participate in the same provider priority/preemption model as normal
    /// streams, but they never consume grace capacity and wait externally on notifications.
    pub fn acquire_connection_for_download(&self, input_name: &Arc<str>, priority: i8) -> Option<ProviderHandle> {
        self.acquire_connection_inner(
            input_name,
            false,
            &AcquireProviderParams { addr: &DUMMY_ADDR, priority, kind: ConnectionKind::Normal, lease: None },
        )
    }

    // This method is used for redirects to cycle through the provider
    pub fn get_next_provider(&self, provider_name: &Arc<str>) -> Option<Arc<ProviderConfig>> {
        self.providers.get_next_provider(provider_name)
    }

    pub fn find_provider_config(&self, provider_name: &Arc<str>) -> Option<Arc<ProviderConfig>> {
        self.providers.find_provider_config(provider_name)
    }

    pub fn is_provider_for_input(&self, provider_name: &Arc<str>, input_name: &Arc<str>) -> bool {
        self.providers.is_provider_for_input(provider_name.as_ref(), input_name.as_ref())
    }

    pub fn is_provider_reserved_for_other_session(
        &self,
        provider_name: &Arc<str>,
        session_owner: Option<&str>,
    ) -> bool {
        self.reserved_capacity_blocks_next(provider_name, session_owner)
    }

    pub fn active_connections(&self) -> Option<HashMap<Arc<str>, usize>> { self.providers.active_connections() }

    pub fn is_over_limit(&self, provider_name: &Arc<str>) -> bool { self.providers.is_over_limit(provider_name) }

    pub fn is_exhausted(&self, provider_name: &Arc<str>) -> bool { self.providers.is_exhausted(provider_name) }

    /// Finds every shared subscription carried by one transport socket.
    ///
    /// This is an explicit socket-wide transport action (connection close, kick), not
    /// the normal playback cleanup path. A single shared subscription is released
    /// through `release_shared_subscriber`, which cannot touch other clients that
    /// happen to arrive through the same reverse-proxy socket.
    fn release_shared_by_addr(&self, addr: &SocketAddr) -> Vec<SharedSubscriberId> {
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

    fn release_shared_connection_inner(&self, subscriber_id: SharedSubscriberId) {
        if let Some(allocation) = self.release_shared_subscriber(subscriber_id) {
            debug_if_enabled!(
                "Released last shared connection for provider {} (subscriber={subscriber_id})",
                allocation.get_provider_name().unwrap_or_default()
            );
            allocation.release();
        }
    }

    pub fn register_body_owner(&self, alloc_id: AllocationId) -> bool {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        if let Some(info) = connections.single.get_mut(&alloc_id) {
            if info.lifecycle == ConnectionLifecycle::Closing || info.lifecycle == ConnectionLifecycle::Closed {
                return false;
            }
            info.has_body_owner = true;
            if info.lifecycle == ConnectionLifecycle::Opening {
                info.lifecycle = ConnectionLifecycle::Active;
            }
            true
        } else {
            false
        }
    }

    pub fn mark_opening(&self, alloc_id: AllocationId) {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        if let Some(info) = connections.single.get_mut(&alloc_id) {
            if info.lifecycle != ConnectionLifecycle::Closing && info.lifecycle != ConnectionLifecycle::Closed {
                info.lifecycle = ConnectionLifecycle::Opening;
            }
        }
    }

    pub fn renew_opening_tokens(&self, alloc_id: AllocationId) -> Option<(CancellationToken, CancellationToken, u64)> {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        if let Some(info) = connections.single.get_mut(&alloc_id) {
            if info.lifecycle != ConnectionLifecycle::Closing && info.lifecycle != ConnectionLifecycle::Closed {
                info.lifecycle = ConnectionLifecycle::Opening;
                info.open_generation = info.open_generation.wrapping_add(1);
                info.reaper_spawned = false;
                let cancel_token = CancellationToken::new();
                let completion_token = CancellationToken::new();
                info.cancel_token = cancel_token.clone();
                info.completion_token = completion_token.clone();
                return Some((cancel_token, completion_token, info.open_generation));
            }
        }
        None
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

    fn release_shared_handle_locked(
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
        }
        released
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
                            if info.session_owner.as_deref() == Some(owner) {
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
                            if info.session_owner.as_deref() == Some(owner)
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
                if info.session_owner.as_deref() != owner {
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

    pub fn get_provider_connections_count(&self) -> usize { self.providers.active_connection_count() }

    pub fn provider_capacities_for_input(&self, input_name: &Arc<str>) -> Vec<(Arc<str>, usize, usize)> {
        let mut result = Vec::new();
        for provider_name in self.providers.provider_names_for_input(input_name) {
            if let Some((current, max)) = self.providers.provider_capacity(&provider_name) {
                result.push((provider_name, current, max));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveProviderManager, ConnectionKind, PlaybackLeaseRef};
    use crate::{ActiveUserManager, EventManager, SharedStreamManager};
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        defaults::{default_probe_user_priority, default_user_priority},
        model::{ConfigPaths, InputFetchMethod, InputType},
        utils::Internable,
    };
    use std::{
        collections::{HashMap, HashSet},
        net::SocketAddr,
        sync::{Arc, Weak},
        time::Duration,
    };
    use tuliprox_core::{
        model::{
            AppConfig, Config, ConfigInput, ConfigInputAlias, MediaToolCapabilities, PlaybackKind,
            PlaybackRequestOutcome, ProviderAllocation, SharedSubscriberId, SourcesConfig,
        },
        utils::FileLockManager,
    };

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
            media_tools: Arc::new(MediaToolCapabilities::new()),
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
                stalker: None,
            }]),
            1,
        )
    }

    fn create_test_app_config_with_capacity_ordered_pool() -> AppConfig {
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
                stalker: None,
            }]),
            3,
        )
    }

    fn create_test_app_config_single_provider_pool() -> AppConfig { build_test_app_config(None, 1) }

    fn create_test_app_config_single_unlimited_provider_pool() -> AppConfig { build_test_app_config(None, 0) }

    /// Pool where the higher-priority provider (A) and its lower-priority alias (B)
    /// each carry their own capacity, mirroring the reported A=2 / B=3 setup.
    fn create_test_app_config_with_pool(primary_max: u16, alias_max: u16) -> AppConfig {
        build_test_app_config(
            Some(vec![ConfigInputAlias {
                id: 2,
                name: "provider_2".intern(),
                url: "http://provider-2.example".to_string(),
                username: Some("user2".to_string()),
                password: Some("pass2".to_string()),
                priority: 1,
                max_connections: alias_max,
                exp_date: None,
                enabled: true,
                stalker: None,
            }]),
            primary_max,
        )
    }

    #[test]
    fn shared_stream_manager_backref_is_weak_and_does_not_leak() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared = Arc::new(SharedStreamManager::new(Arc::clone(&provider)));
        provider.set_shared_stream_manager(&shared);

        // The only strong reference to the shared manager is the local `shared`; the
        // provider holds a Weak backref, so dropping `shared` releases both managers.
        drop(shared);
        assert!(provider.shared_stream_manager.get().and_then(Weak::upgrade).is_none());
    }

    #[tokio::test]
    async fn confirmed_playbacks_fill_higher_priority_provider_before_alias() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();

        let mut selected = Vec::new();
        for index in 0..5 {
            let owner = format!("playback-{index}");
            let addr = SocketAddr::from(([172, 18, 0, 9], 50_000 + index));
            let handle = manager
                .acquire_connection_with_grace_for_session(
                    &input_name,
                    &addr,
                    false,
                    default_user_priority(),
                    ConnectionKind::Normal,
                    Some(&owner),
                )
                .expect("pool has capacity for five confirmed playbacks");
            let provider = handle.allocation.get_provider_name().expect("provider name");
            // Only real media activity confirms a playback and lets it reserve capacity.
            manager.confirm_playback_activity(&owner);
            selected.push(provider.to_string());
        }

        assert_eq!(selected, ["provider_1", "provider_1", "provider_2", "provider_2", "provider_2"]);
    }

    #[tokio::test]
    async fn concurrent_proxy_requests_fill_exact_pool_capacity() -> Result<(), Box<dyn std::error::Error>> {
        let manager = Arc::new(ActiveProviderManager::new(
            &create_test_app_config_with_pool(2, 3),
            &Arc::new(EventManager::new()),
        ));
        let addr = SocketAddr::from(([127, 0, 0, 1], 50001));
        let mut tasks = tokio::task::JoinSet::new();
        for attempt in 0..32 {
            let manager = Arc::clone(&manager);
            tasks.spawn(async move {
                manager.acquire_connection_with_grace_for_session(
                    &Arc::from("provider_1"),
                    &addr,
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some(&format!("playback-{attempt}")),
                )
            });
        }
        let mut handles = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Some(handle) = result? {
                handles.push(handle);
            }
        }
        assert_eq!(handles.len(), 5);
        assert_eq!(
            handles.iter().filter(|h| h.allocation.get_provider_name().as_deref() == Some("provider_1")).count(),
            2
        );
        assert_eq!(
            handles.iter().filter(|h| h.allocation.get_provider_name().as_deref() == Some("provider_2")).count(),
            3
        );
        for handle in handles {
            manager.release_handle(&handle);
        }
        assert_eq!(manager.get_provider_connections_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn forced_reopen_and_late_cleanup_preserve_other_proxy_requests() -> Result<(), Box<dyn std::error::Error>> {
        let manager =
            ActiveProviderManager::new(&create_test_app_config_with_pool(2, 3), &Arc::new(EventManager::new()));
        let addr = SocketAddr::from(([127, 0, 0, 1], 50002));
        let input = Arc::from("provider_1");
        let first = manager
            .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("first"))
            .ok_or("first allocation missing")?;
        let other = manager
            .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("other"))
            .ok_or("other allocation missing")?;
        manager.release_playback_connections("first", &[addr]);
        assert!(first.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
        assert!(!other.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
        assert_eq!(manager.get_provider_connections_count(), 1);
        let replacement = manager
            .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("first"))
            .ok_or("replacement missing")?;
        manager.refresh_adaptive_playback_lease(&input, "first", tuliprox_core::model::PlaybackKind::LiveHls, 15);
        manager.confirm_playback_activity("first");
        manager.finish_playback_request("first", PlaybackRequestOutcome::ProviderFailed, None);
        assert_eq!(manager.provider_lease_usage(&input).active, 1);
        manager.release_handle(&replacement);
        manager.release_handle(&other);
        Ok(())
    }

    #[tokio::test]
    async fn release_snapshot_tracks_original_allocation_after_addr_reuse() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let events = Arc::new(EventManager::new());
        let provider = ActiveProviderManager::new(&app_cfg, &events);
        let addr = SocketAddr::from(([127, 0, 0, 1], 50_022));
        let input = "provider_1".intern();
        let original = provider
            .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("old"))
            .expect("original allocation");
        let original_snapshot = provider.release_snapshot_for_addr(&addr);
        assert!(!provider.wait_for_snapshot_release(&original_snapshot, Duration::ZERO).await);

        provider.release_handle(&original);
        let replacement = provider
            .acquire_connection_with_grace_for_session(&input, &addr, false, 0, ConnectionKind::Normal, Some("new"))
            .expect("replacement allocation");
        let replacement_snapshot = provider.release_snapshot_for_addr(&addr);
        assert!(provider.wait_for_snapshot_release(&original_snapshot, Duration::ZERO).await);
        assert!(!provider.wait_for_snapshot_release(&replacement_snapshot, Duration::ZERO).await);

        let geoip = Arc::new(ArcSwapOption::default());
        let users = ActiveUserManager::new(&Config::default(), &geoip, &events);
        users.set_pending_provider_release("user", original_snapshot.clone()).await;
        users.set_pending_provider_release("user", replacement_snapshot.clone()).await;
        assert!(!users.clear_pending_provider_release("user", &original_snapshot).await);
        assert_eq!(users.pending_provider_release("user").await, Some(replacement_snapshot.clone()));
        assert!(users.clear_pending_provider_release("user", &replacement_snapshot).await);
        assert_eq!(users.pending_provider_release("user").await, None);
        provider.release_handle(&replacement);
    }

    #[tokio::test]
    async fn release_snapshot_tracks_shared_subscriber_without_waiting_for_other_subscribers() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let events = Arc::new(EventManager::new());
        let provider = ActiveProviderManager::new(&app_cfg, &events);
        let input = "provider_1".intern();
        let first_addr = SocketAddr::from(([127, 0, 0, 1], 50_023));
        let second_addr = SocketAddr::from(([127, 0, 0, 1], 50_024));
        let first = SharedSubscriberId::from_stream_uid(50_023);
        let second = SharedSubscriberId::from_stream_uid(50_024);
        let origin =
            provider.acquire_connection(&input, &first_addr, 0, ConnectionKind::Normal).expect("shared origin");
        assert!(provider.make_shared_connection(&origin, "shared-release", first));
        provider
            .add_shared_connection(&second_addr, second, "shared-release", 0, ConnectionKind::Normal)
            .expect("second subscriber");
        let snapshot = provider.release_snapshot_for_addr(&first_addr);
        assert!(!provider.wait_for_snapshot_release(&snapshot, Duration::ZERO).await);

        provider.release_connection(&first_addr);
        assert!(provider.wait_for_snapshot_release(&snapshot, Duration::ZERO).await);
        assert_eq!(provider.get_provider_connections_count(), 1);
        provider.release_connection(&second_addr);
    }

    #[tokio::test(start_paused = true)]
    async fn manifest_retries_without_media_do_not_reserve_capacity() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "hls-cache:shared-session";
        let addr = SocketAddr::from(([172, 18, 0, 9], 51_000));

        // Five manifest responses without a single media segment, all on one owner.
        for _ in 0..5 {
            let handle = manager
                .acquire_connection_with_grace_for_session(
                    &input_name,
                    &addr,
                    false,
                    default_user_priority(),
                    ConnectionKind::Normal,
                    Some(owner),
                )
                .expect("manifest start allocates on the preferred provider");
            assert_eq!(handle.allocation.get_provider_name().as_deref(), Some("provider_1"));
            manager.refresh_provider_reservation(&input_name, owner, 15);
            manager.release_connection(&addr);
        }

        // Unconfirmed leases never reserve capacity, so an unrelated client is served
        // by the higher-priority provider even though its own counters read zero.
        let other = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &SocketAddr::from(([172, 18, 0, 9], 51_001)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("other-client"),
            )
            .expect("unrelated client must still get the preferred provider");
        assert_eq!(other.allocation.get_provider_name().as_deref(), Some("provider_1"));

        // The abandoned starts stop holding anything once their startup deadline passes.
        tokio::time::advance(Duration::from_secs(6)).await;
        manager.prune_expired_leases_now();
        assert!(!manager.is_provider_reserved_for_other_session(&input_name, Some("other-client")));
    }

    #[tokio::test(start_paused = true)]
    async fn identified_reservation_recreates_a_cleared_lease() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "hls-cache:restore-session";
        let addr = SocketAddr::from(([172, 18, 0, 9], 53_000));

        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner),
            )
            .expect("allocate");
        manager.refresh_identified_provider_reservation(
            &input_name,
            owner,
            tuliprox_core::model::PlaybackKind::Catchup,
            15,
        );
        assert_eq!(manager.provider_lease_usage(&input_name).starting, 1);

        // Clearing removes the lease entirely; recreating must bring it back rather
        // than silently no-op like a plain adaptive refresh would.
        let binding_tag = manager.binding_tag_for_owner(owner);
        manager.clear_identified_provider_reservation(owner, &input_name, binding_tag);
        assert_eq!(manager.provider_lease_usage(&input_name).total(), 0);

        manager.refresh_identified_provider_reservation(
            &input_name,
            owner,
            tuliprox_core::model::PlaybackKind::Catchup,
            15,
        );
        assert_eq!(manager.provider_lease_usage(&input_name).starting, 1);

        manager.release_handle(&handle);
    }

    #[tokio::test(start_paused = true)]
    async fn reservations_do_not_survive_after_counters_reach_zero() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "finished-playback";
        let addr = SocketAddr::from(([172, 18, 0, 9], 52_000));

        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner),
            )
            .expect("first playback acquires the preferred provider");
        assert_eq!(handle.allocation.get_provider_name().as_deref(), Some("provider_1"));
        manager.refresh_provider_reservation(&input_name, owner, 15);
        manager.confirm_playback_activity(owner);
        manager.release_connection(&addr);

        // Live TS is not reconnect capable: the confirmed lease is dropped on release,
        // so the provider is immediately free again for the next higher-priority client.
        manager.finish_playback_request(owner, PlaybackRequestOutcome::Completed, None);
        let next = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &SocketAddr::from(([172, 18, 0, 9], 52_001)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("next-client"),
            )
            .expect("freed capacity must be selectable again");
        assert_eq!(next.allocation.get_provider_name().as_deref(), Some("provider_1"));
    }

    #[tokio::test]
    async fn two_playbacks_sharing_one_proxy_socket_keep_separate_slots() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        // Both external clients arrive through the same reverse-proxy peer socket.
        let proxy_addr = SocketAddr::from(([172, 18, 0, 9], 53_000));

        let first = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &proxy_addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("device-one"),
            )
            .expect("first device behind the proxy acquires a slot");
        let second = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &proxy_addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("device-two"),
            )
            .expect("second device behind the proxy acquires its own slot");
        assert_ne!(first.allocation_id, second.allocation_id);
        manager.confirm_playback_activity("device-one");
        manager.confirm_playback_activity("device-two");

        // Releasing one playback must free exactly its own allocation and leave the
        // other device's slot on the same transport untouched.
        manager.release_handle(&first);
        manager.finish_playback_request("device-one", PlaybackRequestOutcome::Completed, None);
        let remaining = manager.provider_capacities_for_input(&input_name);
        let primary = remaining.iter().find(|(name, _, _)| name.as_ref() == "provider_1").expect("primary pool entry");
        assert_eq!(primary.1, 1, "exactly one slot must remain in use on the shared socket");
        assert!(manager.read_leases().lease_of_owner("device-one").is_none());
        assert!(manager.read_leases().lease_of_owner("device-two").is_some());

        manager.release_handle(&second);
        manager.finish_playback_request("device-two", PlaybackRequestOutcome::Completed, None);
    }

    #[tokio::test]
    async fn unlimited_provider_connection_is_not_in_priority_index() {
        let app_cfg = create_test_app_config_single_unlimited_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let addr_1: SocketAddr = "127.0.0.1:44001".parse().unwrap();
        let addr_2: SocketAddr = "127.0.0.1:44002".parse().unwrap();
        let addr_3: SocketAddr = "127.0.0.1:44003".parse().unwrap();

        manager
            .acquire_connection(&input_name, &addr_1, default_user_priority(), ConnectionKind::Normal)
            .expect("acquire #1 on unlimited provider");
        manager
            .acquire_connection(&input_name, &addr_2, default_user_priority(), ConnectionKind::Normal)
            .expect("acquire #2 on unlimited provider");
        manager
            .acquire_connection(&input_name, &addr_3, default_user_priority(), ConnectionKind::Normal)
            .expect("acquire #3 on unlimited provider");

        {
            let connections = manager.read_connections();
            let priority_tree = connections.priority_index.get(&input_name);
            assert!(
                priority_tree.is_none_or(std::collections::BTreeMap::is_empty),
                "unlimited provider must not be present in priority_index, found {priority_tree:?}"
            );
            let soft_tree = connections.soft_priority_index.get(&input_name);
            assert!(
                soft_tree.is_none_or(std::collections::BTreeMap::is_empty),
                "unlimited provider must not be present in soft_priority_index, found {soft_tree:?}"
            );
            let by_provider = connections.by_provider.get(&input_name);
            assert!(
                by_provider.is_none_or(std::collections::HashSet::is_empty),
                "unlimited provider must not be present in by_provider, found {by_provider:?}"
            );
        }

        manager.release_connection(&addr_1);
        manager.release_connection(&addr_2);
        manager.release_connection(&addr_3);
    }

    #[tokio::test]
    async fn preemption_does_not_select_unlimited_provider_as_victim() {
        let app_cfg = create_test_app_config_single_unlimited_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let low_addr: SocketAddr = "127.0.0.1:45001".parse().unwrap();
        let high_addr: SocketAddr = "127.0.0.1:45002".parse().unwrap();

        // Acquire one low-priority and one high-priority connection on the unlimited provider.
        let low = manager
            .acquire_connection(&input_name, &low_addr, 50, ConnectionKind::Normal)
            .expect("low-priority acquire on unlimited provider");
        let low_token = low.cancel_token.clone().expect("cancel token for low-priority connection");
        let _high = manager
            .acquire_connection(&input_name, &high_addr, 0, ConnectionKind::Normal)
            .expect("high-priority acquire on unlimited provider");

        // A request at an even higher priority must not be able to preempt the unlimited
        // provider's connection, because the connection is intentionally absent from the
        // preemption indices.
        let candidate = {
            let connections = manager.read_connections();
            manager.select_preemption_candidate(&connections, &input_name, 0, ConnectionKind::Normal, &HashSet::new())
        };
        assert!(
            candidate.is_none(),
            "select_preemption_candidate must not return an unlimited-provider connection as a victim, got {candidate:?}"
        );

        // The low-priority connection is still alive and its cancel token has not been fired.
        assert!(
            !low_token.is_cancelled(),
            "low-priority unlimited-provider connection must not be cancelled by preemption"
        );

        manager.release_connection(&low_addr);
        manager.release_connection(&high_addr);
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
            .acquire_connection(&input_name, &client_1_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("client1 initial allocation");
        let pinned_provider = first_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(pinned_provider.as_ref(), "provider_1");

        // provider_1 has max_connections=1 and is already in use by client1
        let forced = manager.force_exact_acquire_connection(
            &pinned_provider,
            &client_2_addr,
            default_user_priority(),
            ConnectionKind::Normal,
        );
        assert!(forced.is_none(), "forced exact acquire must not over-allocate busy provider");

        manager.release_connection(&client_1_addr);
        manager.release_connection(&client_2_addr);
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
        let first_alloc = manager
            .acquire_connection(&input_name, &client_1_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("client1 initial allocation");
        assert_eq!(first_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Step 2: Client1 stops -> release provider_1
        manager.release_connection(&client_1_addr);

        // Step 3: Client2 starts live -> provider_1
        let live_alloc = manager
            .acquire_connection(&input_name, &client_2_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("client2 live allocation");
        let busy_provider = live_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(busy_provider.as_ref(), input_name.as_ref());
        assert!(manager.is_exhausted(&busy_provider));

        // Step 4: Client1 restarts same movie.
        // This emulates force-session fallback path by acquiring without provider grace.
        let fallback_alloc = manager
            .acquire_connection_with_grace(&input_name, &client_1_addr, false, 0, ConnectionKind::Normal)
            .expect("client1 fallback allocation without grace");
        let fallback_provider = fallback_alloc.allocation.get_provider_name().expect("fallback provider expected");

        assert_ne!(fallback_provider.as_ref(), busy_provider.as_ref());
        assert_eq!(fallback_provider.as_ref(), "provider_2");

        manager.release_connection(&client_1_addr);
        manager.release_connection(&client_2_addr);
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
            .acquire_connection(&input_name, &client_1_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("client1 initial allocation");
        let pinned_provider = first_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(pinned_provider.as_ref(), "provider_1");

        // Another client occupies the alternate account while client1 keeps seeking.
        let second_alloc = manager
            .acquire_connection(&input_name, &client_2_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("client2 allocation");
        let second_provider = second_alloc.allocation.get_provider_name().expect("provider name expected");
        assert_eq!(second_provider.as_ref(), "provider_2");

        // Simulate repeated seek/range reconnects for client1:
        // release old connection for the same client, then force exact pinned provider.
        for _ in 0..3 {
            manager.release_connection(&client_1_addr);
            let seek_alloc = manager
                .force_exact_acquire_connection(
                    &pinned_provider,
                    &client_1_addr,
                    default_user_priority(),
                    ConnectionKind::Normal,
                )
                .expect("seek reacquire should stay on pinned provider");
            let seek_provider = seek_alloc.allocation.get_provider_name().expect("provider name expected");
            assert_eq!(seek_provider.as_ref(), pinned_provider.as_ref());
        }

        // Stream stop / cleanup.
        manager.release_connection(&client_1_addr);
        manager.release_connection(&client_2_addr);
    }

    #[tokio::test]
    async fn test_probe_preemption_releases_capacity_and_cancels_immediately() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let user_addr: SocketAddr = "127.0.0.1:43001".parse().unwrap();

        let probe_handle = manager
            .acquire_connection_for_probe(&input_name, default_probe_user_priority())
            .expect("probe allocation should succeed");
        let probe_token = probe_handle.cancel_token.clone().expect("probe handle must carry cancel token");

        // User request should preempt probe and immediately acquire released capacity.
        let user_alloc = manager
            .acquire_connection_with_grace(
                &input_name,
                &user_addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
            )
            .expect("user allocation should preempt probe");
        assert_eq!(user_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Cancellation happens inline during preemption; yield once to observe any deferred work.
        tokio::task::yield_now().await;
        assert!(probe_token.is_cancelled(), "probe token should be cancelled immediately after preemption");

        manager.release_connection(&user_addr);
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

        manager.refresh_provider_reservation(&input_name, owner_1, 15);
        manager.confirm_playback_activity(owner_1);

        let first = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_1,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner_1),
            )
            .expect("reserved owner should reacquire its provider");
        assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        manager.release_connection(&addr_1);

        let blocked = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &addr_2,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_2),
        );
        assert!(blocked.is_none(), "other sessions must not take a reserved provider before TTL expiry");

        tokio::time::advance(Duration::from_secs(16)).await;

        let second = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_2,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner_2),
            )
            .expect("reservation should expire after TTL");
        assert_eq!(second.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        manager.release_connection(&addr_2);
    }

    #[tokio::test(start_paused = true)]
    async fn denied_confirmation_followed_by_refresh_cannot_reserve_foreign_slot() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner_x = "session-owner-x";
        let owner_y = "session-owner-y";
        let owner_z = "session-owner-z";
        let addr_y: SocketAddr = "127.0.0.1:43310".parse().unwrap();
        let addr_z: SocketAddr = "127.0.0.1:43311".parse().unwrap();

        // Y holds the only provider slot.
        let y = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_y,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner_y),
            )
            .expect("y acquires the single slot");

        // X starts a lease and confirms media while Y holds the slot, so its
        // reservation right is denied. A later refresh must not restore it.
        manager.refresh_provider_reservation(&input_name, owner_x, 15);
        manager.confirm_playback_activity(owner_x);
        manager.refresh_provider_reservation(&input_name, owner_x, 15);

        manager.release_handle(&y);

        // If X's refresh had re-granted the denied reservation, Z would be blocked here.
        let z = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &addr_z,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_z),
        );
        assert!(z.is_some(), "a denied reservation must not be restored by a later refresh");
        if let Some(z) = z {
            manager.release_handle(&z);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn untagged_clear_cannot_delete_successor_reservation() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "session-owner";
        let addr: SocketAddr = "127.0.0.1:43320".parse().unwrap();

        // First incarnation acquires, then is removed entirely.
        let first = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner),
            )
            .expect("first acquire");
        manager.release_handle(&first);
        manager.clear_provider_reservation(owner);

        // Second incarnation acquires a fresh lease on the same account.
        let second = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner),
            )
            .expect("second acquire");

        // An untagged clear has no delete right and must not remove the successor.
        manager.clear_identified_provider_reservation(owner, &input_name, None);
        assert!(
            manager.binding_tag_for_owner(owner).is_some(),
            "an untagged clear must not delete the successor reservation"
        );

        manager.release_handle(&second);
        manager.clear_provider_reservation(owner);
    }

    #[tokio::test(start_paused = true)]
    async fn session_reservations_preserve_capacity_and_priority_order() {
        let app_cfg = create_test_app_config_with_capacity_ordered_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();

        for index in 0..3 {
            let owner = format!("session-owner-{index}");
            let addr = SocketAddr::from(([127, 0, 0, 1], 43200 + index));
            let allocation = manager
                .acquire_connection_with_grace_for_session(
                    &input_name,
                    &addr,
                    false,
                    default_user_priority(),
                    ConnectionKind::Normal,
                    Some(&owner),
                )
                .expect("preferred provider should retain free capacity for another session");
            assert_eq!(allocation.allocation.get_provider_name().as_deref(), Some("provider_1"));
            manager.refresh_provider_reservation(&input_name, &owner, 15);
            manager.confirm_playback_activity(&owner);
        }

        let fallback_addr = SocketAddr::from(([127, 0, 0, 1], 43203));
        let fallback = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &fallback_addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("session-owner-3"),
            )
            .expect("lower-priority provider should be used after preferred capacity is exhausted");
        assert_eq!(fallback.allocation.get_provider_name().as_deref(), Some("provider_2"));

        manager.release_connection(&SocketAddr::from(([127, 0, 0, 1], 43200)));
        manager.release_connection(&fallback_addr);

        let foreign_after_release = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &fallback_addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("session-owner-4"),
            )
            .expect("idle reservation should protect one preferred-provider slot");
        assert_eq!(foreign_after_release.allocation.get_provider_name().as_deref(), Some("provider_2"));

        let reserved_owner_addr = SocketAddr::from(([127, 0, 0, 1], 43204));
        let reserved_owner = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &reserved_owner_addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some("session-owner-0"),
            )
            .expect("reservation owner should reclaim its preferred-provider slot");
        assert_eq!(reserved_owner.allocation.get_provider_name().as_deref(), Some("provider_1"));

        for index in 1..5 {
            let addr = SocketAddr::from(([127, 0, 0, 1], 43200 + index));
            manager.release_connection(&addr);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_unlimited_provider_reservation_does_not_block_other_sessions() {
        let app_cfg = create_test_app_config_single_unlimited_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let owner_1 = "session-owner-1";
        let owner_2 = "session-owner-2";
        let addr_1: SocketAddr = "127.0.0.1:43121".parse().unwrap();
        let addr_2: SocketAddr = "127.0.0.1:43122".parse().unwrap();

        manager.refresh_provider_reservation(&input_name, owner_1, 15);

        let first = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_1,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner_1),
            )
            .expect("reserved owner should reacquire its unlimited provider");
        assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        let second = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_2,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(owner_2),
            )
            .expect("other sessions should not be blocked by reservations on unlimited providers");
        assert_eq!(second.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        manager.release_connection(&addr_1);
        manager.release_connection(&addr_2);
    }

    #[tokio::test(start_paused = true)]
    async fn different_session_cannot_take_idle_reservation_from_same_client_family() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let Ok(second_addr) = "127.0.0.1:43112".parse() else {
            return;
        };
        let owner_channel_1 = "client|ua|user|100";
        let owner_channel_2 = "client|ua|user|200";

        manager.refresh_provider_reservation(&input_name, owner_channel_1, 15);
        manager.confirm_playback_activity(owner_channel_1);

        let blocked = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &second_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_channel_2),
        );
        assert!(blocked.is_none(), "a related but distinct playback must not steal an idle reservation");

        manager.clear_provider_reservation(owner_channel_1);
        let acquired = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &second_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_channel_2),
        );
        assert!(acquired.is_some(), "explicitly clearing the old playback should release its provider");
        manager.release_connection(&second_addr);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_same_family_playbacks_keep_independent_provider_reservations() {
        let app_cfg = create_test_app_config_with_dual_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let provider_2 = "provider_2".intern();
        let Ok(first_addr) = "127.0.0.1:43121".parse() else {
            return;
        };
        let Ok(second_addr) = "127.0.0.1:43122".parse() else {
            return;
        };
        let owner_channel_1 = "proxy|player|user|100";
        let owner_channel_2 = "proxy|player|user|200";

        let first = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &first_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_channel_1),
        );
        assert!(first.is_some(), "first playback should acquire the preferred provider");
        let Some(first) = first else {
            return;
        };
        assert_eq!(first.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        manager.refresh_provider_reservation(&input_name, owner_channel_1, 15);
        manager.confirm_playback_activity(owner_channel_1);
        manager.release_connection(&first_addr);

        let second = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &second_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_channel_2),
        );
        assert!(second.is_some(), "parallel playback should acquire the remaining provider");
        let Some(second) = second else {
            return;
        };
        assert_eq!(second.allocation.get_provider_name().as_deref(), Some(provider_2.as_ref()));
        manager.refresh_provider_reservation(&provider_2, owner_channel_2, 15);
        manager.confirm_playback_activity(owner_channel_2);

        let leases = manager.read_leases();
        assert_eq!(leases.provider_for_owner(owner_channel_1).as_deref(), Some(input_name.as_ref()));
        assert_eq!(leases.provider_for_owner(owner_channel_2).as_deref(), Some(provider_2.as_ref()));
        drop(leases);

        manager.clear_provider_reservation(owner_channel_2);
        let leases = manager.read_leases();
        assert_eq!(leases.provider_for_owner(owner_channel_1).as_deref(), Some(input_name.as_ref()));
        assert!(leases.lease_of_owner(owner_channel_2).is_none());
        drop(leases);

        manager.release_connection(&second_addr);
    }

    #[tokio::test(start_paused = true)]
    async fn test_clear_provider_reservation_releases_family_block() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let owner_1 = "session-owner-1";
        let owner_2 = "session-owner-2";
        let addr_2: SocketAddr = "127.0.0.1:43132".parse().unwrap();

        manager.refresh_provider_reservation(&input_name, owner_1, 15);
        manager.confirm_playback_activity(owner_1);

        let blocked = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &addr_2,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_2),
        );
        assert!(blocked.is_none(), "reservation should initially block another session");

        manager.clear_provider_reservation(owner_1);

        let acquired = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &addr_2,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(owner_2),
        );
        assert!(acquired.is_some(), "clearing reservation should unblock the provider");
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
            .acquire_connection(&input_name, &low_prio_addr, 5, ConnectionKind::Normal)
            .expect("low-priority user should get connection");
        assert_eq!(low_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name));

        // High-priority user arrives (priority -1 = higher importance), should preempt low-priority user
        let high_alloc = manager
            .acquire_connection_with_grace(&input_name, &high_prio_addr, false, -1, ConnectionKind::Normal)
            .expect("high-priority user should preempt low-priority user and get connection");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        manager.release_connection(&high_prio_addr);
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
            .acquire_connection(&input_name, &user_1_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("user1 should get connection");
        assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name));

        // User 2 arrives with the same priority 0 — should NOT preempt user 1
        let alloc2 = manager.acquire_connection_with_grace(
            &input_name,
            &user_2_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
        );
        assert!(alloc2.is_none(), "same-priority user should not preempt existing user");

        manager.release_connection(&user_1_addr);
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
            .acquire_connection(&input_name, &high_prio_addr, -10, ConnectionKind::Normal)
            .expect("high-priority user should get connection");
        assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name));

        // Low-priority user arrives (priority 10) — should NOT preempt high-priority user
        let alloc2 =
            manager.acquire_connection_with_grace(&input_name, &low_prio_addr, false, 10, ConnectionKind::Normal);
        assert!(alloc2.is_none(), "low-priority user should not preempt high-priority user");

        manager.release_connection(&high_prio_addr);
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
            .acquire_connection(&input_name, &low_prio_addr, 20, ConnectionKind::Normal)
            .expect("low-priority user should get connection");
        assert_eq!(low_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        let low_token = low_alloc.cancel_token.clone().expect("must have cancel token");

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name));

        // High-priority user arrives WITH grace allowed (default streaming path)
        // This should get a GracePeriod allocation and then evict the low-prio user
        let high_alloc = manager
            .acquire_connection(&input_name, &high_prio_addr, 0, ConnectionKind::Normal)
            .expect("high-priority user should get grace allocation and evict low-prio");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // Low-priority user's cancel token should be cancelled
        assert!(low_token.is_cancelled(), "low-prio user should be cancelled after eviction");

        // Provider should not be over limit (eviction freed a slot)
        assert!(!manager.is_over_limit(&input_name), "provider should not be over limit after eviction");

        manager.release_connection(&high_prio_addr);
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
            .acquire_connection(&input_name, &user_1_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("user1 should get connection");
        assert_eq!(alloc1.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        let token1 = alloc1.cancel_token.clone().expect("must have cancel token");

        // Provider is now exhausted
        assert!(manager.is_exhausted(&input_name));

        // User 2 arrives with the same priority and should be granted grace instead of being rejected.
        let alloc2 = manager
            .acquire_connection(&input_name, &user_2_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("same-priority user should get grace allocation");
        assert!(matches!(alloc2.allocation, ProviderAllocation::GracePeriod(_)));
        assert_eq!(alloc2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));

        // User 1 should NOT be cancelled, and the provider should be temporarily over limit.
        assert!(!token1.is_cancelled(), "same-prio user should not be evicted");
        assert!(manager.is_over_limit(&input_name), "provider should be temporarily over limit during grace");

        manager.release_connection(&user_1_addr);
        manager.release_connection(&user_2_addr);
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
            .acquire_exact_connection_with_grace(
                &"provider_2".intern(),
                &old_low_addr,
                false,
                20,
                ConnectionKind::Normal,
            )
            .expect("old low-priority allocation should succeed");
        let old_token = old_alloc.cancel_token.clone().expect("old allocation should have cancel token");
        assert_eq!(old_alloc.allocation.get_provider_name().as_deref(), Some("provider_2"));

        let new_alloc = manager
            .acquire_exact_connection_with_grace(
                &"provider_1".intern(),
                &new_low_addr,
                false,
                20,
                ConnectionKind::Normal,
            )
            .expect("new low-priority allocation should succeed");
        let new_token = new_alloc.cancel_token.clone().expect("new allocation should have cancel token");
        assert_eq!(new_alloc.allocation.get_provider_name().as_deref(), Some("provider_1"));

        {
            let mut connections = manager.write_connections();
            let old_created_at = connections
                .single
                .get(&old_alloc.allocation_id)
                .map(|info| info.created_at)
                .expect("old allocation should still be registered");

            let (new_created_at, new_priority) = {
                let info = connections
                    .single
                    .get_mut(&new_alloc.allocation_id)
                    .expect("new allocation should still be registered");
                let original_created_at = info.created_at;
                info.created_at = old_created_at;
                (original_created_at, info.priority)
            };

            let provider_name = "provider_1".intern();
            let tree =
                connections.priority_index.get_mut(&provider_name).expect("priority index for provider_1 should exist");
            let owner = tree
                .remove(&(new_priority, std::cmp::Reverse(new_created_at), new_alloc.allocation_id))
                .expect("new allocation should still be indexed");
            tree.insert((new_priority, std::cmp::Reverse(old_created_at), new_alloc.allocation_id), owner);
        }

        assert!(manager.is_exhausted(&input_name));

        // Higher-priority request should now select the first-inserted victim because
        // priority and created_at are exactly tied across provider aliases.
        let high_alloc = manager
            .acquire_connection(&input_name, &high_prio_addr, 0, ConnectionKind::Normal)
            .expect("higher-priority request should preempt the first-inserted low-priority victim on exact tie");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some("provider_2"));

        assert!(old_token.is_cancelled(), "first-inserted low-priority victim should be canceled first on exact tie");
        assert!(!new_token.is_cancelled(), "later allocation should remain active after the stable tie-break");

        manager.release_connection(&high_prio_addr);
        manager.release_connection(&old_low_addr);
        manager.release_connection(&new_low_addr);
    }

    #[tokio::test]
    async fn shared_promotion_and_release_preserve_other_allocations_on_same_socket(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = build_test_app_config(None, 3);
        let events = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &events);
        let addr = "127.0.0.1:48500".parse()?;
        let input = "provider_1".intern();
        let unrelated = manager
            .acquire_connection(&input, &addr, 0, ConnectionKind::Normal)
            .ok_or("unrelated allocation missing")?;
        let origin =
            manager.acquire_connection(&input, &addr, 5, ConnectionKind::Normal).ok_or("origin allocation missing")?;
        let first = SharedSubscriberId::from_stream_uid(1);
        let second = SharedSubscriberId::from_stream_uid(2);
        let key = "https://example.invalid/shared.ts";
        assert!(manager.make_shared_connection(&origin, key, first));
        assert_eq!(manager.get_provider_connections_count(), 2);
        assert!(manager.read_connections().single.contains_key(&unrelated.allocation_id));
        assert!(manager
            .connections
            .read()
            .unwrap()
            .single_by_addr
            .get(&addr)
            .is_some_and(|allocs| allocs.contains(&unrelated.allocation_id)));
        assert!(!unrelated.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
        manager.add_shared_connection(&addr, second, key, 9, ConnectionKind::Soft)?;
        assert!(manager.reclassify_shared_connection(second, ConnectionKind::Normal, 1));
        {
            let connections = manager.read_connections();
            let shared = connections.shared.by_key.get(key).ok_or("shared origin missing")?;
            assert_eq!(shared.connections.get(&first).map(|subscriber| subscriber.priority), Some(5));
            assert_eq!(shared.priority, 1);
        }
        manager.release_shared_connection(second);
        manager.release_shared_connection(second);
        assert_eq!(manager.get_provider_connections_count(), 2);
        assert_eq!(manager.read_connections().shared.by_key.get(key).map(|shared| shared.priority), Some(5));
        manager.release_shared_connection(first);
        manager.release_handle(&origin);
        assert_eq!(manager.get_provider_connections_count(), 1);
        manager.release_handle(&unrelated);
        assert_eq!(manager.get_provider_connections_count(), 0);
        Ok(())
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
            .acquire_connection(&input_name, &addr_a, 0, ConnectionKind::Normal)
            .expect("A should get initial connection");
        let shared_token = alloc_a.cancel_token.clone().expect("shared allocation should have cancel token");
        assert!(manager.make_shared_connection(&alloc_a, stream_key, SharedSubscriberId::from_stream_uid(1)));

        // B joins the same shared stream with lower importance (priority 1).
        let join_result = manager.add_shared_connection(
            &addr_b,
            SharedSubscriberId::from_stream_uid(2),
            stream_key,
            1,
            ConnectionKind::Normal,
        );
        assert!(join_result.is_ok(), "B should join existing shared stream, got: {join_result:?}");

        // A leaves shared stream. Shared allocation should now inherit B's lower priority.
        manager.release_connection(&addr_a);
        {
            let connections = manager.read_connections();
            let shared = connections.shared.by_key.get(stream_key).expect("shared entry should remain for B");
            assert_eq!(shared.priority, 1, "shared priority must downgrade to remaining subscriber priority");
        }

        // A starts another stream with higher importance and should preempt B's shared stream.
        let alloc_a2 = manager
            .acquire_connection(&input_name, &addr_a, 0, ConnectionKind::Normal)
            .expect("A should preempt lower-priority shared stream");
        assert_eq!(alloc_a2.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        assert!(shared_token.is_cancelled(), "shared stream should be cancelled when preempted");
        assert!(!manager.is_over_limit(&input_name), "provider should not remain over limit after preemption");

        manager.release_connection(&addr_a);
        manager.release_connection(&addr_b);
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
            .acquire_connection(&input_name, &addr_a, 5, ConnectionKind::Normal)
            .expect("low-priority shared stream should get initial connection");
        let shared_token = shared_alloc.cancel_token.clone().expect("shared allocation should have cancel token");
        assert!(manager.make_shared_connection(&shared_alloc, stream_key, SharedSubscriberId::from_stream_uid(1)));

        manager
            .add_shared_connection(
                &addr_b,
                SharedSubscriberId::from_stream_uid(2),
                stream_key,
                6,
                ConnectionKind::Normal,
            )
            .expect("second subscriber should join shared stream");

        let high_alloc = manager
            .acquire_connection(&input_name, &addr_high, 0, ConnectionKind::Normal)
            .expect("higher-priority user should preempt lower-priority shared stream");
        assert_eq!(high_alloc.allocation.get_provider_name().as_deref(), Some(input_name.as_ref()));
        assert!(shared_token.is_cancelled(), "shared stream should be cancelled when preempted");

        {
            let connections = manager.read_connections();
            assert!(
                !connections.shared.by_key.contains_key(stream_key),
                "preempted shared stream must be removed even with multiple subscribers"
            );
        }

        manager.release_connection(&addr_high);
        manager.release_connection(&addr_a);
        manager.release_connection(&addr_b);
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
        let alloc_a = manager.acquire_connection(&input_name, &addr_a, 10, ConnectionKind::Normal).expect("alloc_a");

        // Check index has 1 entry
        {
            let connections = manager.read_connections();
            let tree = connections.priority_index.get(&input_name).expect("index for provider_1");
            assert_eq!(tree.len(), 1, "index should have 1 entry after first allocation");
        }

        // High-priority user evicts low-priority via grace path
        let alloc_b = manager
            .acquire_connection(&input_name, &addr_b, -5, ConnectionKind::Normal)
            .expect("alloc_b should evict alloc_a");

        // Check index: should have 1 entry (alloc_a evicted, alloc_b added)
        {
            let connections = manager.read_connections();
            let tree = connections.priority_index.get(&input_name).expect("index for provider_1");
            assert_eq!(tree.len(), 1, "index should have 1 entry after eviction + new allocation");
            // The remaining entry should be alloc_b
            let ((prio, _, _), _) = tree.iter().next().expect("one entry");
            assert_eq!(*prio, -5, "remaining entry should be the high-prio connection");
        }

        // Release alloc_b
        manager.release_handle(&alloc_b);

        // Check index: should be empty
        {
            let connections = manager.read_connections();
            let tree = connections.priority_index.get(&input_name);
            let is_empty = tree.is_none_or(std::collections::BTreeMap::is_empty);
            assert!(is_empty, "index should be empty after releasing all connections");
        }

        // Verify alloc_a handle can be safely released (already evicted - no-op)
        manager.release_handle(&alloc_a);
    }

    #[tokio::test]
    async fn test_preemption_respects_registered_body_owner_completion() {
        let app_cfg = build_test_app_config(None, 1);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let addr_1: SocketAddr = "127.0.0.1:49061".parse().unwrap();
        let addr_2: SocketAddr = "127.0.0.1:49062".parse().unwrap();

        // 1. First request with prio 5 occupies limit=1
        let handle_1 = manager
            .acquire_connection(&input_name, &addr_1, 5, ConnectionKind::Normal)
            .expect("first allocation should succeed");

        // 2. Register body owner on handle_1
        manager.register_body_owner(handle_1.allocation_id);

        // 3. Higher-priority request with prio -1 arrives without grace while handle_1 is active
        let handle_2 = manager.acquire_connection_with_grace(&input_name, &addr_2, false, -1, ConnectionKind::Normal);

        // 4. Replacement must NOT be admitted before completion
        assert!(handle_2.is_none(), "replacement must not be admitted before victim completion");
        assert!(
            handle_1.cancel_token.as_ref().expect("cancel token").is_cancelled(),
            "victim should receive cancel signal"
        );
        assert_eq!(
            handle_1.close_reason.load(std::sync::atomic::Ordering::Acquire),
            tuliprox_core::model::ProviderCloseReason::PriorityPreempted as u8,
            "victim close reason must be PriorityPreempted"
        );

        // 5. Victim completes upstream reading
        handle_1.completion_token.as_ref().expect("completion token").cancel();

        // Allow background complete_release task to run
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // 6. Now replacement request can be admitted
        let handle_2 = manager
            .acquire_connection(&input_name, &addr_2, -1, ConnectionKind::Normal)
            .expect("replacement should succeed after victim has completed release");

        manager.release_handle(&handle_2);
    }

    #[tokio::test]
    async fn test_preemption_respects_opening_connection_completion() {
        let app_cfg = build_test_app_config(None, 1);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let addr_1: SocketAddr = "127.0.0.1:49063".parse().unwrap();
        let addr_2: SocketAddr = "127.0.0.1:49064".parse().unwrap();

        // 1. First request with prio 5 occupies limit=1
        let handle_1 = manager
            .acquire_connection(&input_name, &addr_1, 5, ConnectionKind::Normal)
            .expect("first allocation should succeed");
        manager.mark_opening(handle_1.allocation_id);

        // Verify lifecycle is Opening and body owner is not yet registered
        {
            let connections = manager.read_connections();
            let info = connections.single.get(&handle_1.allocation_id).expect("handle_1 exists");
            assert_eq!(info.lifecycle, tuliprox_core::model::ConnectionLifecycle::Opening);
            assert!(!info.has_body_owner);
        }

        // 2. Higher-priority request with prio -1 arrives without grace while handle_1 is in Opening
        let handle_2 = manager.acquire_connection_with_grace(&input_name, &addr_2, false, -1, ConnectionKind::Normal);

        // 3. Replacement must NOT be admitted before completion of opening
        assert!(handle_2.is_none(), "replacement must not be admitted while victim is still opening");
        assert!(
            handle_1.cancel_token.as_ref().expect("cancel token").is_cancelled(),
            "victim should receive cancel signal during open"
        );
        assert_eq!(
            handle_1.close_reason.load(std::sync::atomic::Ordering::Acquire),
            tuliprox_core::model::ProviderCloseReason::PriorityPreempted as u8,
            "victim close reason must be PriorityPreempted"
        );

        // 4. Victim's open future completes and signals completion
        handle_1.completion_token.as_ref().expect("completion token").cancel();

        // Allow background complete_release task to run
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // 5. Now replacement request can be admitted
        let handle_2 = manager
            .acquire_connection(&input_name, &addr_2, -1, ConnectionKind::Normal)
            .expect("replacement should succeed after victim open future has completed release");

        manager.release_handle(&handle_2);
    }

    #[tokio::test]
    async fn test_owner_index_consistent_through_lifecycle() {
        let app_cfg = build_test_app_config(None, 2);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let addr_a: SocketAddr = "127.0.0.1:49051".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:49052".parse().unwrap();

        let owner_a = "owner-a";
        let owner_b = "owner-b";
        let alloc_a = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_a,
                false,
                0,
                ConnectionKind::Normal,
                Some(owner_a),
            )
            .expect("alloc_a");
        let alloc_b = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr_b,
                false,
                0,
                ConnectionKind::Normal,
                Some(owner_b),
            )
            .expect("alloc_b");

        {
            let connections = manager.read_connections();
            assert_eq!(connections.by_owner.get(owner_a).map_or(0, HashSet::len), 1, "owner-a has one allocation");
            assert_eq!(connections.by_owner.get(owner_b).map_or(0, HashSet::len), 1, "owner-b has one allocation");
            assert!(connections.by_owner.get(owner_a).unwrap().contains(&alloc_a.allocation_id));
            assert!(connections.by_owner.get(owner_b).unwrap().contains(&alloc_b.allocation_id));
        }

        manager.release_handle(&alloc_a);

        {
            let connections = manager.read_connections();
            assert!(!connections.by_owner.contains_key(owner_a), "owner-a index is removed after release");
            assert_eq!(connections.by_owner.get(owner_b).map_or(0, HashSet::len), 1, "owner-b still indexed");
        }

        manager.release_handle(&alloc_b);
        {
            let connections = manager.read_connections();
            assert!(connections.by_owner.is_empty(), "owner index is empty after all releases");
        }
    }

    #[tokio::test]
    async fn test_normal_connection_preempts_existing_soft_connection() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let soft_addr: SocketAddr = "127.0.0.1:49011".parse().unwrap();
        let normal_addr: SocketAddr = "127.0.0.1:49012".parse().unwrap();

        let soft_alloc = manager
            .acquire_connection(&input_name, &soft_addr, default_user_priority(), ConnectionKind::Soft)
            .expect("soft allocation");
        let soft_token = soft_alloc.cancel_token.clone().expect("soft allocations expose a cancel token");

        let normal_alloc = manager
            .acquire_connection(&input_name, &normal_addr, default_user_priority(), ConnectionKind::Normal)
            .expect("normal allocation should preempt soft allocation");

        tokio::task::yield_now().await;
        assert!(soft_token.is_cancelled(), "soft allocation should be preempted by normal traffic");
        assert_eq!(manager.get_provider_connections_count(), 1);

        manager.release_handle(&normal_alloc);
    }

    #[tokio::test]
    async fn test_higher_priority_soft_preempts_lower_priority_soft() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let low_soft_addr: SocketAddr = "127.0.0.1:49013".parse().unwrap();
        let high_soft_addr: SocketAddr = "127.0.0.1:49014".parse().unwrap();

        let low_soft_alloc = manager
            .acquire_connection(&input_name, &low_soft_addr, 10, ConnectionKind::Soft)
            .expect("low-priority soft allocation");
        let low_soft_token = low_soft_alloc.cancel_token.clone().expect("soft allocations expose a cancel token");

        let high_soft_alloc = manager
            .acquire_connection(&input_name, &high_soft_addr, -5, ConnectionKind::Soft)
            .expect("higher-priority soft allocation should preempt lower-priority soft allocation");

        tokio::task::yield_now().await;
        assert!(low_soft_token.is_cancelled(), "lower-priority soft allocation should be preempted");
        assert_eq!(manager.get_provider_connections_count(), 1);

        manager.release_handle(&high_soft_alloc);
    }

    #[tokio::test]
    async fn test_reclassify_soft_to_normal_prevents_same_priority_preemption() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let promoted_addr: SocketAddr = "127.0.0.1:49015".parse().unwrap();
        let challenger_addr: SocketAddr = "127.0.0.1:49016".parse().unwrap();

        let promoted_alloc = manager
            .acquire_connection(&input_name, &promoted_addr, default_user_priority(), ConnectionKind::Soft)
            .expect("initial soft allocation");
        let promoted_token = promoted_alloc.cancel_token.clone().expect("soft allocations expose a cancel token");

        assert!(
            manager.reclassify_connection(&promoted_addr, ConnectionKind::Normal, default_user_priority()),
            "soft allocation should be promotable to normal"
        );

        let challenger = manager.acquire_connection_with_grace(
            &input_name,
            &challenger_addr,
            false,
            default_user_priority(),
            ConnectionKind::Normal,
        );
        assert!(challenger.is_none(), "same-priority normal traffic should not preempt a promoted normal connection");
        assert!(!promoted_token.is_cancelled(), "promoted connection should remain active");
        assert_eq!(manager.get_provider_connections_count(), 1);

        manager.release_handle(&promoted_alloc);
    }

    #[tokio::test]
    async fn test_reclassify_connection_for_owner_preserves_other_owner_on_same_socket() {
        let app_cfg = build_test_app_config(None, 2);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);

        let input_name = "provider_1".intern();
        let shared_addr: SocketAddr = "127.0.0.1:49020".parse().unwrap();

        let alloc_1 = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &shared_addr,
                false,
                -10,
                ConnectionKind::Soft,
                Some(PlaybackLeaseRef::new("owner-1", PlaybackKind::LiveTs)),
            )
            .expect("first soft allocation");

        let alloc_2 = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &shared_addr,
                false,
                -10,
                ConnectionKind::Soft,
                Some(PlaybackLeaseRef::new("owner-2", PlaybackKind::LiveTs)),
            )
            .expect("second soft allocation");

        assert!(manager.reclassify_connection_for_owner(&shared_addr, Some("owner-1"), ConnectionKind::Normal, 0));

        {
            let connections = manager.read_connections();
            let info_1 = connections.single.get(&alloc_1.allocation_id).expect("alloc_1 exists");
            assert_eq!(info_1.kind, ConnectionKind::Normal);
            assert_eq!(info_1.priority, 0);

            let info_2 = connections.single.get(&alloc_2.allocation_id).expect("alloc_2 exists");
            assert_eq!(info_2.kind, ConnectionKind::Soft);
            assert_eq!(info_2.priority, -10);
        }

        manager.release_handle(&alloc_1);
        manager.release_handle(&alloc_2);
    }

    /// A=2 / B=3 pool: five confirmed playbacks exhaust the pool and a sixth start is
    /// rejected; releasing one confirmed playback frees exactly one slot again.
    #[tokio::test]
    async fn sixth_playback_rejected_after_pool_exhausted_by_confirmed_leases() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();

        let mut acquired = Vec::new();
        for index in 0..5u16 {
            let owner = format!("playback-{index}");
            let addr = SocketAddr::from(([172, 18, 0, 9], 60_000 + index));
            let handle = manager
                .acquire_connection_with_grace_for_session(
                    &input_name,
                    &addr,
                    false,
                    default_user_priority(),
                    ConnectionKind::Normal,
                    Some(&owner),
                )
                .expect("pool has capacity for five playbacks");
            manager.confirm_playback_activity(&owner);
            acquired.push((handle, owner));
        }

        let sixth = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 60_005)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("playback-5"),
        );
        assert!(sixth.is_none(), "sixth start must be rejected once the pool is exhausted");

        manager.release_handle(&acquired[0].0);
        let replacement = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 60_006)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some("playback-6"),
        );
        assert!(replacement.is_some(), "freed capacity must be selectable again");
        manager.release_handle(&replacement.expect("replacement handle"));
        for (handle, _owner) in &acquired[1..] {
            manager.release_handle(handle);
        }
        assert_eq!(manager.get_provider_connections_count(), 0);
    }

    /// Confirmed reconnect-capable lease, physical handle release first, then the real
    /// provider-error outcome: the error must reach the lease and leave no idle reserve.
    #[tokio::test(start_paused = true)]
    async fn provider_error_cleanup_after_physical_release_does_not_keep_idle_lease() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "r5-error-owner";
        let addr = SocketAddr::from(([172, 18, 0, 9], 55_000));
        let handle = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
            )
            .expect("acquire a reconnect-capable slot");
        let request_id = handle.playback_request_id.expect("identified request id");
        manager.refresh_adaptive_playback_lease(&input_name, owner, PlaybackKind::LiveHls, 15);
        manager.confirm_playback_activity(owner);

        // Physical release must not conclude the request; the outcome is decided later.
        manager.release_handle(&handle);
        assert_eq!(
            manager.provider_lease_usage(&input_name).active,
            1,
            "physical release must not finish a confirmed lease"
        );

        manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::ProviderFailed);
        assert_eq!(
            manager.provider_lease_usage(&input_name).total(),
            0,
            "provider error must not keep an idle reconnect lease"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_refresh_does_not_recreate_terminal_request() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "terminal-refresh-owner";
        let handle = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &SocketAddr::from(([172, 18, 0, 9], 55_002)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
            )
            .expect("acquire identified request");
        let request_id = handle.playback_request_id.expect("identified request id");
        let lease_ref = PlaybackLeaseRef { owner, kind: PlaybackKind::LiveHls, request_id };

        manager.release_handle(&handle);
        manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::ProviderFailed);
        assert_eq!(manager.provider_lease_usage(&input_name).total(), 0);

        manager.refresh_playback_lease(&input_name, &lease_ref, 15);
        manager.confirm_identified_playback_activity(owner, request_id);
        assert_eq!(manager.provider_lease_usage(&input_name).total(), 0);
    }

    /// Counterexample: the same sequence with a clean end keeps the configured idle window.
    #[tokio::test(start_paused = true)]
    async fn clean_cleanup_after_physical_release_keeps_configured_idle_window() {
        let app_cfg = create_test_app_config_with_pool(2, 3);
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();
        let owner = "r5-clean-owner";
        let addr = SocketAddr::from(([172, 18, 0, 9], 55_001));

        let handle = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &addr,
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
            )
            .expect("acquire a reconnect-capable slot");
        let request_id = handle.playback_request_id.expect("identified request id");
        manager.refresh_adaptive_playback_lease(&input_name, owner, PlaybackKind::LiveHls, 15);
        manager.confirm_playback_activity(owner);

        manager.release_handle(&handle);
        manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::Completed);
        let usage = manager.provider_lease_usage(&input_name);
        assert_eq!(usage.active, 0, "clean end must move the lease out of active");
        assert_eq!(usage.idle, 1, "clean end must keep the configured reconnect window");
    }

    /// max=1: X fetch → physical release → Y acquire → X first byte within the startup
    /// deadline. X's late confirmation records media but must not over-commit Y's slot.
    #[tokio::test(start_paused = true)]
    async fn late_first_byte_cannot_reserve_slot_taken_by_other_playback() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();

        let x_handle = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &SocketAddr::from(([172, 18, 0, 9], 40_000)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new("x", PlaybackKind::LiveHls)),
            )
            .expect("x acquires the only slot");
        manager.refresh_adaptive_playback_lease(&input_name, "x", PlaybackKind::LiveHls, 15);
        manager.release_handle(&x_handle);

        let y_handle = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &SocketAddr::from(([172, 18, 0, 9], 40_001)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new("y", PlaybackKind::LiveHls)),
            )
            .expect("y acquires the now-free slot");

        manager.confirm_playback_activity("x");
        assert_eq!(manager.get_provider_connections_count(), 1, "y still holds the only slot");
        assert!(
            !manager.is_provider_reserved_for_other_session(&input_name, Some("y")),
            "x's late confirmation must not over-commit the provider"
        );

        manager.confirm_playback_activity("y");
        manager.release_handle(&y_handle);
        manager.finish_playback_request("y", PlaybackRequestOutcome::Completed, None);
    }

    /// Counterexample: the same X fetch → release → first-byte sequence without a
    /// competing Y keeps the free slot as a real reservation.
    #[tokio::test(start_paused = true)]
    async fn first_byte_reserves_when_slot_still_free() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
        let input_name = "provider_1".intern();

        let x_handle = manager
            .acquire_connection_with_lease_for_session(
                &input_name,
                &SocketAddr::from(([172, 18, 0, 9], 40_002)),
                false,
                default_user_priority(),
                ConnectionKind::Normal,
                Some(PlaybackLeaseRef::new("x", PlaybackKind::LiveHls)),
            )
            .expect("x acquires the only slot");
        manager.refresh_adaptive_playback_lease(&input_name, "x", PlaybackKind::LiveHls, 15);
        manager.release_handle(&x_handle);

        // No competing playback took the slot, so X's first byte reserves it.
        manager.confirm_playback_activity("x");
        assert!(
            manager.is_provider_reserved_for_other_session(&input_name, Some("other")),
            "a free slot must be reservable by the confirming playback"
        );

        manager.finish_playback_request("x", PlaybackRequestOutcome::Completed, None);
    }

    /// The RAII owner releases the allocation synchronously even when dropped outside
    /// a tokio runtime, so a body/context drop can never leak a provider slot.
    #[test]
    fn managed_handle_drop_outside_runtime_releases_allocation() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (manager, managed_handle) = rt.block_on(async {
            let app_cfg = create_test_app_config_single_provider_pool();
            let event_manager = Arc::new(EventManager::new());
            let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
            let input_name = "provider_1".intern();

            let handle = manager
                .acquire_connection_with_grace_for_session(
                    &input_name,
                    &SocketAddr::from(([127, 0, 0, 1], 45_000)),
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some("managed-owner"),
                )
                .expect("allocation should succeed");
            assert_eq!(manager.get_provider_connections_count(), 1);

            let managed_handle = super::ManagedProviderHandle::new(Arc::clone(&manager), handle);
            (manager, managed_handle)
        });

        drop(managed_handle);
        assert_eq!(manager.get_provider_connections_count(), 0);
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    fn latency_percentile(sorted: &[u64], p: f64) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[index.min(sorted.len() - 1)]
    }

    #[cfg(target_os = "linux")]
    fn resident_set_kib() -> Option<u64> {
        let Ok(content) = std::fs::read_to_string("/proc/self/status") else {
            return None;
        };
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    fn resident_set_kib() -> Option<u64> { None }

    struct LeaseWorkloadStats {
        ops: u64,
        elapsed_secs: f64,
        acquire_p50: u64,
        acquire_p95: u64,
        acquire_p99: u64,
        confirm_p50: u64,
        confirm_p95: u64,
        confirm_p99: u64,
        release_p50: u64,
        release_p95: u64,
        release_p99: u64,
    }

    #[allow(clippy::cast_possible_truncation)]
    async fn measure_lease_workload(
        manager: &Arc<ActiveProviderManager>,
        input_name: &Arc<str>,
        concurrency: usize,
        rounds_per_task: usize,
    ) -> LeaseWorkloadStats {
        let samples = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64, u64)>::new()));
        let started = std::time::Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for task_index in 0..concurrency {
            let manager = Arc::clone(manager);
            let input_name = Arc::clone(input_name);
            let samples = Arc::clone(&samples);
            tasks.spawn(async move {
                for round in 0..rounds_per_task {
                    let owner = format!("bench-{task_index}-{round}");
                    let addr = SocketAddr::from(([127, 0, 0, 1], 40_000 + (task_index as u16)));
                    let acquire_at = std::time::Instant::now();
                    let Some(handle) = manager.acquire_connection_with_grace_for_session(
                        &input_name,
                        &addr,
                        false,
                        0,
                        ConnectionKind::Normal,
                        Some(&owner),
                    ) else {
                        continue;
                    };
                    let acquire_us = acquire_at.elapsed().as_micros() as u64;

                    let confirm_at = std::time::Instant::now();
                    manager.confirm_playback_activity(&owner);
                    let confirm_us = confirm_at.elapsed().as_micros() as u64;

                    let release_at = std::time::Instant::now();
                    manager.release_handle(&handle);
                    let release_us = release_at.elapsed().as_micros() as u64;

                    // A physical release never concludes the request; model the real
                    // lifecycle by finishing with a clean outcome so the lease returns
                    // to baseline (LiveTs is not reconnect-capable, so it is removed).
                    manager.finish_playback_request(&owner, PlaybackRequestOutcome::Completed, None);

                    samples.lock().unwrap().push((acquire_us, confirm_us, release_us));
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.expect("benchmark task must not fail");
        }

        let elapsed_secs = started.elapsed().as_secs_f64();
        let samples = Arc::try_unwrap(samples).expect("samples unique").into_inner().unwrap();
        let mut acquire = Vec::with_capacity(samples.len());
        let mut confirm = Vec::with_capacity(samples.len());
        let mut release = Vec::with_capacity(samples.len());
        for (a, c, r) in samples {
            acquire.push(a);
            confirm.push(c);
            release.push(r);
        }
        acquire.sort_unstable();
        confirm.sort_unstable();
        release.sort_unstable();
        let ops = acquire.len() as u64;
        LeaseWorkloadStats {
            ops,
            elapsed_secs,
            acquire_p50: latency_percentile(&acquire, 0.50),
            acquire_p95: latency_percentile(&acquire, 0.95),
            acquire_p99: latency_percentile(&acquire, 0.99),
            confirm_p50: latency_percentile(&confirm, 0.50),
            confirm_p95: latency_percentile(&confirm, 0.95),
            confirm_p99: latency_percentile(&confirm, 0.99),
            release_p50: latency_percentile(&release, 0.50),
            release_p95: latency_percentile(&release, 0.95),
            release_p99: latency_percentile(&release, 0.99),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "manager microbenchmark: cargo +stable test -p tuliprox-session --release -- --ignored --nocapture bench_provider_lease_microbenchmark"]
    #[allow(clippy::cast_precision_loss)]
    async fn bench_provider_lease_microbenchmark() {
        let manager = Arc::new(ActiveProviderManager::new(
            &create_test_app_config_single_unlimited_provider_pool(),
            &Arc::new(EventManager::new()),
        ));
        let input_name: Arc<str> = "provider_1".intern();

        // Warm-up before measurement so allocator and lock caches are exercised.
        for index in 0..64u16 {
            let addr = SocketAddr::from(([127, 0, 0, 1], 40_000 + index));
            let Some(handle) = manager.acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                None,
            ) else {
                continue;
            };
            manager.release_handle(&handle);
        }

        let baseline_rss_kib = resident_set_kib();
        eprintln!("provider lease manager microbenchmark (unlimited provider, in-process, no HTTP bodies)");
        for concurrency in [1usize, 5, 50, 200] {
            let stats = measure_lease_workload(&manager, &input_name, concurrency, 10).await;
            assert_eq!(
                stats.ops,
                (concurrency * 10) as u64,
                "unexpected acquire failures at concurrency {concurrency}"
            );
            let throughput = stats.ops as f64 / stats.elapsed_secs.max(f64::EPSILON);
            let rss_delta_kib = resident_set_kib()
                .zip(baseline_rss_kib)
                .map_or_else(|| "unsupported".to_string(), |(rss, baseline)| rss.saturating_sub(baseline).to_string());
            eprintln!(
                "concurrency={concurrency:>3} ops={:>5} throughput={:>9.1} ops/s | acquire p50/p95/p99={}/{}/{}us | confirm p50/p95/p99={}/{}/{}us | release p50/p95/p99={}/{}/{}us | rss_delta_kib={}",
                stats.ops,
                throughput,
                stats.acquire_p50,
                stats.acquire_p95,
                stats.acquire_p99,
                stats.confirm_p50,
                stats.confirm_p95,
                stats.confirm_p99,
                stats.release_p50,
                stats.release_p95,
                stats.release_p99,
                rss_delta_kib,
            );
        }

        // Soak-style churn: every connection and lease must return to baseline.
        assert_eq!(manager.get_provider_connections_count(), 0);
        let usage = manager.provider_lease_usage(&input_name);
        assert_eq!(usage.total(), 0, "lease table must return to baseline after churn");
    }

    #[tokio::test]
    async fn closing_state_blocks_slot_release_until_completion_token_cancelled() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name: Arc<str> = "provider_1".intern();

        let handle1 = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &SocketAddr::from(([127, 0, 0, 1], 46_001)),
                false,
                0,
                ConnectionKind::Normal,
                Some("session-1"),
            )
            .expect("first allocation should succeed");
        assert_eq!(manager.get_provider_connections_count(), 1);

        let completion_token = handle1.completion_token.clone().expect("completion token present");
        let alloc_id = handle1.allocation_id;

        // Mark Closing (simulating supersede / release_playback_connections_await)
        manager.mark_closing(alloc_id);

        // Drop the handle while completion_token is still pending (upstream body not yet closed)
        manager.release_handle(&handle1);

        // The slot must still be occupied by the Closing allocation
        assert_eq!(manager.get_provider_connections_count(), 1);

        // A second start attempt must fail because the provider is at its limit (1 connection)
        let handle2 = manager.acquire_connection_with_grace_for_session(
            &input_name,
            &SocketAddr::from(([127, 0, 0, 1], 46_002)),
            false,
            0,
            ConnectionKind::Normal,
            Some("session-2"),
        );
        assert!(handle2.is_none(), "second start must be rejected while first slot is Closing");

        // Now signal completion (upstream body owner dropped upstream socket)
        completion_token.cancel();

        // Allow background reaper task to execute complete_release
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(manager.get_provider_connections_count(), 0);

        // Second start attempt now succeeds
        let handle3 = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &SocketAddr::from(([127, 0, 0, 1], 46_003)),
                false,
                0,
                ConnectionKind::Normal,
                Some("session-3"),
            )
            .expect("start must succeed after completion releases the slot");
        assert_eq!(manager.get_provider_connections_count(), 1);

        manager.release_handle(&handle3);
        assert_eq!(manager.get_provider_connections_count(), 0);
    }

    #[tokio::test]
    async fn reaper_cleanup_is_idempotent_and_removes_all_indices() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name: Arc<str> = "provider_1".intern();

        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &SocketAddr::from(([127, 0, 0, 1], 47_001)),
                false,
                0,
                ConnectionKind::Normal,
                Some("session-idempotent"),
            )
            .expect("allocation should succeed");

        let completion_token = handle.completion_token.clone().expect("completion token present");
        let alloc_id = handle.allocation_id;
        manager.mark_closing(alloc_id);

        manager.release_handle(&handle);
        completion_token.cancel();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(manager.get_provider_connections_count(), 0);

        // Duplicate releases must be completely safe and no-op
        manager.complete_release(alloc_id);
        manager.release_handle(&handle);
        manager.release_connection(&SocketAddr::from(([127, 0, 0, 1], 47_001)));
        assert_eq!(manager.get_provider_connections_count(), 0);
    }

    #[tokio::test]
    async fn test_single_request_preemption_awaits_victim_completion() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name: Arc<str> = "provider_1".intern();

        let low_addr = SocketAddr::from(([127, 0, 0, 1], 48_001));
        let high_addr = SocketAddr::from(([127, 0, 0, 1], 48_002));

        let low_handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &low_addr,
                false,
                10,
                ConnectionKind::Normal,
                Some("session-victim"),
            )
            .expect("initial low-priority allocation succeeds");

        manager.mark_opening(low_handle.allocation_id);
        assert!(manager.register_body_owner(low_handle.allocation_id));

        let cancel_token = low_handle.cancel_token.clone().unwrap();
        let completion_token = low_handle.completion_token.clone().unwrap();

        // Simulate upstream body owner: when cancelled, complete release after 30ms
        let comp_clone = completion_token.clone();
        tokio::spawn(async move {
            cancel_token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(30)).await;
            comp_clone.cancel();
        });

        // High priority acquire should preempt the victim, await its completion, and succeed in the same call
        let high_handle = manager
            .acquire_connection_with_lease_for_session_await(
                &input_name,
                &high_addr,
                false,
                0,
                ConnectionKind::Normal,
                None,
            )
            .await
            .expect("high-priority allocation must succeed in the same call after awaiting victim completion");

        assert_eq!(manager.get_provider_connections_count(), 1);
        manager.release_handle(&high_handle);
        assert_eq!(manager.get_provider_connections_count(), 0);
    }

    #[tokio::test]
    async fn stale_handle_does_not_cancel_newer_generation_slot() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name: Arc<str> = "provider_1".intern();
        let addr = SocketAddr::from(([127, 0, 0, 1], 49_001));

        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                Some("session-gen"),
            )
            .expect("initial allocation succeeds");

        manager.mark_opening(handle.allocation_id);
        assert!(manager.register_body_owner(handle.allocation_id));

        // Gen 0 has an old completion token
        let old_completion_token = handle.completion_token.clone().expect("completion token present");

        // Renew opening tokens to advance generation to 1
        let (_new_cancel, new_completion, gen) =
            manager.renew_opening_tokens(handle.allocation_id).expect("renewal should succeed");
        assert_eq!(gen, 1);

        // Simulate an old handle belonging to generation 0 whose completion token was cancelled
        old_completion_token.cancel();
        let stale_handle = tuliprox_core::model::ProviderHandle {
            playback_request_id: handle.playback_request_id,
            binding_tag: handle.binding_tag,
            client_id: handle.client_id,
            allocation_id: handle.allocation_id,
            allocation: handle.allocation.clone(),
            cancel_token: handle.cancel_token.clone(),
            completion_token: Some(old_completion_token),
            close_reason: Arc::clone(&handle.close_reason),
            open_generation: 0, // Stale generation
        };

        // Releasing the stale handle must NOT poison/cancel the new generation's completion token
        manager.release_handle(&stale_handle);
        assert!(
            !new_completion.is_cancelled(),
            "new generation completion token must remain uncancelled after stale handle release"
        );
        assert_eq!(manager.get_provider_connections_count(), 1, "slot must still be held by the active new generation");

        // When the real new generation completes, the slot is reaped
        new_completion.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(manager.get_provider_connections_count(), 0);
    }

    #[tokio::test]
    async fn dropped_cleanup_future_does_not_permanently_leak_slot() {
        let app_cfg = create_test_app_config_single_provider_pool();
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let input_name: Arc<str> = "provider_1".intern();
        let addr = SocketAddr::from(([127, 0, 0, 1], 49_002));
        let addrs = [addr];

        let handle = manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                Some("session-drop"),
            )
            .expect("allocation succeeds");

        manager.mark_opening(handle.allocation_id);
        assert!(manager.register_body_owner(handle.allocation_id));
        let completion_token = handle.completion_token.clone().expect("token present");

        // Start release_playback_connections_await but drop it before completion
        {
            let cleanup_fut = manager.release_playback_connections_await("session-drop", &addrs);
            // Poll once and drop
            tokio::select! {
                biased;
                () = async {} => {},
                () = cleanup_fut => {},
            }
        }

        // Slot must still be occupied
        assert_eq!(manager.get_provider_connections_count(), 1);

        // Now the handle is released and the body owner signals completion
        manager.release_handle(&handle);
        completion_token.cancel();

        // Give the background reaper task time to run
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            manager.get_provider_connections_count(),
            0,
            "slot must be freed after completion and not permanently leaked in Closing state"
        );
    }
}
