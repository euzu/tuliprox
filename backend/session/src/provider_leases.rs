//! Provider capacity slot leases.
//!
//! A lease is one logical provider slot owned by a playback, never by a transport
//! socket. Active allocations and idle reservations are two states of the same
//! lease. The provider manager serializes allocation counters, indices and lease
//! transitions when deciding capacity.
//!
//! Lifecycle:
//!
//! ```text
//! allocation acquired -> Starting (short startup deadline, blocks nobody)
//!   confirmed media activity -> Active (expires on inactivity)
//!   request finished cleanly + reconnect capable -> Idle (reconnect window)
//!   never confirmed / failed / expired -> removed
//! ```
//!
//! A lease only reserves capacity against other playbacks once real media activity
//! confirmed it *and* the endpoint configured a reconnect window. A manifest
//! response with HTTP 200 is not playback: treating it as one let abandoned starts
//! and manifest retries pile up reservations that outlived the streams they belonged
//! to and blocked unrelated clients behind the same reverse proxy.

use log::debug;
use shared::utils::sanitize_sensitive_info;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant as TokioInstant;
use tuliprox_core::model::{
    PlaybackKind, PlaybackLeaseId, PlaybackRequestId, PlaybackRequestOutcome, ProviderBindingTag,
};

/// Hard upper bound for an unconfirmed lease.
///
/// Provider open plus first media byte normally completes well inside this window.
/// Anything that never confirms is an abandoned start and must stop holding
/// capacity, independently of the configured reconnect TTL.
pub const STARTING_LEASE_TTL_SECS: u64 = 5;

#[derive(Debug, Clone)]
pub enum ProviderLeaseState {
    Starting { expires_at: TokioInstant },
    Active { request_id: PlaybackRequestId, expires_at: TokioInstant },
    Idle { expires_at: TokioInstant },
}

impl ProviderLeaseState {
    #[inline]
    pub fn is_active(&self) -> bool { matches!(self, Self::Active { .. }) }

    #[inline]
    pub fn is_idle(&self) -> bool { matches!(self, Self::Idle { .. }) }

    /// True once real media activity was recorded.
    #[inline]
    pub fn is_confirmed(&self) -> bool { matches!(self, Self::Active { .. } | Self::Idle { .. }) }

    fn expires_at(&self) -> TokioInstant {
        match self {
            Self::Starting { expires_at } | Self::Active { expires_at, .. } | Self::Idle { expires_at } => *expires_at,
        }
    }

    pub fn as_log_state(&self) -> &'static str {
        match self {
            Self::Starting { .. } => "starting",
            Self::Active { .. } => "active",
            Self::Idle { .. } => "idle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderSlotLease {
    pub id: PlaybackLeaseId,
    /// Stable playback owner. Never a socket address.
    pub owner: Arc<str>,
    pub provider_name: Arc<str>,
    pub kind: PlaybackKind,
    pub state: ProviderLeaseState,
    pub request_id: PlaybackRequestId,
    /// Monotonic incarnation of this owner's provider binding.
    pub binding_generation: u64,
    /// Requests attached to the current binding incarnation. Parallel segment and
    /// range requests remain valid until that incarnation is explicitly rebound.
    request_ids: HashSet<PlaybackRequestId>,
    /// Reconnect window configured for this playback; `0` means the endpoint wants no
    /// reservation, so the lease never blocks foreign capacity.
    pub idle_ttl_secs: u64,
    pub created_at: TokioInstant,
    pub last_activity_at: TokioInstant,
}

impl ProviderSlotLease {
    /// True when this lease holds a slot back for its playback against others.
    #[inline]
    fn reserves_capacity(&self) -> bool { self.state.is_confirmed() && self.idle_ttl_secs > 0 }

