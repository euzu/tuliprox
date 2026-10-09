use super::{
    ActiveProviderManager, ActiveProviderManagerCore, CapacityReleases, ConnectionKind, ManagedProviderHandle,
    PlaybackLeaseRef, PreemptionOutcome, PriorityKey, PriorityOwner, ProviderAllocationGuard, ProviderCapacityNotifier,
    ProviderReleaseSnapshot, SharedConnections, DUMMY_ADDR, PREEMPTION_COMPLETION_TIMEOUT,
};
use crate::{
    provider_leases::{ProviderLeaseTable, ProviderLeaseUsage},
    provider_lineup_manager::ProviderLineupManager,
    EventManager,
};
use ::shared::utils::sanitize_sensitive_info;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{
        AllocationId, AppConfig, ConfigInput, ConnectionLifecycle, PlaybackKind, PlaybackRequestId,
        PlaybackRequestOutcome, PlaybackSelectionReason, ProviderAllocation, ProviderBindingTag, ProviderCloseReason,
        ProviderConfig, ProviderHandle,
    },
    utils::debug_if_enabled,
};

impl ProviderReleaseSnapshot {
    pub fn is_empty(&self) -> bool { self.single_allocations.is_empty() && self.shared_subscribers.is_empty() }
}

/// Resolves a public live HLS session token to its stable provider playback owner.
///
/// The random suffix distinguishes public sessions, while the preceding client,
/// user and channel fingerprint identifies one provider playback across retries.
/// Catchup and shared-cache owners retain their own identity semantics.
pub(super) fn playback_lease_owner(owner: &str) -> &str {
    if owner.starts_with("m3u-catchup|") || owner.starts_with("catchup|") || owner.starts_with("hls-cache:") {
        return owner;
    }
    let Some((base, suffix)) = owner.rsplit_once("|hls|") else {
        return owner;
    };
    if suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        base
    } else {
        owner
    }
}

impl<'a> PlaybackLeaseRef<'a> {
    pub fn new(owner: &'a str, kind: PlaybackKind) -> Self {
        Self { owner, kind, request_id: PlaybackRequestId::next() }
    }

    /// Stable provider lease identity, independent of the public HLS token suffix.
    pub fn provider_owner(&self) -> &'a str { playback_lease_owner(self.owner) }
}

pub(super) struct AcquireProviderParams<'a> {
    pub(super) addr: &'a SocketAddr,
    pub(super) priority: i8,
    pub(super) kind: ConnectionKind,
    pub(super) lease: Option<PlaybackLeaseRef<'a>>,
}

impl AcquireProviderParams<'_> {
    #[inline]
    pub(super) fn session_owner(&self) -> Option<&str> { self.lease.map(|lease| lease.provider_owner()) }
}

