use super::{HlsEffectiveOriginAcquirePolicy, ProxySessionId};
use crate::api::model::ConnectionKind;
use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::OsRng, RngCore, TryRngCore};
use std::{collections::HashMap, fmt};

const HLS_ACCESS_LEASE_ID_BYTES: usize = 16;
const HLS_ACCESS_LEASE_REUSE_WINDOW_MS: u64 = 5_000;

/// Short opaque lookup key for a server-side HLS access lease.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct HlsAccessLeaseId(pub String);

impl fmt::Debug for HlsAccessLeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HlsAccessLeaseId").field(&"<redacted>").finish()
    }
}

/// Stable user/player family used only for diagnostics or future UX grouping.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct HlsPlaybackFamilyKey {
    pub username: String,
    pub client_fingerprint: String,
}

impl HlsPlaybackFamilyKey {
    pub fn new(username: impl Into<String>, client_fingerprint: impl Into<String>) -> Self {
        Self { username: username.into(), client_fingerprint: client_fingerprint.into() }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsAccessLeaseState {
    Pending,
    Activated,
    Idle,
    Expired,
    Denied,
}

impl HlsAccessLeaseState {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Activated => "Activated",
            Self::Idle => "Idle",
            Self::Expired => "Expired",
            Self::Denied => "Denied",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsAccessLeaseTiming {
    pub active_window_ms: u64,
    pub valid_window_ms: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLease {
    pub lease_id: HlsAccessLeaseId,
    pub family_key: HlsPlaybackFamilyKey,
    pub proxy_session_id: ProxySessionId,
    pub username: String,
    pub user_session_token: String,
    pub input_id: u16,
    pub stream_ref: String,
    pub virtual_id: u32,
    pub origin_connection_kind: ConnectionKind,
    pub origin_priority: i8,
    pub state: HlsAccessLeaseState,
    pub issued_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub active_until_ms: Option<u64>,
    pub valid_until_ms: u64,
}

impl HlsAccessLease {
    #[allow(clippy::too_many_arguments)]
    pub fn pending(
        lease_id: HlsAccessLeaseId,
        family_key: HlsPlaybackFamilyKey,
        proxy_session_id: ProxySessionId,
        username: String,
        user_session_token: String,
        input_id: u16,
        stream_ref: String,
        virtual_id: u32,
        now_ms: u64,
        valid_window_ms: u64,
    ) -> Self {
        Self {
            lease_id,
            family_key,
            proxy_session_id,
            username,
            user_session_token,
            input_id,
            stream_ref,
            virtual_id,
            origin_connection_kind: ConnectionKind::Normal,
            origin_priority: 0,
            state: HlsAccessLeaseState::Pending,
            issued_at_ms: now_ms,
            last_seen_at_ms: now_ms,
            active_until_ms: None,
            valid_until_ms: now_ms.saturating_add(valid_window_ms),
        }
    }

    pub const fn with_origin_acquire_policy(mut self, connection_kind: ConnectionKind, priority: i8) -> Self {
        self.origin_connection_kind = connection_kind;
        self.origin_priority = priority;
        self
    }

    pub fn update_origin_acquire_policy(&mut self, connection_kind: ConnectionKind, priority: i8) {
        self.origin_connection_kind = connection_kind;
        self.origin_priority = priority;
    }

    pub fn is_entry_reusable(&self, now_ms: u64) -> bool {
        self.state == HlsAccessLeaseState::Pending
            && now_ms.saturating_sub(self.issued_at_ms) < HLS_ACCESS_LEASE_REUSE_WINDOW_MS
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 { now_ms.saturating_sub(self.issued_at_ms) }

    fn refresh_validity(&mut self, now_ms: u64) {
        if self.valid_until_ms <= now_ms {
            self.state = HlsAccessLeaseState::Expired;
        }
    }

    fn refresh_activity(&mut self, now_ms: u64) -> Option<HlsAccessLeaseIdleRelease> {
        let was_activated = self.state == HlsAccessLeaseState::Activated;
        self.refresh_validity(now_ms);
        if was_activated && self.state == HlsAccessLeaseState::Expired {
            return Some(HlsAccessLeaseIdleRelease {
                lease_id: self.lease_id.clone(),
                username: self.username.clone(),
                user_session_token: self.user_session_token.clone(),
            });
        }
        if self.state == HlsAccessLeaseState::Activated
            && self.active_until_ms.is_some_and(|active_until| active_until <= now_ms)
        {
            self.state = HlsAccessLeaseState::Idle;
            return Some(HlsAccessLeaseIdleRelease {
                lease_id: self.lease_id.clone(),
                username: self.username.clone(),
                user_session_token: self.user_session_token.clone(),
            });
        }
        None
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum HlsAccessLeaseActivation {
    Activated { lease: Box<HlsAccessLease>, previous_state: HlsAccessLeaseState },
    Expired,
    Denied,
    UnknownLease,
    SessionMismatch,
}

impl HlsAccessLeaseActivation {
    pub const fn is_activated(&self) -> bool { matches!(self, Self::Activated { .. }) }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum HlsAccessLeaseTouch {
    Touched { lease: Box<HlsAccessLease> },
    Expired,
    Denied,
    UnknownLease,
    SessionMismatch,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLeaseIdleRelease {
    pub lease_id: HlsAccessLeaseId,
    pub username: String,
    pub user_session_token: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLeaseLifecycleSnapshot {
    pub lease_id: HlsAccessLeaseId,
    pub proxy_session_id: ProxySessionId,
    pub state: HlsAccessLeaseState,
    pub active_until_ms: Option<u64>,
    pub valid_until_ms: u64,
    pub idle_release: Option<HlsAccessLeaseIdleRelease>,
}

/// Registry for user-specific HLS access leases above shared content sessions.
#[derive(Debug, Default)]
pub struct HlsAccessLeaseStore {
    by_lease_id: HashMap<HlsAccessLeaseId, HlsAccessLease>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLeaseSessionSnapshot {
    pub active_count: usize,
    pub effective_origin_policy: Option<HlsEffectiveOriginAcquirePolicy>,
    pub idle_releases: Vec<HlsAccessLeaseIdleRelease>,
}

impl HlsAccessLeaseStore {
    pub fn prepare_access_lease(&mut self, lease: HlsAccessLease) {
        self.by_lease_id.insert(lease.lease_id.clone(), lease);
    }

    pub fn remove_access_lease(&mut self, lease_id: &HlsAccessLeaseId) -> Option<HlsAccessLease> {
        self.by_lease_id.remove(lease_id)
    }

    pub fn remove_access_leases_for_session(&mut self, proxy_session_id: &ProxySessionId) -> Vec<HlsAccessLease> {
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        lease_ids
            .into_iter()
            .filter_map(|lease_id| self.by_lease_id.remove(&lease_id))
            .collect()
    }

    pub fn clear(&mut self) -> usize {
        let removed = self.by_lease_id.len();
        self.by_lease_id.clear();
        removed
    }

    pub fn len(&self) -> usize { self.by_lease_id.len() }

    pub fn is_empty(&self) -> bool { self.by_lease_id.is_empty() }

    pub fn first_username_for_session(&self, proxy_session_id: &ProxySessionId) -> Option<String> {
        self.by_lease_id
            .values()
            .find(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.username.clone())
    }

    pub fn response_snapshot(&mut self, lease_id: &HlsAccessLeaseId, path_proxy_session_id: &ProxySessionId, now_ms: u64) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get_mut(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        lease.refresh_validity(now_ms);
        Some(lease.clone())
    }

    pub fn prune_expired_access_leases(&mut self, now_ms: u64) -> usize {
        let initial_len = self.by_lease_id.len();
        self.by_lease_id.retain(|_, lease| {
            lease.refresh_validity(now_ms);
            lease.state != HlsAccessLeaseState::Expired
        });
        initial_len.saturating_sub(self.by_lease_id.len())
    }

    pub fn find_reusable_access_lease(
        &mut self,
        family_key: &HlsPlaybackFamilyKey,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> AccessLeaseReuseResult {
        let mut blocked = AccessLeaseReuseBlock::None;
        let mut blocked_lease_id = None;
        let mut blocked_state = None;
        let mut blocked_age_ms = None;
        let reusable = self
            .by_lease_id
            .values_mut()
            .filter(|lease| lease.family_key == *family_key && lease.proxy_session_id == *proxy_session_id)
            .find_map(|lease| {
                let age_ms = lease.age_ms(now_ms);
                lease.refresh_validity(now_ms);
                if lease.state == HlsAccessLeaseState::Expired {
                    blocked = AccessLeaseReuseBlock::Expired;
                    blocked_lease_id = Some(lease.lease_id.clone());
                    blocked_state = Some(lease.state);
                    blocked_age_ms = Some(age_ms);
                    return None;
                }
                if lease.state != HlsAccessLeaseState::Pending {
                    blocked = AccessLeaseReuseBlock::StateNotPending;
                    blocked_lease_id = Some(lease.lease_id.clone());
                    blocked_state = Some(lease.state);
                    blocked_age_ms = Some(age_ms);
                    return None;
                }
                if !lease.is_entry_reusable(now_ms) {
                    blocked = AccessLeaseReuseBlock::ReuseWindowExpired;
                    blocked_lease_id = Some(lease.lease_id.clone());
                    blocked_state = Some(lease.state);
                    blocked_age_ms = Some(age_ms);
                    return None;
                }
                Some(lease.clone())
            });

        reusable.map_or(
            AccessLeaseReuseResult::NotReusable {
                lease_id: blocked_lease_id,
                reason: blocked,
                state: blocked_state,
                age_ms: blocked_age_ms,
            },
            AccessLeaseReuseResult::Reusable,
        )
    }

    pub fn access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let state = {
            let lease = self.by_lease_id.get_mut(lease_id)?;
            if &lease.proxy_session_id != path_proxy_session_id {
                return None;
            }
            lease.refresh_validity(now_ms);
            lease.state
        };
        if state == HlsAccessLeaseState::Expired {
            self.by_lease_id.remove(lease_id);
            return None;
        }
        if !lease_state_allows_use(state) {
            return None;
        }
        self.by_lease_id.get(lease_id).cloned()
    }

    pub fn update_origin_acquire_policy(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        connection_kind: ConnectionKind,
        priority: i8,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get_mut(lease_id)?;
        if !lease_state_allows_use(lease.state) {
            return None;
        }
        lease.update_origin_acquire_policy(connection_kind, priority);
        Some(lease.clone())
    }

    pub fn activate_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> HlsAccessLeaseActivation {
        let Some(new_lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        if &new_lease.proxy_session_id != path_proxy_session_id {
            return HlsAccessLeaseActivation::SessionMismatch;
        }
        if !lease_state_allows_use(new_lease.state) {
            return activation_for_state(new_lease.state);
        }
        new_lease.refresh_validity(now_ms);
        if new_lease.state == HlsAccessLeaseState::Expired {
            return HlsAccessLeaseActivation::Expired;
        }

        let previous_state = new_lease.state;
        new_lease.state = HlsAccessLeaseState::Activated;
        new_lease.last_seen_at_ms = now_ms;
        new_lease.active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
        new_lease.valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
        let lease = new_lease.clone();

        HlsAccessLeaseActivation::Activated { lease: Box::new(lease), previous_state }
    }

    pub fn touch_manifest_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        active_timing: Option<HlsAccessLeaseTiming>,
        valid_window_ms: u64,
    ) -> HlsAccessLeaseTouch {
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        if &lease.proxy_session_id != path_proxy_session_id {
            return HlsAccessLeaseTouch::SessionMismatch;
        }
        if !lease_state_allows_use(lease.state) {
            return touch_for_state(lease.state);
        }
        lease.refresh_validity(now_ms);
        if lease.state == HlsAccessLeaseState::Expired {
            return HlsAccessLeaseTouch::Expired;
        }
        lease.last_seen_at_ms = now_ms;
        if lease.state == HlsAccessLeaseState::Activated {
            if let Some(timing) = active_timing {
                lease.active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
                lease.valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
            } else {
                lease.valid_until_ms = now_ms.saturating_add(valid_window_ms);
            }
        } else {
            lease.valid_until_ms = now_ms.saturating_add(valid_window_ms);
        }
        HlsAccessLeaseTouch::Touched { lease: Box::new(lease.clone()) }
    }

    pub fn touch_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> bool {
        self.touch_access_lease_snapshot(lease_id, now_ms, timing).is_some()
    }

    pub fn touch_access_lease_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get_mut(lease_id)?;
        if lease.state != HlsAccessLeaseState::Activated {
            return None;
        }
        lease.refresh_validity(now_ms);
        if lease.state == HlsAccessLeaseState::Expired {
            return None;
        }
        lease.last_seen_at_ms = now_ms;
        lease.active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
        lease.valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
        Some(lease.clone())
    }

    pub fn deny_access_lease(&mut self, lease_id: &HlsAccessLeaseId) {
        if let Some(lease) = self.by_lease_id.get_mut(lease_id) {
            lease.state = HlsAccessLeaseState::Denied;
        }
    }

    pub fn lease_state(&self, lease_id: &HlsAccessLeaseId, now_ms: u64) -> Option<HlsAccessLeaseState> {
        self.by_lease_id.get(lease_id).map(|lease| {
            if lease.valid_until_ms <= now_ms {
                HlsAccessLeaseState::Expired
            } else {
                lease.state
            }
        })
    }

    pub fn active_access_lease_count_for_session(&mut self, proxy_session_id: &ProxySessionId, now_ms: u64) -> usize {
        let mut active_count = 0;
        for lease in self.by_lease_id.values_mut() {
            if lease.proxy_session_id == *proxy_session_id {
                lease.refresh_validity(now_ms);
                if lease.state == HlsAccessLeaseState::Activated
                    && lease.active_until_ms.is_some_and(|active_until| active_until > now_ms)
                {
                    active_count += 1;
                }
            }
        }
        active_count
    }

    pub fn has_usable_access_lease_for_session(&mut self, proxy_session_id: &ProxySessionId, now_ms: u64) -> bool {
        let mut has_usable_lease = false;
        for lease in self.by_lease_id.values_mut() {
            lease.refresh_validity(now_ms);
            if lease.proxy_session_id == *proxy_session_id
                && (lease.state == HlsAccessLeaseState::Pending
                    || (lease.state == HlsAccessLeaseState::Activated
                        && lease.active_until_ms.is_some_and(|active_until| active_until > now_ms)))
            {
                has_usable_lease = true;
            }
        }
        has_usable_lease
    }

    pub fn session_snapshot(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> HlsAccessLeaseSessionSnapshot {
        let mut active_count = 0;
        let mut effective_origin_policy = None;
        let mut idle_releases = Vec::new();
        for lease in self.by_lease_id.values_mut() {
            if lease.proxy_session_id != *proxy_session_id {
                continue;
            }
            if let Some(release) = lease.refresh_activity(now_ms) {
                idle_releases.push(release);
            }
            if lease.state == HlsAccessLeaseState::Activated {
                active_count += 1;
            }
            if matches!(lease.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated) {
                let candidate =
                    HlsEffectiveOriginAcquirePolicy::new(lease.origin_connection_kind, lease.origin_priority, now_ms);
                effective_origin_policy = Some(effective_origin_policy.map_or(candidate, |current| {
                    if candidate.is_better_than(current) {
                        candidate
                    } else {
                        current
                    }
                }));
            }
        }
        HlsAccessLeaseSessionSnapshot { active_count, effective_origin_policy, idle_releases }
    }

    pub fn lifecycle_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseLifecycleSnapshot> {
        let lease = self.by_lease_id.get_mut(lease_id)?;
        let idle_release = lease.refresh_activity(now_ms);
        Some(HlsAccessLeaseLifecycleSnapshot {
            lease_id: lease.lease_id.clone(),
            proxy_session_id: lease.proxy_session_id.clone(),
            state: lease.state,
            active_until_ms: lease.active_until_ms,
            valid_until_ms: lease.valid_until_ms,
            idle_release,
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AccessLeaseReuseResult {
    Reusable(HlsAccessLease),
    NotReusable {
        lease_id: Option<HlsAccessLeaseId>,
        reason: AccessLeaseReuseBlock,
        state: Option<HlsAccessLeaseState>,
        age_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AccessLeaseReuseBlock {
    None,
    StateNotPending,
    ReuseWindowExpired,
    Expired,
}

impl AccessLeaseReuseBlock {
    pub const fn as_log_reason(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StateNotPending => "state-not-pending",
            Self::ReuseWindowExpired => "reuse-window-expired",
            Self::Expired => "expired",
        }
    }
}

pub fn new_hls_access_lease_id() -> HlsAccessLeaseId {
    let mut bytes = [0u8; HLS_ACCESS_LEASE_ID_BYTES];
    if OsRng.try_fill_bytes(&mut bytes).is_err() {
        rand::rng().fill_bytes(&mut bytes);
    }
    HlsAccessLeaseId(general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

const fn lease_state_allows_use(state: HlsAccessLeaseState) -> bool {
    matches!(state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
}

const fn activation_for_state(state: HlsAccessLeaseState) -> HlsAccessLeaseActivation {
    match state {
        HlsAccessLeaseState::Expired => HlsAccessLeaseActivation::Expired,
        HlsAccessLeaseState::Denied => HlsAccessLeaseActivation::Denied,
        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle => {
            HlsAccessLeaseActivation::UnknownLease
        }
    }
}

const fn touch_for_state(state: HlsAccessLeaseState) -> HlsAccessLeaseTouch {
    match state {
        HlsAccessLeaseState::Expired => HlsAccessLeaseTouch::Expired,
        HlsAccessLeaseState::Denied => HlsAccessLeaseTouch::Denied,
        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle => {
            HlsAccessLeaseTouch::UnknownLease
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        new_hls_access_lease_id, AccessLeaseReuseBlock, AccessLeaseReuseResult, HlsAccessLease,
        HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeaseState, HlsAccessLeaseStore, HlsAccessLeaseTiming,
        HlsAccessLeaseTouch, HlsPlaybackFamilyKey,
    };
    use crate::api::model::{ConnectionKind, ProxySessionId};

    fn lease(lease_id: HlsAccessLeaseId, proxy_session_id: &str, now_ms: u64) -> HlsAccessLease {
        HlsAccessLease::pending(
            lease_id,
            HlsPlaybackFamilyKey::new("alice", "client-a"),
            ProxySessionId(proxy_session_id.to_string()),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            15_000,
        )
    }

    fn family_key() -> HlsPlaybackFamilyKey { HlsPlaybackFamilyKey::new("alice", "client-a") }

    const fn timing(active_window_ms: u64, valid_window_ms: u64) -> HlsAccessLeaseTiming {
        HlsAccessLeaseTiming { active_window_ms, valid_window_ms }
    }

    #[test]
    fn access_lease_id_is_short_and_opaque() {
        let lease_id = new_hls_access_lease_id();

        assert_eq!(lease_id.0.len(), 22);
        assert!(!lease_id.0.contains("alice"));
        assert!(!lease_id.0.contains("session"));
    }

    #[test]
    fn access_lease_activates_and_slides_validity() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store
            .activate_access_lease(&lease_id, &proxy_session_id, 10_000, timing(5_000, 30_000))
            .is_activated());
        assert_eq!(store.lease_state(&lease_id, 24_999), Some(HlsAccessLeaseState::Activated));
        assert!(store.touch_access_lease(&lease_id, 24_000, timing(5_000, 30_000)));
        assert_eq!(store.lease_state(&lease_id, 53_999), Some(HlsAccessLeaseState::Activated));
    }

    #[test]
    fn access_lease_idles_at_exact_active_until_boundary() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());

        let snapshot = store.lifecycle_snapshot(&lease_id, 7_000).expect("lease should exist");

        assert_eq!(snapshot.state, HlsAccessLeaseState::Idle);
        assert!(snapshot.idle_release.is_some());
        assert_eq!(store.active_access_lease_count_for_session(&proxy_session_id, 7_000), 0);
    }

    #[test]
    fn access_lease_expires_at_exact_valid_until_boundary() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

        let snapshot = store.lifecycle_snapshot(&lease_id, 17_000).expect("lease should exist");

        assert_eq!(snapshot.state, HlsAccessLeaseState::Expired);
        assert!(snapshot.idle_release.is_some());
        assert_eq!(store.lease_state(&lease_id, 17_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn access_lease_expires_without_activity() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert_eq!(
            store.activate_access_lease(&lease_id, &proxy_session_id, 17_000, timing(5_000, 15_000)),
            HlsAccessLeaseActivation::Expired
        );
    }

    #[test]
    fn same_family_leases_remain_independently_valid() {
        let mut store = HlsAccessLeaseStore::default();
        let old_lease_id = HlsAccessLeaseId("old".to_string());
        let new_lease_id = HlsAccessLeaseId("new".to_string());
        let proxy_a = ProxySessionId("proxy-a".to_string());
        let proxy_b = ProxySessionId("proxy-b".to_string());
        let family = HlsPlaybackFamilyKey::new("alice", "client-a");

        store.prepare_access_lease(HlsAccessLease::pending(
            old_lease_id.clone(),
            family.clone(),
            proxy_a.clone(),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "12345".to_string(),
            12345,
            1_000,
            15_000,
        ));
        assert!(store.activate_access_lease(&old_lease_id, &proxy_a, 2_000, timing(5_000, 15_000)).is_activated());
        store.prepare_access_lease(HlsAccessLease::pending(
            new_lease_id.clone(),
            family,
            proxy_b.clone(),
            "alice".to_string(),
            "session-b".to_string(),
            1,
            "67890".to_string(),
            67890,
            3_000,
            15_000,
        ));

        let activation = store.activate_access_lease(&new_lease_id, &proxy_b, 4_000, timing(5_000, 15_000));
        assert!(activation.is_activated());
        assert_eq!(store.lease_state(&old_lease_id, 4_000), Some(HlsAccessLeaseState::Activated));
        assert!(store.touch_access_lease(&old_lease_id, 5_000, timing(5_000, 15_000)));
        assert_eq!(store.lease_state(&old_lease_id, 19_999), Some(HlsAccessLeaseState::Activated));
    }

    #[test]
    fn exact_user_session_proxy_session_reuses_pending_lease_within_start_window() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        let AccessLeaseReuseResult::Reusable(reused) =
            store.find_reusable_access_lease(&family_key(), &proxy_session_id, 4_999)
        else {
            panic!("existing lease should be reused");
        };

        assert_eq!(reused.lease_id, lease_id);
        assert_eq!(reused.valid_until_ms, 16_000);
        assert!(matches!(
            store.find_reusable_access_lease(&HlsPlaybackFamilyKey::new("alice", "client-b"), &proxy_session_id, 4_999),
            AccessLeaseReuseResult::NotReusable { reason: AccessLeaseReuseBlock::None, .. }
        ));
    }

    #[test]
    fn manifest_touch_keeps_pending_lease_entry_reusable() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(matches!(
            store.touch_manifest_access_lease(&lease_id, &proxy_session_id, 2_000, None, 15_000),
            super::HlsAccessLeaseTouch::Touched { .. }
        ));
        assert_eq!(store.lease_state(&lease_id, 2_000), Some(HlsAccessLeaseState::Pending));

        let AccessLeaseReuseResult::Reusable(reused) =
            store.find_reusable_access_lease(&family_key(), &proxy_session_id, 4_999)
        else {
            panic!("manifest-touched lease should remain reusable");
        };

        assert_eq!(reused.lease_id, lease_id);
        assert_eq!(reused.state, HlsAccessLeaseState::Pending);
        assert_eq!(reused.last_seen_at_ms, 2_000);
    }

    #[test]
    fn manifest_touch_extends_activated_lease_active_window() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                6_000,
                Some(timing(10_000, 30_000)),
                15_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));
        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.state, HlsAccessLeaseState::Activated);
        assert_eq!(lease.last_seen_at_ms, 6_000);
        assert_eq!(lease.active_until_ms, Some(16_000));
        assert_eq!(lease.valid_until_ms, 36_000);
    }

    #[test]
    fn resource_activation_moves_pending_lease_out_of_entry_reuse() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());
        assert_eq!(store.lease_state(&lease_id, 2_000), Some(HlsAccessLeaseState::Activated));

        assert!(matches!(
            store.find_reusable_access_lease(&family_key(), &proxy_session_id, 3_000),
            AccessLeaseReuseResult::NotReusable {
                reason: AccessLeaseReuseBlock::StateNotPending,
                state: Some(HlsAccessLeaseState::Activated),
                age_ms: Some(2_000),
                ..
            }
        ));
        assert!(store.touch_access_lease(&lease_id, 3_000, timing(5_000, 15_000)));
    }

    #[test]
    fn activated_lease_is_not_entry_reusable_but_remains_valid() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

        assert!(matches!(
            store.find_reusable_access_lease(&family_key(), &proxy_session_id, 3_000),
            AccessLeaseReuseResult::NotReusable { reason: AccessLeaseReuseBlock::StateNotPending, .. }
        ));
        assert!(store.touch_access_lease(&lease_id, 3_000, timing(5_000, 15_000)));
        assert_eq!(store.lease_state(&lease_id, 17_999), Some(HlsAccessLeaseState::Activated));
    }

    #[test]
    fn activated_lease_becomes_idle_after_active_window_but_remains_reactivatable() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());

        let snapshot = store.session_snapshot(&proxy_session_id, 8_000);
        assert_eq!(snapshot.active_count, 0);
        assert_eq!(snapshot.idle_releases.len(), 1);
        assert_eq!(snapshot.idle_releases[0].lease_id, lease_id);
        assert_eq!(store.lease_state(&lease_id, 8_000), Some(HlsAccessLeaseState::Idle));
        assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 8_000));
        assert!(store.access_lease(&lease_id, &proxy_session_id, 8_000).is_some());

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 8_000, timing(5_000, 30_000)).is_activated());
        assert_eq!(store.session_snapshot(&proxy_session_id, 8_000).active_count, 1);
    }

    #[test]
    fn activated_lease_validity_expiry_reports_idle_release() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(30_000, 5_000)).is_activated());