    pub fn idle_ttl_remaining_ms(&self, now: TokioInstant) -> u64 {
        self.state
            .expires_at()
            .checked_duration_since(now)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    #[inline]
    pub fn contains_request(&self, request_id: PlaybackRequestId) -> bool { self.request_ids.contains(&request_id) }
}

/// Capacity-relevant view of one provider's lease table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderLeaseUsage {
    pub starting: usize,
    pub active: usize,
    pub idle: usize,
}

impl ProviderLeaseUsage {
    /// Logical slots held by this provider's leases.
    #[inline]
    pub fn total(&self) -> usize { self.starting + self.active + self.idle }
}

type ByOwner = HashMap<Arc<str>, PlaybackLeaseId>;
type ByProvider = HashMap<Arc<str>, HashSet<PlaybackLeaseId>>;

/// Owner-keyed table of provider capacity slot leases.
///
/// One owner is one logical playback, so repeated requests of the same playback
/// (manifest retries, segment fetches, range reconnects) share one slot instead of
/// stacking a reservation per attempt.
#[derive(Debug, Default)]
pub struct ProviderLeaseTable {
    leases: HashMap<PlaybackLeaseId, ProviderSlotLease>,
    by_owner: ByOwner,
    by_provider: ByProvider,
    // Exactly one deadline per lease; renewals replace entries rather than leaving
    // stale heap entries that grow with the number of segment requests.
    expirations: BTreeSet<(TokioInstant, PlaybackLeaseId)>,
}

impl ProviderLeaseTable {
    #[inline]
    pub fn is_empty(&self) -> bool { self.leases.is_empty() }

    #[inline]
    pub fn len(&self) -> usize { self.leases.len() }

    /// Removes every lease during terminal server shutdown.
    pub fn clear(&mut self) { *self = Self::default(); }

    pub fn lease(&self, id: PlaybackLeaseId) -> Option<&ProviderSlotLease> { self.leases.get(&id) }

    pub fn lease_of_owner(&self, owner: &str) -> Option<&ProviderSlotLease> {
        self.by_owner.get(owner).and_then(|id| self.leases.get(id))
    }

    fn index(by_owner: &mut ByOwner, by_provider: &mut ByProvider, lease: &ProviderSlotLease) {
        by_owner.insert(Arc::clone(&lease.owner), lease.id);
        by_provider.entry(Arc::clone(&lease.provider_name)).or_default().insert(lease.id);
    }

    fn deindex(by_owner: &mut ByOwner, by_provider: &mut ByProvider, lease: &ProviderSlotLease) {
        if by_owner.get(&lease.owner) == Some(&lease.id) {
            by_owner.remove(&lease.owner);
        }
        if let Some(ids) = by_provider.get_mut(&lease.provider_name) {
            ids.remove(&lease.id);
            if ids.is_empty() {
                by_provider.remove(&lease.provider_name);
            }
        }
    }

    fn remove_lease(&mut self, id: PlaybackLeaseId) -> Option<ProviderSlotLease> {
        let lease = self.leases.remove(&id)?;
        self.expirations.remove(&(lease.state.expires_at(), id));
        Self::deindex(&mut self.by_owner, &mut self.by_provider, &lease);
        Some(lease)
    }

    fn rebind_lease_provider(
        by_owner: &mut ByOwner,
        by_provider: &mut ByProvider,
        lease: &mut ProviderSlotLease,
        new_provider: &Arc<str>,
    ) {
        if lease.provider_name != *new_provider {
            Self::deindex(by_owner, by_provider, lease);
            lease.provider_name = Arc::clone(new_provider);
            Self::index(by_owner, by_provider, lease);
        }
    }

    fn startup_expiry(now: TokioInstant, idle_ttl_secs: u64) -> TokioInstant {
        let ttl = if idle_ttl_secs == 0 { STARTING_LEASE_TTL_SECS } else { STARTING_LEASE_TTL_SECS.min(idle_ttl_secs) };
        now + Duration::from_secs(ttl)
    }

    fn confirmed_expiry(now: TokioInstant, idle_ttl_secs: u64) -> TokioInstant {
        now + Duration::from_secs(idle_ttl_secs.max(1))
    }