#[derive(Debug, Clone)]
pub(super) struct ActiveConnectionInfo {
    pub(super) allocation_id: AllocationId,
    pub(super) client_addr: SocketAddr,
    pub(super) allocation: ProviderAllocation,
    // Used to signal preemption to the consumer of this connection
    pub(super) cancel_token: CancellationToken,
    pub(super) completion_token: CancellationToken,
    pub(super) close_reason: Arc<AtomicU8>,
    pub(super) lifecycle: ConnectionLifecycle,
    pub(super) has_body_owner: bool,
    pub(super) reaper_spawned: bool,
    pub(super) open_generation: u64,
    pub(super) created_at: Instant,
    pub(super) priority: i8,
    pub(super) kind: ConnectionKind,
    pub(super) session_owner: Option<Arc<str>>,
    // Public session identity scopes targeted socket cleanup independently of the
    // stable provider lease owner shared by entry retries.
    pub(super) request_owner: Option<Arc<str>>,
    pub(super) playback_request_id: Option<PlaybackRequestId>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Connections {
    // Primary map keyed by AllocationId, never by socket address.
    pub(super) single: HashMap<AllocationId, ActiveConnectionInfo>,
    // Secondary index for socket-wide transport close/kick actions.
    pub(super) single_by_addr: HashMap<SocketAddr, HashSet<AllocationId>>,
    pub(super) shared: SharedConnections,
    // Index to quickly find connections by provider name for preemption
    // ProviderName -> Set<AllocationId>
    pub(super) by_provider: HashMap<Arc<str>, HashSet<AllocationId>>,
    // Index to find every allocation (single and shared) owned by a playback session.
    // SessionOwner -> Set<AllocationId>
    pub(super) by_owner: HashMap<Arc<str>, HashSet<AllocationId>>,
    // Priority index per provider alias for O(log n) victim lookup
    // ProviderName -> BTreeMap<PriorityKey, PriorityOwner>
    pub(super) priority_index: HashMap<Arc<str>, BTreeMap<PriorityKey, PriorityOwner>>,
    pub(super) soft_priority_index: HashMap<Arc<str>, BTreeMap<PriorityKey, PriorityOwner>>,
    // Providers whose allocations were released since the last drain.
    pub(super) released_providers: Vec<Arc<str>>,
}

impl CapacityReleases for Connections {
    fn take_released_providers(&mut self) -> Vec<Arc<str>> { std::mem::take(&mut self.released_providers) }
}

impl ProviderCapacityNotifier {
    fn lock_waiters(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, Arc<tokio::sync::Notify>>> {
        self.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The notification fired whenever `provider` frees capacity. Provider names are bounded by
    /// the configuration, so entries are kept.
    pub fn subscribe(&self, provider: &Arc<str>) -> Arc<tokio::sync::Notify> {
        Arc::clone(self.lock_waiters().entry(Arc::clone(provider)).or_default())
    }

    pub(super) fn notify(&self, provider: &str) {
        if let Some(notify) = self.lock_waiters().get(provider) {
            notify.notify_waiters();
        }
    }

    /// Called while a table lock is held; [`Self::flush`] delivers after it is released.
    pub(super) fn stage(&self, released: Vec<Arc<str>>) {
        if released.is_empty() {
            return;
        }
        self.staged.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(released);
        self.has_staged.store(true, Ordering::Release);
    }

    pub(super) fn flush(&self) {
        if !self.has_staged.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut staged = std::mem::take(&mut *self.staged.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        staged.sort_unstable();
        staged.dedup();
        let waiters = self.lock_waiters();
        for provider in &staged {
            if let Some(notify) = waiters.get(provider) {
                notify.notify_waiters();
            }
        }
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
}

impl std::ops::Deref for ActiveProviderManager {
    type Target = ActiveProviderManagerCore;

    #[inline]
    fn deref(&self) -> &Self::Target { &self.core }
}

impl ActiveProviderManager {
    fn index_owner(connections: &mut Connections, allocation_id: AllocationId, owner: Option<&Arc<str>>) {
        if let Some(owner) = owner {
            connections.by_owner.entry(Arc::clone(owner)).or_default().insert(allocation_id);
        }
    }

    pub(super) fn unindex_owner(connections: &mut Connections, allocation_id: AllocationId, owner: Option<&Arc<str>>) {
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
                leases: std::sync::RwLock::new(ProviderLeaseTable::with_affinity_ttl(Self::get_affinity_ttl(cfg))),
                next_allocation_id: AtomicU64::new(1),
                capacity: ProviderCapacityNotifier::default(),
            }),
            shared_stream_manager: OnceLock::new(),
        }
    }

    /// Routes account observations reported through `event_manager` to this manager.
    /// Only the first binding wins, so test or auxiliary managers cannot hijack the runtime one.
    pub fn bind_event_manager(&self, event_manager: &EventManager) {
        let _ = event_manager.account_provider.set(Arc::downgrade(&self.core));
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

    pub fn update_config(&self, cfg: &AppConfig) {
        let grace_period_options = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        self.providers.update_config(inputs, &grace_period_options);
        self.write_leases().set_affinity_ttl(Self::get_affinity_ttl(cfg));
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

    pub(super) fn prune_expired_leases(leases: &mut ProviderLeaseTable) { leases.prune(TokioInstant::now()); }

    /// A provisional lease only pins its provider while its allocation is active.
    /// Once an unstarted request releases its slot, the next attempt may use another alias.
    pub fn should_reuse_playback_provider(&self, session_owner: &str, provider_name: &Arc<str>) -> bool {
        let session_owner = playback_lease_owner(session_owner);
        let _transition = self.lock_capacity_transition();
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        let confirmed = leases
            .lease_of_owner(session_owner)
            .is_some_and(|lease| lease.provider_name == *provider_name && lease.state.is_confirmed());
        drop(leases);
        confirmed || self.has_active_owner_for_provider(provider_name, session_owner)
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

    pub(super) fn reserved_provider_names_for_other(
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
        let session_owner = playback_lease_owner(session_owner);
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
        let lease_ref = PlaybackLeaseRef { owner: lease_ref.provider_owner(), ..*lease_ref };
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

    pub fn finish_identified_playback_request(
        &self,
        owner: &str,
        request_id: PlaybackRequestId,
        outcome: PlaybackRequestOutcome,
    ) {
        self.finish_playback_request_inner(owner, Some(request_id), outcome, None);
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

    pub(super) fn acquire_exact_connection_inner(
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

    pub(super) fn finalize_lineup_allocation(
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

    pub(super) fn register_allocation(
        &self,
        allocation: ProviderAllocation,
        params: &AcquireProviderParams<'_>,
    ) -> ProviderHandle {
        let AcquireProviderParams { addr, priority, kind, lease } = *params;
        let session_owner = lease.map(|lease| lease.provider_owner());
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
            let id = leases.begin_owner(lease.provider_owner(), &provider_name, lease.kind, lease.request_id);
            if lease.owner != lease.provider_owner() {
                leases.attach_request_token(id, lease.request_id, lease.owner);
            }
            let generation = leases.lease(id).map_or(1, |lease| lease.binding_generation);
            debug_if_enabled!(
                "Playback lease began: provider={} owner={} kind={} request_id={} lease_id={} generation={} state=starting",
                sanitize_sensitive_info(&provider_name),
                sanitize_sensitive_info(lease.provider_owner()),
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
                request_owner: lease.map(|lease| {
                    session_owner_arc
                        .as_ref()
                        .filter(|owner| owner.as_ref() == lease.owner)
                        .map_or_else(|| Arc::from(lease.owner), Arc::clone)
                }),
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

    pub(super) fn try_acquire_allocation_after_freed(
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
        self.acquire_exact_connection_with_lease_for_session_until(
            provider_name,
            addr,
            allow_grace,
            priority,
            kind,
            lease,
            None,
        )
        .await
    }

    /// Earliest instant at which a playback lease may expire and free reserved capacity, after
    /// pruning the ones already expired. Lease expiry is time based and sends no notification.
    pub fn next_lease_expiry(&self) -> Option<TokioInstant> {
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.next_expiry()
    }

    /// Like [`Self::acquire_exact_connection_with_lease_for_session_await`], but never waits past
    /// `deadline`. A preemption that outlives the deadline is finished by its reaper.
    #[allow(clippy::too_many_arguments)]
    pub async fn acquire_exact_connection_with_lease_for_session_until(
        &self,
        provider_name: &Arc<str>,
        addr: &SocketAddr,
        allow_grace: bool,
        priority: i8,
        kind: ConnectionKind,
        lease: Option<PlaybackLeaseRef<'_>>,
        deadline: Option<TokioInstant>,
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
                let completion_deadline = TokioInstant::now() + PREEMPTION_COMPLETION_TIMEOUT;
                let wait_until = deadline.map_or(completion_deadline, |deadline| deadline.min(completion_deadline));
                let completed = tokio::time::timeout_at(wait_until, completion_token.cancelled()).await.is_ok();
                if self.is_shutting_down.load(Ordering::Acquire) {
                    return None;
                }
                if !completed && wait_until < completion_deadline {
                    // The preemption reaper forces the victim release on time without this caller.
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

    /// Transfers an already reserved recording allocation into the internal
    /// playback request that opens the provider body. The worker keeps its
    /// original handle so preemption can still stop the recording; releases
    /// are allocation-id based and therefore remain idempotent.
    pub fn claim_download_connection(
        &self,
        allocation_id: AllocationId,
        input_name: &Arc<str>,
    ) -> Option<ProviderHandle> {
        let _transition = self.lock_capacity_transition();
        let mut connections = self.write_connections();
        let info = connections.single.get_mut(&allocation_id)?;
        if info.lifecycle != ConnectionLifecycle::Active || info.has_body_owner {
            return None;
        }
        let provider_name = info.allocation.get_provider_name()?;
        if !self.providers.is_provider_for_input(provider_name.as_ref(), input_name.as_ref()) {
            return None;
        }

        // Claim under the capacity-transition lock so a duplicated internal
        // request cannot attach a second provider body to this reservation.
        info.lifecycle = ConnectionLifecycle::Opening;
        let handle = ProviderHandle {
            playback_request_id: info.playback_request_id,
            binding_tag: None,
            client_id: info.client_addr,
            allocation_id: info.allocation_id,
            allocation: info.allocation.clone(),
            cancel_token: Some(info.cancel_token.clone()),
            completion_token: Some(info.completion_token.clone()),
            close_reason: Arc::clone(&info.close_reason),
            open_generation: info.open_generation,
        };
        Some(handle)
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
        let session_owner = session_owner.map(playback_lease_owner);
        self.reserved_capacity_blocks_next(provider_name, session_owner)
    }

    pub fn active_connections(&self) -> Option<HashMap<Arc<str>, usize>> { self.providers.active_connections() }

    pub fn is_exhausted(&self, provider_name: &Arc<str>) -> bool { self.providers.is_exhausted(provider_name) }

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
