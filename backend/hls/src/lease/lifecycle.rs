use super::{
    HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeaseIdleRelease,
    HlsAccessLeaseLifecycleSnapshot, HlsAccessLeasePendingDeadline, HlsAccessLeaseRemovalPreparation,
    HlsAccessLeaseSessionSnapshot, HlsAccessLeaseState, HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsAccessLeaseTouch,
    HlsAvailabilityEvidenceAdvanceError, HlsAvailabilityEvidenceGeneration, HlsEffectiveOriginAcquirePolicy,
    HlsLeaseManifestPublicationOutcome, HlsLeasePlaybackMode, HlsManifestDeliveryMode, HlsMediaLeaseIdentity,
    HlsMediaLeasePlaybackIdentity, HlsPlaybackCompletionOutcome, HlsPlaybackFamilyKey, HlsPlaybackRequestToken,
    HlsTerminalTailGeneration, ProxySessionId, TransientManifestLeaseBinding,
};
use tuliprox_session::ConnectionKind;

impl HlsAvailabilityEvidenceGeneration {
    pub(super) const NONE: Self = Self(0);

    #[cfg(any(test, feature = "test-support"))]
    pub const fn for_test(generation: u64) -> Self { Self(generation) }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn as_u64(self) -> u64 { self.0 }
}

impl HlsPlaybackFamilyKey {
    pub fn new(username: impl Into<String>, client_fingerprint: impl Into<String>) -> Self {
        Self { username: username.into(), client_fingerprint: client_fingerprint.into() }
    }
}

impl HlsAccessLeaseState {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Activated => "Activated",
            Self::Idle => "Idle",
            Self::PolicyRevoking => "PolicyRevoking",
            Self::Expired => "Expired",
            Self::Denied => "Denied",
        }
    }
}

impl HlsAccessLeasePendingDeadline {
    pub(super) const fn tightened_with(self, candidate: Self) -> Self {
        let deadline_ms =
            if self.deadline_ms() <= candidate.deadline_ms() { self.deadline_ms() } else { candidate.deadline_ms() };
        match (self, candidate) {
            (Self::FollowUp { .. }, _) | (_, Self::FollowUp { .. }) => Self::FollowUp { deadline_ms },
            (Self::Bootstrap { .. }, Self::Bootstrap { .. }) => Self::Bootstrap { deadline_ms },
        }
    }
}

impl HlsLeaseManifestPublicationOutcome {
    pub const fn is_committed(self) -> bool { matches!(self, Self::Committed { .. }) }

    pub const fn snapshot_generation(self) -> Option<u64> {
        match self {
            Self::Committed { snapshot_generation } => Some(snapshot_generation),
            Self::Rejected(_) => None,
        }
    }
}

impl HlsAccessLease {
    pub fn with_archive_playback(mut self, epg_reference_ts: Option<i64>, archive_origin_url: Option<String>) -> Self {
        self.epg_reference_ts = epg_reference_ts;
        self.archive_origin_url = archive_origin_url;
        self
    }

    pub const fn with_origin_acquire_policy(mut self, connection_kind: ConnectionKind, priority: i8) -> Self {
        self.origin_connection_kind = connection_kind;
        self.origin_priority = priority;
        self
    }

    pub const fn with_known_bitrate_bps(mut self, known_bitrate_bps: Option<u32>) -> Self {
        self.known_bitrate_bps = match known_bitrate_bps {
            Some(0) | None => None,
            Some(bitrate_bps) => Some(bitrate_bps),
        };
        self
    }

    pub fn update_origin_acquire_policy(&mut self, connection_kind: ConnectionKind, priority: i8) {
        self.origin_connection_kind = connection_kind;
        self.origin_priority = priority;
    }

    pub(super) fn end_playback(&mut self) {
        self.runtime_policy_revocation = None;
        let release_generation = match std::mem::replace(&mut self.playback_mode, HlsLeasePlaybackMode::Ended) {
            HlsLeasePlaybackMode::TerminalTail(plan) => Some(plan.generation),
            HlsLeasePlaybackMode::TerminalUnavailable { decision_generation, .. } => {
                Some(HlsTerminalTailGeneration(decision_generation))
            }
            HlsLeasePlaybackMode::Live | HlsLeasePlaybackMode::Ended => None,
        };
        if self.pending_terminal_protection_release.is_none() {
            self.pending_terminal_protection_release = release_generation;
        }
    }
}