    /// Claims a slot for one playback at allocation time, before any media flows.
    ///
    /// An existing lease on the same provider attaches the request to its current
    /// generation. Selecting a different provider starts a new generation and
    /// invalidates delayed mutations from the preceding binding.
    pub fn begin_owner(
        &mut self,
        owner: &str,
        provider_name: &Arc<str>,
        kind: PlaybackKind,
        request_id: PlaybackRequestId,
    ) -> PlaybackLeaseId {
        let now = TokioInstant::now();
        if let Some(id) = self.by_owner.get(owner).copied() {
            if let Some(lease) = self.leases.get_mut(&id) {
                self.expirations.remove(&(lease.state.expires_at(), id));
                if lease.provider_name != *provider_name {
                    Self::rebind_lease_provider(&mut self.by_owner, &mut self.by_provider, lease, provider_name);
                    lease.binding_generation = lease.binding_generation.saturating_add(1);
                    lease.request_ids.clear();
                    lease.created_at = now;
                    lease.idle_ttl_secs = 0;
                    lease.state = ProviderLeaseState::Starting { expires_at: Self::startup_expiry(now, 0) };
                } else if lease.state.is_idle() {
                    lease.binding_generation = lease.binding_generation.saturating_add(1);
                    lease.request_ids.clear();
                    lease.created_at = now;
                    lease.state =
                        ProviderLeaseState::Starting { expires_at: Self::startup_expiry(now, lease.idle_ttl_secs) };
                } else if !lease.state.is_confirmed() {
                    lease.state = ProviderLeaseState::Starting {
                        expires_at: Self::startup_expiry(lease.created_at, lease.idle_ttl_secs),
                    };
                }
                // The kind is authoritative from the first acquire of this playback and
                // is never overwritten, so a compatibility renewal cannot downgrade a
                // reconnect-capable lease to a one-shot live TS one.
                lease.request_id = request_id;
                lease.request_ids.insert(request_id);
                lease.last_activity_at = now;
                self.expirations.insert((lease.state.expires_at(), id));
                return id;
            }
        }

        let lease = ProviderSlotLease {
            id: PlaybackLeaseId::next(),
            owner: Arc::from(owner),
            provider_name: Arc::clone(provider_name),
            kind,
            state: ProviderLeaseState::Starting { expires_at: Self::startup_expiry(now, 0) },
            request_id,
            binding_generation: 1,
            request_ids: HashSet::from([request_id]),
            idle_ttl_secs: 0,
            created_at: now,
            last_activity_at: now,
        };
        let id = lease.id;
        self.expirations.insert((lease.state.expires_at(), id));
        Self::index(&mut self.by_owner, &mut self.by_provider, &lease);
        self.leases.insert(id, lease);
        id
    }

    /// Renews the reconnect window of an existing request claim.
    ///
    /// A confirmed lease stays confirmed and only extends its window, so a follow-up
    /// request of a running playback never downgrades it. An unconfirmed lease stays
    /// bounded by [`STARTING_LEASE_TTL_SECS`] and keeps blocking nobody.
    pub fn renew_identified_owner(
        &mut self,
        owner: &str,
        provider_name: &Arc<str>,
        kind: PlaybackKind,
        request_id: PlaybackRequestId,
        idle_ttl_secs: u64,
    ) -> Option<PlaybackLeaseId> {
        if owner.is_empty() {
            return None;
        }
        let now = TokioInstant::now();
        if let Some(id) = self.by_owner.get(owner).copied() {
            let lease = self.leases.get_mut(&id)?;
            if lease.provider_name != *provider_name || !lease.contains_request(request_id) {
                return None;
            }
            self.expirations.remove(&(lease.state.expires_at(), id));
            let was_confirmed = lease.state.is_confirmed();
            // The renewing endpoint may know the true media kind (VOD/Series/HLS via
            // classification), so adopt it — but never downgrade a reconnect-capable
            // lease, because compatibility refreshes default to one-shot live TS and
            // must not strip an HLS/DASH/Catchup playback of its reconnect window.
            if !lease.kind.is_reconnect_capable() {
                lease.kind = kind;
            }
            lease.last_activity_at = now;
            lease.idle_ttl_secs = idle_ttl_secs;
            lease.state = if was_confirmed {
                ProviderLeaseState::Active {
                    request_id: lease.request_id,
                    expires_at: Self::confirmed_expiry(now, idle_ttl_secs),
                }
            } else {
                ProviderLeaseState::Starting { expires_at: Self::startup_expiry(lease.created_at, idle_ttl_secs) }
            };
            self.expirations.insert((lease.state.expires_at(), id));
            return Some(id);
        }
        None
    }

    /// Renews the current binding without manufacturing a request identity.
    pub fn renew_current_owner(
        &mut self,
        owner: &str,
        provider_name: &Arc<str>,
        kind: PlaybackKind,
        idle_ttl_secs: u64,
    ) -> Option<PlaybackLeaseId> {
        let request_id = self.lease_of_owner(owner)?.request_id;
        self.renew_identified_owner(owner, provider_name, kind, request_id, idle_ttl_secs)
    }

    /// Starts a new binding incarnation, including when the provider name stays the same.
    pub fn rebind_owner(
        &mut self,
        owner: &str,
        provider_name: &Arc<str>,
        kind: PlaybackKind,
        request_id: PlaybackRequestId,
    ) -> PlaybackLeaseId {
        self.release_owner(owner);
        self.begin_owner(owner, provider_name, kind, request_id)
    }