        let snapshot = store.session_snapshot(&proxy_session_id, 8_000);
        assert_eq!(snapshot.active_count, 0);
        assert_eq!(snapshot.idle_releases.len(), 1);
        assert_eq!(snapshot.idle_releases[0].lease_id, lease_id);
        assert_eq!(store.lease_state(&lease_id, 8_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn usable_access_lease_query_accepts_pending_and_activated_only() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let pending_id = HlsAccessLeaseId("pending".to_string());
        let activated_id = HlsAccessLeaseId("activated".to_string());
        let denied_id = HlsAccessLeaseId("denied".to_string());
        let expired_id = HlsAccessLeaseId("expired".to_string());

        store.prepare_access_lease(lease(pending_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(activated_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(denied_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(expired_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store
            .activate_access_lease(&activated_id, &proxy_session_id, 2_000, timing(5_000, 15_000))
            .is_activated());
        store.deny_access_lease(&denied_id);

        assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 2_000));

        store.deny_access_lease(&pending_id);
        store.deny_access_lease(&activated_id);
        store.deny_access_lease(&expired_id);
        assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 2_000));
        assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 17_000));
        assert_eq!(store.lease_state(&expired_id, 17_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn session_snapshot_prefers_normal_origin_policy_over_soft() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("soft".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Soft, -20),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("normal".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, 50),
        );

        let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
        let policy = snapshot.effective_origin_policy.expect("usable lease policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(policy.priority, 50);
    }

    #[test]
    fn origin_policy_update_reclassifies_existing_access_lease() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        store.prepare_access_lease(
            lease(lease_id.clone(), &proxy_session_id.0, 1_000).with_origin_acquire_policy(ConnectionKind::Soft, 20),
        );

        let updated =
            store.update_origin_acquire_policy(&lease_id, ConnectionKind::Normal, -5).expect("lease should update");
        assert_eq!(updated.origin_connection_kind, ConnectionKind::Normal);
        assert_eq!(updated.origin_priority, -5);

        let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
        let policy = snapshot.effective_origin_policy.expect("updated policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(policy.priority, -5);
    }

    #[test]
    fn session_snapshot_uses_best_priority_within_same_origin_policy_kind() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("low-priority".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, 30),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("high-priority".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, -5),
        );

