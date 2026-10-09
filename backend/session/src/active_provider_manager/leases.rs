use super::{playback_lease_owner, ActiveProviderManager, CapacityReleases, ManagedProviderHandle, PlaybackLeaseRef};
use crate::provider_leases::{ProviderAffinityTtl, ProviderLeaseTable};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{
    AllocationId, AppConfig, ConnectionLifecycle, PlaybackKind, PlaybackLeaseId, PlaybackRequestId,
    PlaybackRequestOutcome, ProviderBindingTag,
};

/// How an acquisition of a playback owner is tied to a provider.
pub(super) enum OwnerProviderPin {
    /// A confirmed or active lease pins the playback to exactly this provider.
    Reserved(Arc<str>),
    /// A recently confirmed playback prefers this provider; it holds no capacity.
    Preferred(Arc<str>),
}

impl<'a> PlaybackLeaseRef<'a> {
    /// Compatibility view of a bare session owner: one request, live TS semantics.
    pub fn for_session_owner(owner: &'a str) -> Self { Self::new(owner, PlaybackKind::LiveTs) }
}

impl CapacityReleases for ProviderLeaseTable {
    fn take_released_providers(&mut self) -> Vec<Arc<str>> { ProviderLeaseTable::take_released_providers(self) }
}

impl ManagedProviderHandle {
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

impl ActiveProviderManager {
    pub(super) fn get_affinity_ttl(cfg: &AppConfig) -> ProviderAffinityTtl {
        ProviderAffinityTtl(cfg.config.load().get_provider_affinity_ttl_secs())
    }

    pub(super) fn has_foreign_reservation(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        if !self.providers.reservation_blocks_other_sessions(provider_name) {
            return false;
        }
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.has_foreign_reserved_lease(provider_name, session_owner)
    }

    /// Resolves how a playback owner is bound to a provider of `input_name`, with a
    /// single lease-table lookup per acquisition.
    pub(super) fn owner_provider_pin(&self, input_name: &Arc<str>, session_owner: &str) -> Option<OwnerProviderPin> {
        let (lease_pin, affinity_provider) = {
            let mut leases = self.write_leases();
            Self::prune_expired_leases(&mut leases);
            let lease_pin = leases
                .lease_of_owner(session_owner)
                .map(|lease| (Arc::clone(&lease.provider_name), lease.state.is_confirmed()));
            (lease_pin, leases.affinity_provider_for_owner(session_owner))
        };
        // The lease lock is released before the connection lock is taken.
        if let Some((provider_name, confirmed)) = lease_pin {
            if self.providers.is_provider_for_input(&provider_name, input_name)
                && (confirmed || self.has_active_owner_for_provider(&provider_name, session_owner))
            {
                return Some(OwnerProviderPin::Reserved(provider_name));
            }
        }
        affinity_provider
            .filter(|provider_name| self.providers.is_provider_for_input(provider_name, input_name))
            .map(OwnerProviderPin::Preferred)
    }

    /// Provider a playback currently prefers on re-entry, for diagnostics and tests.
    pub fn provider_affinity_for_owner(&self, session_owner: &str) -> Option<Arc<str>> {
        let session_owner = playback_lease_owner(session_owner);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.affinity_provider_for_owner(session_owner)
    }

    /// True when `session_owner` already backs a live allocation on `provider_name`,
    /// without materialising the full owner set for a membership test.
    pub(super) fn has_active_owner_for_provider(&self, provider_name: &Arc<str>, session_owner: &str) -> bool {
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

    pub(super) fn active_reservation_owners(&self, provider_name: &Arc<str>) -> HashSet<Arc<str>> {
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
        let session_owner = playback_lease_owner(session_owner);
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

    /// The reconnect window a renewal may actually grant for `owner`.
    ///
    /// A lease that currently holds no reservation right must not gain one through a
    /// mere activity refresh: the grant is re-checked against provider capacity with
    /// the same admission rule as a confirmation, so a previously denied reservation
    /// cannot be silently restored by a later refresh.
    pub(super) fn renewal_granted_ttl(&self, provider_name: &Arc<str>, owner: &str, requested_ttl_secs: u64) -> u64 {
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

    /// Clears an explicit owner. Public HLS tokens intentionally have no owner-wide
    /// delete right: a delayed session cleanup must not erase a retry's shared lease.
    /// HLS request cleanup uses request IDs or binding tags instead.
    pub fn clear_provider_reservation(&self, session_owner: &str) {
        if playback_lease_owner(session_owner) != session_owner {
            return;
        }
        let _transition = self.lock_capacity_transition();
        let mut leases = self.write_leases();
        leases.forget_affinity(session_owner);
        leases.release_owner(session_owner);
    }

    /// Administrative end of the playback behind a session that was actually terminated
    /// (terminate endpoint, kick).
    ///
    /// A public HLS token shares its stable owner with the retries of the same
    /// client/user/channel, so it only ends the binding it acquired itself. A stale
    /// token therefore never removes the lease or provider preference of a newer
    /// binding. A bare owner keeps the owner-wide release semantics.
    pub fn terminate_identified_playback_owner(&self, session_token: &str) -> bool {
        let owner = playback_lease_owner(session_token);
        let _transition = self.lock_capacity_transition();
        let mut leases = self.write_leases();
        if owner == session_token {
            leases.forget_affinity(owner);
            return leases.release_owner(owner).is_some();
        }
        leases.terminate_identified_token(owner, session_token)
    }

    /// Ends the provider preference of a failed playback, but only while it still
    /// belongs to the exact binding that failed. A delayed failure of an older binding
    /// leaves the successor's preference intact.
    pub fn forget_identified_provider_affinity(
        &self,
        session_owner: &str,
        provider_name: &Arc<str>,
        binding_tag: ProviderBindingTag,
    ) -> bool {
        let session_owner = playback_lease_owner(session_owner);
        let _transition = self.lock_capacity_transition();
        self.write_leases().forget_identified_affinity(session_owner, provider_name, binding_tag)
    }

    pub fn clear_identified_provider_reservation(
        &self,
        session_owner: &str,
        provider_name: &Arc<str>,
        binding_tag: Option<ProviderBindingTag>,
    ) {
        let session_owner = playback_lease_owner(session_owner);
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
        let owner = playback_lease_owner(owner);
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

    pub(super) fn finish_playback_request_inner(
        &self,
        owner: &str,
        request_id: Option<PlaybackRequestId>,
        outcome: PlaybackRequestOutcome,
        idle_ttl_secs: Option<u64>,
    ) {
        let owner = playback_lease_owner(owner);
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

    /// The provider lease's binding tag for an owner. The HLS layer captures this
    /// from the acquired handle (via `ProviderHandle::binding_tag`) and passes it
    /// back to [`Self::clear_identified_provider_reservation`] so a stale detach
    /// cannot delete a successor lease on the same account.
    pub fn binding_tag_for_owner(&self, owner: &str) -> Option<ProviderBindingTag> {
        let owner = playback_lease_owner(owner);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        leases.lease_of_owner(owner).map(|lease| ProviderBindingTag::new(lease.id, lease.binding_generation))
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
}