    /// Records confirmed media activity for a playback owner.
    ///
    /// Only a confirmed lease with a reconnect window may reserve provider capacity
    /// against other playbacks.
    pub fn confirm_owner(&mut self, owner: &str) -> Option<PlaybackLeaseId> {
        let id = self.by_owner.get(owner).copied()?;
        self.apply_confirmation(id, true)
    }

    /// Confirms media activity without granting a reservation right, so the lease
    /// never blocks foreign capacity. Used when the provider has no free slot for a
    /// reconnect window but the media bytes are still delivered.
    pub fn confirm_owner_without_reservation(&mut self, owner: &str) -> Option<PlaybackLeaseId> {
        let id = self.by_owner.get(owner).copied()?;
        self.apply_confirmation(id, false)
    }

    /// Confirms real media activity for a specific request ID of a playback lease.
    /// If the lease has moved to a newer request ID, the confirmation is rejected.
    pub fn confirm_identified_owner(&mut self, owner: &str, request_id: PlaybackRequestId) -> Option<PlaybackLeaseId> {
        let id = self.by_owner.get(owner).copied()?;
        let lease = self.leases.get(&id)?;
        if !lease.contains_request(request_id) {
            return None;
        }
        self.apply_confirmation(id, true)
    }

    /// Same as [`Self::confirm_identified_owner`] but records no reservation right.
    pub fn confirm_identified_owner_without_reservation(
        &mut self,
        owner: &str,
        request_id: PlaybackRequestId,
    ) -> Option<PlaybackLeaseId> {
        let id = self.by_owner.get(owner).copied()?;
        let lease = self.leases.get(&id)?;
        if !lease.contains_request(request_id) {
            return None;
        }
        self.apply_confirmation(id, false)
    }

    fn apply_confirmation(&mut self, id: PlaybackLeaseId, reserve: bool) -> Option<PlaybackLeaseId> {
        let now = TokioInstant::now();
        let expired = self.leases.get(&id).is_some_and(|lease| {
            lease.state.expires_at() <= now
                && !(matches!(lease.state, ProviderLeaseState::Active { .. }) && lease.idle_ttl_secs == 0)
        });
        if expired {
            self.remove_lease(id);
            return None;
        }
        let lease = self.leases.get_mut(&id)?;
        self.expirations.remove(&(lease.state.expires_at(), id));
        lease.last_activity_at = now;
        if !reserve {
            lease.idle_ttl_secs = 0;
        }
        lease.state = ProviderLeaseState::Active {
            request_id: lease.request_id,
            expires_at: Self::confirmed_expiry(now, lease.idle_ttl_secs),
        };
        self.expirations.insert((lease.state.expires_at(), id));
        Some(id)
    }

    /// Ends one playback request and applies the outcome-specific lease policy.
    ///
    /// A confirmed, reconnect-capable playback with a configured window keeps its
    /// slot as an idle lease; everything else is removed immediately, so an aborted
    /// start never occupies provider capacity.
    pub fn finish_owner(
        &mut self,
        owner: &str,
        outcome: PlaybackRequestOutcome,
        idle_ttl_secs: Option<u64>,
    ) -> Option<ProviderSlotLease> {
        let id = self.by_owner.get(owner).copied()?;
        let now = TokioInstant::now();
        let keep_idle = {
            let lease = self.leases.get_mut(&id)?;
            lease.last_activity_at = now;
            let ttl = idle_ttl_secs.unwrap_or(lease.idle_ttl_secs);
            let keep = lease.state.is_confirmed()
                && outcome.keeps_idle_lease()
                && lease.kind.is_reconnect_capable()
                && ttl > 0;
            if keep {
                self.expirations.remove(&(lease.state.expires_at(), id));
                lease.idle_ttl_secs = ttl;
                lease.state = ProviderLeaseState::Idle { expires_at: now + Duration::from_secs(ttl) };
                self.expirations.insert((lease.state.expires_at(), id));
            }
            keep
        };
        if keep_idle {
            return None;
        }
        self.remove_lease(id)
    }

    pub fn release(&mut self, id: PlaybackLeaseId) -> Option<ProviderSlotLease> { self.remove_lease(id) }

    pub fn release_owner(&mut self, owner: &str) -> Option<ProviderSlotLease> {
        let id = self.by_owner.get(owner).copied()?;
        self.remove_lease(id)
    }