        let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
        let policy = snapshot.effective_origin_policy.expect("usable lease policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(policy.priority, -5);
    }

    #[test]
    fn session_snapshot_ignores_expired_and_denied_origin_policies() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let denied_id = HlsAccessLeaseId("denied".to_string());
        store.prepare_access_lease(
            lease(denied_id.clone(), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, -100),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("expired".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, -50),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("active-soft".to_string()), &proxy_session_id.0, 10_000)
                .with_origin_acquire_policy(ConnectionKind::Soft, 10),
        );
        store.deny_access_lease(&denied_id);

        let snapshot = store.session_snapshot(&proxy_session_id, 17_000);
        let policy = snapshot.effective_origin_policy.expect("usable lease policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(policy.priority, 10);
    }

    #[test]
    fn pending_lease_after_reuse_window_is_not_entry_reusable() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(matches!(
            store.find_reusable_access_lease(&family_key(), &proxy_session_id, 6_000),
            AccessLeaseReuseResult::NotReusable { reason: AccessLeaseReuseBlock::ReuseWindowExpired, .. }
        ));
        assert_eq!(store.lease_state(&lease_id, 6_000), Some(HlsAccessLeaseState::Pending));
    }

    #[test]
    fn expired_lease_is_not_reusable_but_kept_for_manager_cleanup() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(matches!(
            store.find_reusable_access_lease(&family_key(), &proxy_session_id, 17_000),
            AccessLeaseReuseResult::NotReusable { reason: AccessLeaseReuseBlock::Expired, .. }
        ));
        assert_eq!(store.lease_state(&lease_id, 17_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn expired_lease_lookup_removes_stale_entry() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.access_lease(&lease_id, &proxy_session_id, 17_000).is_none());
        assert_eq!(store.lease_state(&lease_id, 17_000), None);
    }
}
