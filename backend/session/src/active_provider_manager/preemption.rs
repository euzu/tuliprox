use super::{
    AcquireProviderParams, ActiveProviderManager, ActiveProviderManagerCore, ConnectionKind, Connections,
    OwnerProviderPin, PlaybackLeaseRef, PreemptionOutcome, ProviderAllocationGuard, ReleaseAction, SharedConnectionId,
    PREEMPTION_COMPLETION_TIMEOUT,
};
use ::shared::utils::sanitize_sensitive_info;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::{atomic::Ordering, Arc, Weak},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{
        AllocationId, AppConfig, ConnectionLifecycle, GracePeriodOptions, ProviderAllocation, ProviderCloseReason,
        ProviderHandle,
    },
    utils::debug_if_enabled,
};

type PreemptionCandidate = (PriorityOwner, AllocationId, i8, Instant);

// Key for BTreeMap priority index: (priority, Reverse<created_at>, AllocationId)
// Semantics: lower numeric priority value = higher importance (0 = highest, 127 = lowest).
// `.last()` on the BTreeMap returns the entry with the highest priority value, which is
// the lowest-importance connection and therefore the best eviction victim.
// Ties are broken by `Reverse<Instant>`: among equal priority values, the oldest connection
// (smallest `created_at`) sorts last and is evicted first.
pub(super) type PriorityKey = (i8, Reverse<Instant>, AllocationId);

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
pub(super) enum PriorityOwner {
    Single(AllocationId),
    Shared(SharedConnectionId),
}

impl ActiveProviderManager {
    pub(super) fn upsert_priority_entry(
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

    pub(super) fn remove_priority_entry(
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

    pub(super) fn get_grace_options(cfg: &AppConfig) -> GracePeriodOptions { cfg.config.load().get_grace_options() }

    pub(super) fn acquire_exact_connection_inner_no_preempt(
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

    pub(super) fn select_preemption_candidate(
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

    pub(super) fn acquire_connection_inner_no_preempt(
        &self,
        provider_or_input_name: &Arc<str>,
        allow_grace: bool,
        params: &AcquireProviderParams<'_>,
    ) -> Option<ProviderHandle> {
        if let Some(owner) = params.session_owner() {
            match self.owner_provider_pin(provider_or_input_name, owner) {
                Some(OwnerProviderPin::Reserved(reserved_provider)) => {
                    return self.acquire_exact_connection_inner_no_preempt(&reserved_provider, allow_grace, params);
                }
                // A continuing playback whose reconnect lease lapsed during a request gap
                // returns to its provider before priority selection, but only into free
                // capacity: the lineup prefers any free slot over a grace over-allocation,
                // so the preference must not force one either.
                Some(OwnerProviderPin::Preferred(affinity_provider)) => {
                    if let Some(handle) =
                        self.acquire_exact_connection_inner_no_preempt(&affinity_provider, false, params)
                    {
                        debug_if_enabled!(
                            "Provider affinity kept: provider={} owner={} playback_kind={}",
                            sanitize_sensitive_info(&affinity_provider),
                            sanitize_sensitive_info(owner),
                            params.lease.map_or_else(|| "-".to_string(), |lease| lease.kind.to_string())
                        );
                        return Some(handle);
                    }
                    debug_if_enabled!(
                        "Provider affinity unavailable, using lineup: provider={} owner={}",
                        sanitize_sensitive_info(&affinity_provider),
                        sanitize_sensitive_info(owner)
                    );
                }
                None => {}
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

    #[allow(clippy::too_many_lines)]
    /// Evict a single lower-priority connection across the entire input lineup
    /// (all provider aliases). Used when a `GracePeriod` allocation was granted.
    /// Returns true if a victim was successfully evicted.
    pub(super) fn evict_lower_priority_on_input(
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

    #[allow(clippy::too_many_lines)]
    pub(super) fn try_preempt_connection_outcome(
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
                        self.spawn_preemption_reaper(victim_id, gen, comp_token.clone());
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
                            if let Some((victim_id, gen)) = victim_identity {
                                self.spawn_preemption_reaper(victim_id, gen, token.clone());
                            }
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

    pub(super) fn try_preempt_connection(
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

    /// Frees a preempted victim's slot once its body completes, or after
    /// `PREEMPTION_COMPLETION_TIMEOUT` at the latest. It runs independently of the preempting
    /// request, so a request dropped while waiting (client disconnect, deadline, cancelled
    /// future) cannot leave the victim's slot occupied.
    fn spawn_preemption_reaper(&self, victim_id: AllocationId, generation: u64, completion_token: CancellationToken) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let shutdown_token = self.shutdown_token.clone();
        let core = Arc::clone(&self.core);
        runtime.spawn(async move {
            tokio::select! {
                () = shutdown_token.cancelled() => {}
                _ = tokio::time::timeout(PREEMPTION_COMPLETION_TIMEOUT, completion_token.cancelled()) => {
                    core.complete_release_with_generation(victim_id, Some(generation));
                }
            }
        });
    }
}