    /// Releases the lease only if it matches the exact provider binding tag.
    ///
    /// The tag binds a lease id and its incarnation generation, so a delayed clear
    /// cannot delete a successor lease that was recreated on the same account and
    /// happens to share generation `1`. There is deliberately no owner-wide fallback
    /// here: callers that need an administrative owner-wide release must use
    /// [`Self::release_owner`] explicitly.
    pub fn release_matching_owner(
        &mut self,
        owner: &str,
        provider_name: &Arc<str>,
        binding_tag: ProviderBindingTag,
    ) -> Option<ProviderSlotLease> {
        let id = self.by_owner.get(owner).copied()?;
        let lease = self.leases.get(&id)?;
        if lease.provider_name != *provider_name
            || lease.id != binding_tag.lease_id
            || lease.binding_generation != binding_tag.generation
        {
            return None;
        }
        self.remove_lease(id)
    }

    /// Ends one identified playback request claim and applies the outcome-specific lease policy
    /// once the last attached request finishes.
    pub fn finish_identified_owner(
        &mut self,
        owner: &str,
        request_id: PlaybackRequestId,
        outcome: PlaybackRequestOutcome,
        idle_ttl_secs: Option<u64>,
    ) -> Option<ProviderSlotLease> {
        let id = self.by_owner.get(owner).copied()?;
        let now = TokioInstant::now();
        let keep_idle = {
            let lease = self.leases.get_mut(&id)?;
            if !lease.request_ids.remove(&request_id) {
                return None;
            }
            if !lease.request_ids.is_empty() {
                if lease.request_id == request_id {
                    if let Some(next) = lease.request_ids.iter().next().copied() {
                        lease.request_id = next;
                    }
                }
                return None;
            }
            lease.last_activity_at = now;
            let ttl = idle_ttl_secs.unwrap_or(lease.idle_ttl_secs);
            let keep = lease.state.is_confirmed()
                && outcome.keeps_idle_lease()
                && lease.kind.is_reconnect_capable()
                && ttl > 0;
            if keep {
                self.expirations.remove(&(lease.state.expires_at(), id));
                lease.idle_ttl_secs = ttl;
                lease.state = ProviderLeaseState::Idle { expires_at: now + Duration::from_secs(ttl) };
                self.expirations.insert((lease.state.expires_at(), id));
            }
            keep
        };
        if keep_idle {
            None
        } else {
            self.remove_lease(id)
        }
    }

    /// Releases one request claim. A stale generation cannot clear its successor.
    pub fn release_identified_owner(
        &mut self,
        owner: &str,
        request_id: PlaybackRequestId,
    ) -> Option<ProviderSlotLease> {
        let id = self.by_owner.get(owner).copied()?;
        let lease = self.leases.get_mut(&id)?;
        if !lease.request_ids.remove(&request_id) {
            return None;
        }
        if let Some(next) = lease.request_ids.iter().next().copied() {
            if lease.request_id == request_id {
                lease.request_id = next;
            }
            return None;
        }
        self.remove_lease(id)
    }

    /// Detaches one request claim from the owner's lease without terminating the lease.
    pub fn detach_identified_request(&mut self, owner: &str, request_id: PlaybackRequestId) -> bool {
        let Some(id) = self.by_owner.get(owner).copied() else {
            return false;
        };
        let Some(lease) = self.leases.get_mut(&id) else {
            return false;
        };
        if !lease.request_ids.remove(&request_id) {
            return false;
        }
        if lease.request_id == request_id {
            if let Some(next) = lease.request_ids.iter().next().copied() {
                lease.request_id = next;
            } else {
                lease.request_id = PlaybackRequestId::default();
            }
        }
        true
    }

    /// The provider this owner currently holds a lease on.
    pub fn provider_for_owner(&self, owner: &str) -> Option<Arc<str>> {
        self.lease_of_owner(owner).map(|lease| Arc::clone(&lease.provider_name))
    }

    pub fn usage(&self, provider_name: &Arc<str>) -> ProviderLeaseUsage {
        let mut usage = ProviderLeaseUsage::default();
        let Some(ids) = self.by_provider.get(provider_name) else {
            return usage;
        };
        for id in ids {
            let Some(lease) = self.leases.get(id) else { continue };
            match lease.state {
                ProviderLeaseState::Starting { .. } => usage.starting += 1,
                ProviderLeaseState::Active { .. } => usage.active += 1,
                ProviderLeaseState::Idle { .. } => usage.idle += 1,
            }
        }
        usage
    }