impl HlsAccessLeaseActivation {
    pub const fn is_activated(&self) -> bool { matches!(self, Self::Activated { .. }) }
}

impl HlsAccessLeaseStore {
    pub fn repair_prewarm_is_current(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        issued_at_ms: u64,
        snapshot_generation: u64,
    ) -> bool {
        self.by_lease_id.get(lease_id).is_some_and(|lease| {
            lease.proxy_session_id == *proxy_session_id
                && lease.issued_at_ms == issued_at_ms
                && lease.playback_mode == HlsLeasePlaybackMode::Live
                && lease
                    .last_manifest_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.snapshot_generation == snapshot_generation)
        })
    }

    fn refresh_access_lease_activity(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Result<Option<HlsAccessLeaseIdleRelease>, HlsAvailabilityEvidenceAdvanceError> {
        let previous_state = match self.by_lease_id.get(lease_id) {
            Some(lease) => lease.state,
            None => return Ok(None),
        };
        self.refresh_access_lease_validity(lease_id, now_ms)?;
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return Ok(None);
        };
        if lease.state == HlsAccessLeaseState::Expired
            && matches!(previous_state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated)
        {
            return Ok(Some(HlsAccessLeaseIdleRelease {
                lease_id: lease.lease_id.clone(),
                username: lease.username.clone(),
                user_session_token: lease.user_session_token.clone(),
            }));
        }
        if lease.state != HlsAccessLeaseState::Activated
            || lease.active_until_ms.is_none_or(|active_until| active_until > now_ms)
        {
            return Ok(None);
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let release = HlsAccessLeaseIdleRelease {
            lease_id: lease.lease_id.clone(),
            username: lease.username.clone(),
            user_session_token: lease.user_session_token.clone(),
        };
        self.availability_evidence_can_advance()?;
        if let Some(lease) = self.by_lease_id.get_mut(lease_id) {
            lease.state = HlsAccessLeaseState::Idle;
        }
        self.advance_availability_evidence(&proxy_session_id)?;
        Ok(Some(release))
    }

    /// Returns false only when the process-lifetime evidence generation is
    /// exhausted. In that case no lease replacement is published.
    pub fn prepare_access_lease(&mut self, lease: HlsAccessLease) -> bool {
        if self.by_lease_id.get(&lease.lease_id) == Some(&lease) {
            return true;
        }
        let new_proxy_session_id = lease.proxy_session_id.clone();
        let replaced_proxy_session_id = self
            .by_lease_id
            .get(&lease.lease_id)
            .map(|current| current.proxy_session_id.clone())
            .filter(|current| *current != new_proxy_session_id);
        let advances = 1_u64.saturating_add(u64::from(replaced_proxy_session_id.is_some()));
        if self.last_availability_evidence_generation.0.checked_add(advances).is_none() {
            return false;
        }
        self.by_lease_id.insert(lease.lease_id.clone(), lease);
        if self.advance_availability_evidence(&new_proxy_session_id).is_err() {
            return false;
        }
        if let Some(old_proxy_session_id) = replaced_proxy_session_id {
            if self.advance_availability_evidence(&old_proxy_session_id).is_err() {
                return false;
            }
        }
        true
    }

    pub fn remove_access_lease(&mut self, lease_id: &HlsAccessLeaseId) -> Option<HlsAccessLease> {
        let proxy_session_id = self.by_lease_id.get(lease_id)?.proxy_session_id.clone();
        if self.availability_evidence_can_advance().is_err() {
            return None;
        }
        self.advance_availability_evidence(&proxy_session_id).ok()?;
        let removed = self.by_lease_id.remove(lease_id)?;
        Some(removed)
    }

    /// Snapshots cleanup state without deleting the persistent cancellation-recovery ticket.
    pub fn prepare_access_lease_removal(
        &self,
        lease_id: &HlsAccessLeaseId,
    ) -> Option<HlsAccessLeaseRemovalPreparation> {
        let lease = self.by_lease_id.get(lease_id)?;
        Some(HlsAccessLeaseRemovalPreparation {
            proxy_session_id: lease.proxy_session_id.clone(),
            issued_at_ms: lease.issued_at_ms,
            terminal_protection_generation: lease.pending_terminal_protection_release.or_else(|| {
                if let HlsLeasePlaybackMode::TerminalTail(plan) = &lease.playback_mode {
                    Some(plan.generation)
                } else {
                    None
                }
            }),
        })
    }

    /// Deletes only the lease instance observed before its protection cleanup.
    pub fn remove_access_lease_if_preparation_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        preparation: &HlsAccessLeaseRemovalPreparation,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != preparation.proxy_session_id || lease.issued_at_ms != preparation.issued_at_ms {
            return None;
        }
        self.remove_access_lease(lease_id)
    }

    pub fn remove_access_leases_for_session(&mut self, proxy_session_id: &ProxySessionId) -> Vec<HlsAccessLease> {
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        if lease_ids.is_empty() || self.availability_evidence_can_advance().is_err() {
            return Vec::new();
        }
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return Vec::new();
        }
        lease_ids.into_iter().filter_map(|lease_id| self.by_lease_id.remove(&lease_id)).collect()
    }

    pub fn clear(&mut self) -> usize {
        let removed = self.by_lease_id.len();
        self.by_lease_id.clear();
        self.availability_generation_by_proxy_session.clear();
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

    /// Returns the oldest sequence which every usable live lease has consumed.
    ///
    /// `None` deliberately keeps the canonical render protected when a lease
    /// has not published or completed media yet. Terminal leases are excluded
    /// here because their exact base media is protected by the terminal-tail
    /// generation stored on the shared session.
    pub fn capacity_release_through(&self, proxy_session_id: &ProxySessionId) -> Option<u64> {
        let mut found_live_lease = false;
        let mut release_through = None;
        for lease in self.by_lease_id.values().filter(|lease| {
            lease.proxy_session_id == *proxy_session_id
                && lease_state_protects_live_evidence(lease.state)
                && lease.playback_mode == HlsLeasePlaybackMode::Live
        }) {
            found_live_lease = true;
            let snapshot = lease.last_manifest_snapshot.as_ref()?;
            if snapshot.delivery_mode != HlsManifestDeliveryMode::NormalCacheTimeline {
                return None;
            }
            let completed = lease.playback_cursor.highest_contiguous_completed_proxy_seq?;
            let completed = completed.min(snapshot.last_proxy_seq);
            release_through = Some(release_through.map_or(completed, |current: u64| current.min(completed)));
        }
        if found_live_lease {
            release_through
        } else {
            None
        }
    }

    pub fn response_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return None;
        }
        self.by_lease_id.get(lease_id).cloned()
    }

    /// Returns immutable playback evidence for leases that are actively consuming this shared session.
    pub fn active_live_playback_snapshots_for_session(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Vec<HlsAccessLease> {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return Vec::new();
        }
        self.by_lease_id
            .values()
            .filter_map(|lease| {
                if lease.proxy_session_id != *proxy_session_id {
                    return None;
                }
                (lease.state == HlsAccessLeaseState::Activated
                    && lease.active_until_ms.is_some_and(|active_until_ms| active_until_ms > now_ms)
                    && lease.playback_mode == HlsLeasePlaybackMode::Live
                    && lease.last_manifest_snapshot.is_some())
                .then(|| lease.clone())
            })
            .collect()
    }

    pub fn record_segment_request_started_if_identity_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsMediaLeaseIdentity,
        proxy_seq: u64,
        requested_at_ms: u64,
    ) -> Option<HlsPlaybackRequestToken> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, requested_at_ms).is_err() {
            return None;
        }
        let lease = self.by_lease_id.get(lease_id)?;
        if !lease_state_allows_use(lease.state)
            || lease.issued_at_ms != expected.issued_at_ms
            || lease.media_identity() != Some(expected)
            || !matches!(expected.playback, HlsMediaLeasePlaybackIdentity::Live { .. })
            || lease.playback_cursor.cursor_generation == u64::MAX
            || self.availability_evidence_can_advance().is_err()
        {
            return None;
        }
        let token =
            self.by_lease_id.get_mut(lease_id)?.playback_cursor.record_request_started(proxy_seq, requested_at_ms);
        self.advance_availability_evidence(proxy_session_id).ok()?;
        Some(token)
    }

    pub fn record_segment_request_completed_if_identity_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsMediaLeaseIdentity,
        token: HlsPlaybackRequestToken,
        completed_at_ms: u64,
    ) -> Option<HlsPlaybackCompletionOutcome> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, completed_at_ms).is_err() {
            return None;
        }
        let lease = self.by_lease_id.get(lease_id)?;
        if !lease_state_allows_use(lease.state)
            || lease.issued_at_ms != expected.issued_at_ms
            || lease.media_identity() != Some(expected)
            || !matches!(expected.playback, HlsMediaLeasePlaybackIdentity::Live { .. })
        {
            return None;
        }
        let mut cursor = lease.playback_cursor.clone();
        let outcome = cursor.record_request_completed(token, completed_at_ms);
        if cursor.cursor_generation == lease.playback_cursor.cursor_generation {
            return Some(outcome);
        }
        if self.availability_evidence_can_advance().is_err() {
            return None;
        }
        self.by_lease_id.get_mut(lease_id)?.playback_cursor = cursor;
        self.advance_availability_evidence(proxy_session_id).ok()?;
        Some(outcome)
    }

    pub fn access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let state = self.by_lease_id.get(lease_id)?.state;
        if state == HlsAccessLeaseState::Expired {
            return None;
        }
        if matches!(state, HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Denied) {
            return self.by_lease_id.get(lease_id).cloned();
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
        let Some(new_lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        if &new_lease.proxy_session_id != path_proxy_session_id {
            return HlsAccessLeaseActivation::SessionMismatch;
        }
        if !lease_state_allows_use(new_lease.state) {
            return activation_for_state(new_lease.state);
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsAccessLeaseActivation::Expired;
        }
        let Some(new_lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        if new_lease.state == HlsAccessLeaseState::Expired {
            return HlsAccessLeaseActivation::Expired;
        }
        let previous_state = new_lease.state;
        let active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
        let valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
        let evidence_changed = new_lease.state != HlsAccessLeaseState::Activated
            || new_lease.active_until_ms != active_until_ms
            || new_lease.pending_deadline.is_some()
            || new_lease.valid_until_ms != valid_until_ms;
        if evidence_changed && self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseActivation::Expired;
        }
        let proxy_session_id = new_lease.proxy_session_id.clone();
        let Some(new_lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        new_lease.state = HlsAccessLeaseState::Activated;
        new_lease.last_seen_at_ms = now_ms;
        new_lease.active_until_ms = active_until_ms;
        new_lease.pending_deadline = None;
        new_lease.valid_until_ms = valid_until_ms;
        let lease = new_lease.clone();
        if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
            return HlsAccessLeaseActivation::Expired;
        }

        HlsAccessLeaseActivation::Activated { lease: Box::new(lease), previous_state }
    }

    pub fn touch_manifest_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        active_timing: Option<HlsAccessLeaseTiming>,
        pending_deadline: Option<HlsAccessLeasePendingDeadline>,
        valid_window_ms: u64,
    ) -> HlsAccessLeaseTouch {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        if &lease.proxy_session_id != path_proxy_session_id {
            return HlsAccessLeaseTouch::SessionMismatch;
        }
        if !lease_state_allows_use(lease.state) {
            return touch_for_state(lease.state);
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsAccessLeaseTouch::Expired;
        }
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        if lease.state == HlsAccessLeaseState::Expired {
            return HlsAccessLeaseTouch::Expired;
        }
        let before = (lease.active_until_ms, lease.pending_deadline, lease.valid_until_ms);
        let proxy_session_id = lease.proxy_session_id.clone();
        if self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseTouch::Expired;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        lease.last_seen_at_ms = now_ms;
        match lease.state {
            HlsAccessLeaseState::Pending => {
                if let Some(pending_deadline) = pending_deadline {
                    lease.apply_pending_deadline(pending_deadline);
                }
            }
            HlsAccessLeaseState::Activated => {
                if let Some(timing) = active_timing {
                    lease.active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
                    lease.valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
                } else {
                    lease.valid_until_ms = now_ms.saturating_add(valid_window_ms);
                }
            }
            HlsAccessLeaseState::Idle => {
                lease.valid_until_ms = now_ms.saturating_add(valid_window_ms);
            }
            HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied => {}
        }
        let evidence_changed = before != (lease.active_until_ms, lease.pending_deadline, lease.valid_until_ms);
        let lease = lease.clone();
        if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
            return HlsAccessLeaseTouch::Expired;
        }
        HlsAccessLeaseTouch::Touched { lease: Box::new(lease) }
    }

    pub fn mark_pending_manifest_follow_up_for_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        deadline: HlsAccessLeasePendingDeadline,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.state != HlsAccessLeaseState::Pending {
            return None;
        }
        let next_deadline = lease.pending_deadline.map_or(deadline, |current| current.tightened_with(deadline));
        if lease.pending_deadline == Some(next_deadline) {
            return None;
        }
        if self.availability_evidence_can_advance().is_err() {
            return None;
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let lease = self.by_lease_id.get_mut(lease_id)?;
        lease.last_seen_at_ms = now_ms;
        if !lease.apply_pending_deadline(deadline) {
            return None;
        }
        let lease = lease.clone();
        self.advance_availability_evidence(&proxy_session_id).ok()?;
        Some(lease)
    }

    pub fn mark_pending_manifest_follow_up_for_session(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        deadline: HlsAccessLeasePendingDeadline,
    ) -> Vec<HlsAccessLease> {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return Vec::new();
        }
        if self.availability_evidence_can_advance().is_err() {
            return Vec::new();
        }
        let mut leases = Vec::new();
        for lease in self.by_lease_id.values_mut() {
            if lease.proxy_session_id != *proxy_session_id {
                continue;
            }
            if lease.state != HlsAccessLeaseState::Pending {
                continue;
            }
            lease.last_seen_at_ms = now_ms;
            if lease.apply_pending_deadline(deadline) {
                leases.push(lease.clone());
            }
        }
        if !leases.is_empty() && self.advance_availability_evidence(proxy_session_id).is_err() {
            return Vec::new();
        }
        leases
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
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.state != HlsAccessLeaseState::Activated {
            return None;
        }
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.state == HlsAccessLeaseState::Expired {
            return None;
        }
        let active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
        let valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
        let evidence_changed = lease.active_until_ms != active_until_ms || lease.valid_until_ms != valid_until_ms;
        if evidence_changed && self.availability_evidence_can_advance().is_err() {
            return None;
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let lease = self.by_lease_id.get_mut(lease_id)?;
        lease.last_seen_at_ms = now_ms;
        lease.active_until_ms = active_until_ms;
        lease.valid_until_ms = valid_until_ms;
        let lease = lease.clone();
        if evidence_changed {
            self.advance_availability_evidence(&proxy_session_id).ok()?;
        }
        Some(lease)
    }

    pub fn lease_state(&self, lease_id: &HlsAccessLeaseId, now_ms: u64) -> Option<HlsAccessLeaseState> {
        self.by_lease_id.get(lease_id).map(|lease| {
            if lease.validity_due_at_ms() <= now_ms {
                HlsAccessLeaseState::Expired
            } else {
                lease.state
            }
        })
    }

    pub fn active_access_lease_count_for_session(&mut self, proxy_session_id: &ProxySessionId, now_ms: u64) -> usize {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return 0;
        }
        let mut active_count = 0;
        for lease in self.by_lease_id.values() {
            if lease.proxy_session_id == *proxy_session_id
                && lease.state == HlsAccessLeaseState::Activated
                && lease.active_until_ms.is_some_and(|active_until| active_until > now_ms)
            {
                active_count += 1;
            }
        }
        active_count
    }

    pub(crate) fn select_startup_repair_lease(
        &mut self,
        session: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseId> {
        self.refresh_access_lease_validities_for_session(session, now_ms).ok()?;
        self.by_lease_id
            .values()
            .filter(|lease| {
                lease.proxy_session_id == *session
                    && matches!(lease.playback_mode, HlsLeasePlaybackMode::Live)
                    && matches!(
                        lease.state,
                        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Idle | HlsAccessLeaseState::Activated
                    )
            })
            .min_by(|left, right| {
                left.issued_at_ms.cmp(&right.issued_at_ms).then_with(|| left.lease_id.0.cmp(&right.lease_id.0))
            })
            .map(|lease| lease.lease_id.clone())
    }

    pub fn has_usable_access_lease_for_session(&mut self, proxy_session_id: &ProxySessionId, now_ms: u64) -> bool {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return false;
        }
        let mut has_usable_lease = false;
        for lease in self.by_lease_id.values() {
            if lease.proxy_session_id == *proxy_session_id
                && (lease.state == HlsAccessLeaseState::Pending
                    || lease.state == HlsAccessLeaseState::Idle
                    || (lease.state == HlsAccessLeaseState::Activated && lease.valid_until_ms > now_ms))
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
        let mut finalized_transient_manifest_bindings = Vec::new();
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in lease_ids {
            match self.refresh_access_lease_activity(&lease_id, now_ms) {
                Ok(Some(release)) => idle_releases.push(release),
                Ok(None) => {}
                Err(HlsAvailabilityEvidenceAdvanceError::Exhausted) => continue,
            }
            let Some(lease) = self.by_lease_id.get(&lease_id) else {
                continue;
            };
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
            if lease_state_allows_use(lease.state) && lease.playback_mode == HlsLeasePlaybackMode::Live {
                finalized_transient_manifest_bindings.extend(
                    lease.published_finalized_manifest_generations.iter().map(|manifest_generation| {
                        TransientManifestLeaseBinding::new(
                            lease.lease_id.clone(),
                            lease.issued_at_ms,
                            *manifest_generation,
                        )
                    }),
                );
            }
        }
        HlsAccessLeaseSessionSnapshot {
            active_count,
            effective_origin_policy,
            idle_releases,
            finalized_transient_manifest_bindings,
        }
    }

    pub fn lifecycle_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseLifecycleSnapshot> {
        let idle_release = self.refresh_access_lease_activity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get(lease_id)?;
        Some(HlsAccessLeaseLifecycleSnapshot {
            lease_id: lease.lease_id.clone(),
            proxy_session_id: lease.proxy_session_id.clone(),
            state: lease.state,
            active_until_ms: lease.active_until_ms,
            pending_deadline: lease.pending_deadline,
            valid_until_ms: lease.valid_until_ms,
            idle_release,
        })
    }
}

pub(super) const fn lease_state_allows_use(state: HlsAccessLeaseState) -> bool {
    matches!(state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
}

const fn lease_state_protects_live_evidence(state: HlsAccessLeaseState) -> bool {
    lease_state_allows_use(state) || matches!(state, HlsAccessLeaseState::PolicyRevoking)
}

const fn activation_for_state(state: HlsAccessLeaseState) -> HlsAccessLeaseActivation {
    match state {
        HlsAccessLeaseState::Expired => HlsAccessLeaseActivation::Expired,
        HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Denied => HlsAccessLeaseActivation::Denied,
        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle => {
            HlsAccessLeaseActivation::UnknownLease
        }
    }
}

const fn touch_for_state(state: HlsAccessLeaseState) -> HlsAccessLeaseTouch {
    match state {
        HlsAccessLeaseState::Expired => HlsAccessLeaseTouch::Expired,
        HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Denied => HlsAccessLeaseTouch::Denied,
        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle => {
            HlsAccessLeaseTouch::UnknownLease
        }
    }
}