    /// Confirmed leases that reserve a slot on this provider for somebody else.
    ///
    /// Unconfirmed `Starting` leases and leases without a reconnect window are
    /// excluded: a manifest response alone is not playback. `counted_owners` already
    /// hold a real allocation, so their lease is the same slot and must not be added
    /// on top of the connection counter.
    pub fn foreign_reserved_slots(
        &self,
        provider_name: &Arc<str>,
        session_owner: Option<&str>,
        counted_owners: &HashSet<Arc<str>>,
    ) -> usize {
        let Some(ids) = self.by_provider.get(provider_name) else {
            return 0;
        };
        ids.iter()
            .filter_map(|id| self.leases.get(id))
            .filter(|lease| lease.reserves_capacity())
            .filter(|lease| session_owner != Some(lease.owner.as_ref()) && !counted_owners.contains(&lease.owner))
            .count()
    }

    pub fn has_foreign_reserved_lease(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        let Some(ids) = self.by_provider.get(provider_name) else {
            return false;
        };
        ids.iter()
            .filter_map(|id| self.leases.get(id))
            .filter(|lease| lease.reserves_capacity())
            .any(|lease| session_owner != Some(lease.owner.as_ref()))
    }

    /// Removes expired leases of any state.
    pub fn prune(&mut self, now: TokioInstant) -> Vec<ProviderSlotLease> {
        if self.leases.is_empty() {
            return Vec::new();
        }
        let mut removed = Vec::new();
        while let Some((deadline, id)) = self.expirations.first().copied() {
            if deadline > now {
                break;
            }
            self.expirations.pop_first();
            // Active leases with idle_ttl_secs == 0 (VOD, Series, Live-TS)
            // represent a running stream with no reconnect window configured.
            // They must not be pruned by wall-clock time; only explicit
            // finish_owner / remove_lease can end them.
            let should_remove = !matches!(
                self.leases.get(&id),
                Some(lease)
                    if matches!(lease.state, ProviderLeaseState::Active { .. })
                        && lease.idle_ttl_secs == 0
            );
            if should_remove {
                if let Some(lease) = self.remove_lease(id) {
                    debug!(
                        "Provider lease expired: provider={} owner={} state={} kind={} lease_id={} request_id={}",
                        sanitize_sensitive_info(&lease.provider_name),
                        sanitize_sensitive_info(&lease.owner),
                        lease.state.as_log_state(),
                        lease.kind,
                        lease.id,
                        lease.request_id
                    );
                    removed.push(lease);
                }
            }
        }
        removed
    }

    pub fn iter(&self) -> impl Iterator<Item = &ProviderSlotLease> { self.leases.values() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> Arc<str> { Arc::from(name) }

    #[tokio::test(start_paused = true)]
    async fn repeated_begin_keeps_one_starting_lease() {
        let mut table = ProviderLeaseTable::default();
        for index in 0..5 {
            table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, PlaybackRequestId::from_raw(index));
        }
        assert_eq!(table.len(), 1, "manifest retries must not stack provider slots");
        assert_eq!(table.usage(&provider("A")), ProviderLeaseUsage { starting: 1, active: 0, idle: 0 });
    }

    #[tokio::test(start_paused = true)]
    async fn stale_request_cannot_confirm_a_newer_playback_generation() {
        let mut table = ProviderLeaseTable::default();
        let stale_request = PlaybackRequestId::from_raw(1);
        let current_request = PlaybackRequestId::from_raw(2);
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, stale_request);
        table.rebind_owner("owner", &provider("A"), PlaybackKind::LiveHls, current_request);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, current_request, 15);

        assert!(table.confirm_identified_owner("owner", stale_request).is_none());
        assert_eq!(table.usage(&provider("A")), ProviderLeaseUsage { starting: 1, active: 0, idle: 0 });
        assert!(table.confirm_identified_owner("owner", current_request).is_some());
        assert_eq!(table.usage(&provider("A")), ProviderLeaseUsage { starting: 0, active: 1, idle: 0 });
    }

    #[tokio::test(start_paused = true)]
    async fn parallel_attached_claims_can_both_confirm() {
        let mut table = ProviderLeaseTable::default();
        let req1 = PlaybackRequestId::from_raw(1);
        let req2 = PlaybackRequestId::from_raw(2);
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, req1);
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, req2);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, req2, 15);

        assert!(table.confirm_identified_owner("owner", req1).is_some());
        assert_eq!(table.usage(&provider("A")), ProviderLeaseUsage { starting: 0, active: 1, idle: 0 });
        assert!(table.confirm_identified_owner("owner", req2).is_some());
        assert_eq!(table.usage(&provider("A")), ProviderLeaseUsage { starting: 0, active: 1, idle: 0 });
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_lease_expires_on_startup_deadline() {
        let mut table = ProviderLeaseTable::default();
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, PlaybackRequestId::from_raw(1));
        table.renew_identified_owner(
            "owner",
            &provider("A"),
            PlaybackKind::LiveHls,
            PlaybackRequestId::from_raw(1),
            15,
        );

        tokio::time::advance(Duration::from_secs(STARTING_LEASE_TTL_SECS + 1)).await;
        assert_eq!(table.prune(TokioInstant::now()).len(), 1);
        assert!(table.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn starting_lease_never_blocks_other_playbacks() {
        let mut table = ProviderLeaseTable::default();
        table.begin_owner("owner-a", &provider("A"), PlaybackKind::LiveHls, PlaybackRequestId::from_raw(1));
        table.renew_identified_owner(
            "owner-a",
            &provider("A"),
            PlaybackKind::LiveHls,
            PlaybackRequestId::from_raw(1),
            15,
        );

        assert_eq!(table.foreign_reserved_slots(&provider("A"), Some("owner-b"), &HashSet::new()), 0);
        assert!(!table.has_foreign_reserved_lease(&provider("A"), Some("owner-b")));
    }

    #[tokio::test(start_paused = true)]
    async fn confirmed_lease_blocks_until_its_window_expires() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        table.begin_owner("owner-a", &provider("A"), PlaybackKind::LiveHls, request);
        table.renew_identified_owner("owner-a", &provider("A"), PlaybackKind::LiveHls, request, 15);
        assert!(table.confirm_owner("owner-a").is_some());

        assert_eq!(table.foreign_reserved_slots(&provider("A"), Some("owner-b"), &HashSet::new()), 1);
        assert!(table.has_foreign_reserved_lease(&provider("A"), Some("owner-b")));
        // The owner's own live allocation is not counted twice.
        let mut counted = HashSet::new();
        counted.insert(Arc::from("owner-a"));
        assert_eq!(table.foreign_reserved_slots(&provider("A"), Some("owner-b"), &counted), 0);

        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(table.prune(TokioInstant::now()).len(), 1);
        assert!(!table.has_foreign_reserved_lease(&provider("A"), Some("owner-b")));
    }

    #[tokio::test(start_paused = true)]
    async fn confirmed_lease_without_window_does_not_block() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        table.begin_owner("owner-a", &provider("A"), PlaybackKind::Vod, request);
        assert!(table.confirm_owner("owner-a").is_some());
        assert_eq!(table.foreign_reserved_slots(&provider("A"), Some("owner-b"), &HashSet::new()), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn confirmation_without_reservation_records_media_but_never_blocks() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        table.begin_owner("owner-a", &provider("A"), PlaybackKind::LiveHls, request);
        table.renew_identified_owner("owner-a", &provider("A"), PlaybackKind::LiveHls, request, 15);

        assert!(table.confirm_identified_owner_without_reservation("owner-a", request).is_some());
        assert_eq!(table.usage(&provider("A")), ProviderLeaseUsage { starting: 0, active: 1, idle: 0 });
        assert_eq!(
            table.foreign_reserved_slots(&provider("A"), Some("owner-b"), &HashSet::new()),
            0,
            "media-without-reservation must not reserve capacity against other playbacks"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_clear_after_same_account_rebind_preserves_new_lease() {
        let mut table = ProviderLeaseTable::default();
        let req1 = PlaybackRequestId::from_raw(1);
        let req2 = PlaybackRequestId::from_raw(2);

        // First incarnation: confirmed then cleanly finished into an idle window.
        let first_id = table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, req1);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, req1, 15);
        table.confirm_owner("owner");
        table.finish_owner("owner", PlaybackRequestOutcome::Completed, Some(15));
        let first_generation = table.lease_of_owner("owner").expect("idle lease").binding_generation;
        let first_tag = ProviderBindingTag::new(first_id, first_generation);

        // A→A rebind on the same account starts a new incarnation (generation increments).
        let second_id = table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, req2);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, req2, 15);
        let second_generation = table.lease_of_owner("owner").expect("rebound lease").binding_generation;
        let second_tag = ProviderBindingTag::new(second_id, second_generation);
        assert_ne!(first_generation, second_generation, "A→A rebind must advance the incarnation");
        assert_eq!(first_id, second_id, "A→A rebind reuses the lease id");

        // A stale detach/clear with the old generation must not delete the successor lease.
        assert!(table.release_matching_owner("owner", &provider("A"), first_tag).is_none());
        assert!(table.lease_of_owner("owner").is_some(), "stale clear must preserve the new lease");

        // A clear with the captured generation removes exactly the current incarnation.
        assert!(table.release_matching_owner("owner", &provider("A"), second_tag).is_some());
        assert!(table.lease_of_owner("owner").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_clear_after_remove_and_recreate_preserves_successor() {
        let mut table = ProviderLeaseTable::default();
        let req1 = PlaybackRequestId::from_raw(1);
        let req2 = PlaybackRequestId::from_raw(2);

        let first_id = table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, req1);
        let first_tag = ProviderBindingTag::new(first_id, table.lease(first_id).expect("lease").binding_generation);
        table.release_owner("owner");

        let second_id = table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, req2);
        let second_tag = ProviderBindingTag::new(second_id, table.lease(second_id).expect("lease").binding_generation);

        assert_ne!(first_id, second_id, "a recreated lease must have a fresh lease id");
        assert_eq!(
            first_tag.generation, second_tag.generation,
            "generation alone is not unique across remove/recreate"
        );

        // A stale clear with the old tag must not delete the successor lease.
        assert!(table.release_matching_owner("owner", &provider("A"), first_tag).is_none());
        assert!(table.lease_of_owner("owner").is_some(), "stale clear must preserve the successor lease");

        assert!(table.release_matching_owner("owner", &provider("A"), second_tag).is_some());
        assert!(table.lease_of_owner("owner").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_lease_is_dropped_when_request_ends() {
        let mut table = ProviderLeaseTable::default();
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, PlaybackRequestId::from_raw(1));
        assert!(table.finish_owner("owner", PlaybackRequestOutcome::Completed, Some(15)).is_some());
        assert!(table.is_empty(), "a manifest-only start must not keep provider capacity");
    }

    #[tokio::test(start_paused = true)]
    async fn failed_outcome_releases_even_confirmed_lease() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, request);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, request, 15);
        table.confirm_owner("owner");
        assert!(table.finish_owner("owner", PlaybackRequestOutcome::ProviderFailed, Some(15)).is_some());
        assert!(table.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_and_expiry_release_confirmed_leases_immediately() {
        for outcome in [PlaybackRequestOutcome::TimedOut, PlaybackRequestOutcome::SessionExpired] {
            let mut table = ProviderLeaseTable::default();
            let request = PlaybackRequestId::from_raw(1);
            table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, request);
            table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, request, 15);
            table.confirm_owner("owner");

            assert!(table.finish_owner("owner", outcome, Some(15)).is_some());
            assert!(table.is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn live_ts_lease_never_becomes_idle() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveTs, request);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveTs, request, 15);
        table.confirm_owner("owner");
        assert!(table.finish_owner("owner", PlaybackRequestOutcome::Completed, Some(15)).is_some());
        assert!(table.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn renew_moves_owner_to_new_provider_without_leaking_slot() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        let id = table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, request);
        table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, request, 15);
        table.confirm_owner("owner");
        table.finish_owner("owner", PlaybackRequestOutcome::ClientClosed, Some(15));
        assert_eq!(table.usage(&provider("A")).idle, 1);

        let renewed = table.renew_identified_owner("owner", &provider("B"), PlaybackKind::LiveHls, request, 15);
        assert_eq!(renewed, None, "refresh cannot rebind an existing lease");
        let rebound = table.rebind_owner("owner", &provider("B"), PlaybackKind::LiveHls, request);
        assert_ne!(rebound, id);
        assert_eq!(table.usage(&provider("A")).total(), 0);
        // Explicit rebind starts unconfirmed on the new provider.
        assert_eq!(table.usage(&provider("B")).total(), 1);
        assert_eq!(table.usage(&provider("B")).starting, 1);
        assert_eq!(table.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_ttl_disables_reservation_without_clearing_active_identity() {
        let mut table = ProviderLeaseTable::default();
        let request = PlaybackRequestId::from_raw(1);
        table.begin_owner("owner", &provider("A"), PlaybackKind::LiveHls, request);
        table.confirm_identified_owner("owner", request);
        assert!(table.renew_identified_owner("owner", &provider("A"), PlaybackKind::LiveHls, request, 0).is_some());
        assert_eq!(table.usage(&provider("A")).active, 1);
        assert_eq!(table.foreign_reserved_slots(&provider("A"), Some("other"), &HashSet::new()), 0);
    }
}
