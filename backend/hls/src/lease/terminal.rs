use super::{
    lease_state_allows_use, runtime_policy_authorized_manifest_prefix, HlsAccessLease, HlsAccessLeaseId,
    HlsAccessLeaseState, HlsAccessLeaseStore, HlsFiniteTailTrigger, HlsLeasePlaybackMode,
    HlsLeaseStartupAdmissionState, HlsRuntimeCustomTailBasePolicy, HlsRuntimeCustomTailReason,
    HlsTerminalCommitOutcome, HlsTerminalMediaPreparationKey, HlsTerminalMediaPreparationState,
    HlsTerminalMediaRequirementOrigin, HlsTerminalMediaRequirementSource, HlsTerminalTailCompatibility,
    HlsTerminalTailGeneration, HlsTerminalTailPlan, HlsTerminalTailPreparation, HlsTerminalTailPreparationInput,
    ProxySessionId,
};

impl HlsAccessLease {
    pub fn permits_unpublished_standalone_tail(&self, reason: HlsRuntimeCustomTailReason) -> bool {
        reason.permits_unpublished_lease_standalone_tail()
            && self.state == HlsAccessLeaseState::Pending
            && self.playback_mode == HlsLeasePlaybackMode::Live
            && self.startup_admission == HlsLeaseStartupAdmissionState::Pending
            && self.last_manifest_snapshot.is_none()
            && self.runtime_policy_revocation.is_none()
    }

    fn terminal_commit_preconditions_match(
        &mut self,
        proxy_session_id: &ProxySessionId,
        expected_issued_at_ms: u64,
        expected_admission_generation: u64,
        manifest_snapshot_generation: u64,
        cursor_generation: u64,
        _now_ms: u64,
    ) -> Result<(), HlsTerminalCommitOutcome> {
        if !lease_state_allows_use(self.state)
            || self.proxy_session_id != *proxy_session_id
            || self.issued_at_ms != expected_issued_at_ms
            || self.playback_mode != HlsLeasePlaybackMode::Live
        {
            return Err(HlsTerminalCommitOutcome::LeaseNoLongerEligible);
        }
        if self.admission_generation != expected_admission_generation
            || self.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
                != Some(manifest_snapshot_generation)
            || self.playback_cursor.cursor_generation != cursor_generation
        {
            return Err(HlsTerminalCommitOutcome::SupersededGeneration);
        }
        Ok(())
    }

    fn terminal_decision_commit_preconditions_match(
        &mut self,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
    ) -> Result<(), HlsTerminalCommitOutcome> {
        match preparation.runtime_policy_revocation.as_ref() {
            Some(revocation) => {
                self.runtime_policy_tail_commit_preconditions_match(proxy_session_id, preparation, revocation)
            }
            None => self.terminal_commit_preconditions_match(
                proxy_session_id,
                preparation.lease_issued_at_ms,
                preparation.expected_admission_generation,
                preparation.manifest_snapshot_generation,
                preparation.cursor_generation,
                now_ms,
            ),
        }
    }
}

fn terminal_commit_existing_outcome(
    lease: &mut HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    preparation: &HlsTerminalTailPreparation,
    _now_ms: u64,
) -> Option<HlsTerminalCommitOutcome> {
    if lease.proxy_session_id != *proxy_session_id || lease.issued_at_ms != preparation.lease_issued_at_ms {
        return Some(HlsTerminalCommitOutcome::LeaseNoLongerEligible);
    }
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => None,
        HlsLeasePlaybackMode::TerminalTail(current) if current.generation.0 == preparation.decision_generation => {
            Some(HlsTerminalCommitOutcome::AlreadyCommitted)
        }
        HlsLeasePlaybackMode::TerminalUnavailable { decision_generation, .. }
            if *decision_generation == preparation.decision_generation =>
        {
            Some(HlsTerminalCommitOutcome::AlreadyCommitted)
        }
        HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. } => {
            Some(HlsTerminalCommitOutcome::SupersededGeneration)
        }
        HlsLeasePlaybackMode::Ended => Some(HlsTerminalCommitOutcome::LeaseNoLongerEligible),
    }
}

impl HlsTerminalTailPreparation {
    pub fn bind_ready_terminal_media_requirement(
        &mut self,
        prepared_key: HlsTerminalMediaPreparationKey,
    ) -> Result<(), HlsTerminalTailCompatibility> {
        let source = match self.terminal_media_requirement_source {
            HlsTerminalMediaRequirementSource::AcceptanceEpisode { .. } => {
                if self.required_terminal_media_key != Some(prepared_key) {
                    return Err(HlsTerminalTailCompatibility::AssetRevisionMismatch);
                }
                self.terminal_media_requirement_source
            }
            HlsTerminalMediaRequirementSource::CutoverSnapshotPending { decision_generation }
                if decision_generation == self.decision_generation =>
            {
                HlsTerminalMediaRequirementSource::CutoverSnapshot { decision_generation, asset: prepared_key.asset }
            }
            HlsTerminalMediaRequirementSource::CutoverSnapshot { decision_generation, asset }
                if decision_generation == self.decision_generation && asset == prepared_key.asset =>
            {
                self.terminal_media_requirement_source
            }
            HlsTerminalMediaRequirementSource::CutoverSnapshotPending { .. }
            | HlsTerminalMediaRequirementSource::CutoverSnapshot { .. } => {
                return Err(HlsTerminalTailCompatibility::AssetRevisionMismatch);
            }
        };
        self.terminal_media_requirement_source = source;
        self.required_terminal_media_key = Some(prepared_key);
        self.terminal_media_preparation = HlsTerminalMediaPreparationState::Ready { key: prepared_key };
        Ok(())
    }
}

impl HlsTerminalMediaRequirementSource {
    pub fn authorizes_tail(self, decision_generation: u64, prepared_key: HlsTerminalMediaPreparationKey) -> bool {
        match self {
            Self::AcceptanceEpisode { .. } => true,
            Self::CutoverSnapshotPending { .. } => false,
            Self::CutoverSnapshot { decision_generation: source_generation, asset } => {
                source_generation == decision_generation && asset == prepared_key.asset
            }
        }
    }
}

impl HlsAccessLeaseStore {
    pub fn all_live_leases_terminal_for_session(&self, proxy_session_id: &ProxySessionId) -> bool {
        let mut found = false;
        for lease in self.by_lease_id.values().filter(|lease| {
            lease.proxy_session_id == *proxy_session_id
                && matches!(
                    lease.state,
                    HlsAccessLeaseState::Pending
                        | HlsAccessLeaseState::Activated
                        | HlsAccessLeaseState::Idle
                        | HlsAccessLeaseState::PolicyRevoking
                        | HlsAccessLeaseState::Denied
                )
        }) {
            found = true;
            if !matches!(
                lease.playback_mode,
                HlsLeasePlaybackMode::TerminalTail(_)
                    | HlsLeasePlaybackMode::TerminalUnavailable { .. }
                    | HlsLeasePlaybackMode::Ended
            ) {
                return false;
            }
        }
        found
    }

    pub fn prepare_terminal_tail(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        input: &HlsTerminalTailPreparationInput,
    ) -> Option<HlsTerminalTailPreparation> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id || lease.playback_mode != HlsLeasePlaybackMode::Live {
            return None;
        }
        let mut manifest_snapshot = lease.last_manifest_snapshot.clone()?;
        if manifest_snapshot.snapshot_generation != input.expected_manifest_snapshot_generation
            || lease.playback_cursor.cursor_generation != input.expected_cursor_generation
        {
            return None;
        }
        let runtime_policy_revocation = match input.trigger {
            HlsFiniteTailTrigger::AvailabilityReserve => None,
            HlsFiniteTailTrigger::RuntimePolicy(reason) => {
                let revocation = lease.runtime_policy_revocation.clone()?;
                if lease.state != HlsAccessLeaseState::PolicyRevoking
                    || revocation.reason != reason
                    || revocation.lease_issued_at_ms != lease.issued_at_ms
                    || revocation.expected_admission_generation != lease.admission_generation
                    || revocation.manifest_snapshot_generation != manifest_snapshot.snapshot_generation
                    || revocation.cursor_generation != lease.playback_cursor.cursor_generation
                {
                    return None;
                }
                if reason.base_policy() == HlsRuntimeCustomTailBasePolicy::PreserveCompletedOrInFlightPrefix {
                    manifest_snapshot =
                        runtime_policy_authorized_manifest_prefix(manifest_snapshot, &lease.playback_cursor)?;
                }
                Some(revocation)
            }
        };
        let expected_admission_generation = lease.admission_generation;
        let decision_generation = expected_admission_generation.saturating_add(1);
        if decision_generation == expected_admission_generation {
            return None;
        }
        let terminal_media_requirement_source = match input.terminal_media_requirement_origin {
            HlsTerminalMediaRequirementOrigin::AcceptanceEpisode { generation }
                if generation == input.expected_acceptance_generation =>
            {
                HlsTerminalMediaRequirementSource::AcceptanceEpisode { generation }
            }
            HlsTerminalMediaRequirementOrigin::CutoverSnapshot => {
                HlsTerminalMediaRequirementSource::CutoverSnapshotPending { decision_generation }
            }
            HlsTerminalMediaRequirementOrigin::AcceptanceEpisode { .. } => return None,
        };
        Some(HlsTerminalTailPreparation {
            trigger: input.trigger,
            runtime_policy_revocation,
            lease_issued_at_ms: lease.issued_at_ms,
            decision_generation,
            expected_admission_generation,
            manifest_snapshot_generation: manifest_snapshot.snapshot_generation,
            cursor_generation: lease.playback_cursor.cursor_generation,
            origin_progress_generation: input.origin_progress_generation,
            media_readiness_generation: input.media_readiness_generation,
            origin_epoch: input.origin_epoch,
            last_media_progress_at_ms: input.last_media_progress_at_ms,
            expected_acceptance_generation: input.expected_acceptance_generation,
            terminal_media_requirement_source,
            cutover_timing: input.cutover_timing,
            commit_window: input.commit_window,
            required_terminal_media_key: input.required_terminal_media_key,
            terminal_media_preparation: input.terminal_media_preparation,
            reserve: input.reserve,
            manifest_snapshot,
        })
    }

    pub fn commit_terminal_tail_if_generation_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
        plan: std::sync::Arc<HlsTerminalTailPlan>,
    ) -> HlsTerminalCommitOutcome {
        if !plan.matches_route(proxy_session_id, lease_id)
            || plan.generation.0 != preparation.decision_generation
            || plan.base_manifest.snapshot_generation != preparation.manifest_snapshot_generation
        {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        if let Some(outcome) = terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms) {
            return outcome;
        }
        if let Err(outcome) = lease.terminal_decision_commit_preconditions_match(proxy_session_id, preparation, now_ms)
        {
            return outcome;
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        lease.admission_generation = preparation.decision_generation;
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(plan);
        if let Some(revocation) = preparation.runtime_policy_revocation.as_ref() {
            lease.state = HlsAccessLeaseState::Denied;
            lease.runtime_policy_denial_reason = Some(revocation.reason);
            lease.runtime_policy_revocation = None;
        }
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        HlsTerminalCommitOutcome::Committed
    }

    pub fn terminal_tail_replay_outcome(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
    ) -> Option<HlsTerminalCommitOutcome> {
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get_mut(lease_id)?;
        terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms)
    }

    pub fn commit_terminal_unavailable_if_generation_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
        reason: HlsTerminalTailCompatibility,
    ) -> HlsTerminalCommitOutcome {
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        if let Some(outcome) = terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms) {
            return outcome;
        }
        if let Err(outcome) = lease.terminal_decision_commit_preconditions_match(proxy_session_id, preparation, now_ms)
        {
            return outcome;
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        lease.admission_generation = preparation.decision_generation;
        lease.playback_mode =
            HlsLeasePlaybackMode::TerminalUnavailable { decision_generation: preparation.decision_generation, reason };
        if let Some(revocation) = preparation.runtime_policy_revocation.as_ref() {
            lease.state = HlsAccessLeaseState::Denied;
            lease.runtime_policy_denial_reason = Some(revocation.reason);
            lease.runtime_policy_revocation = None;
        }
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        HlsTerminalCommitOutcome::Committed
    }

    pub fn terminal_unavailable_replay_outcome(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
    ) -> Option<HlsTerminalCommitOutcome> {
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get_mut(lease_id)?;
        terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms)
    }

    /// Clears a persisted cleanup ticket only for the same ended lease/session/generation.
    pub fn acknowledge_terminal_protection_release(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        generation: HlsTerminalTailGeneration,
    ) -> bool {
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return false;
        };
        if !matches!(lease.state, HlsAccessLeaseState::Denied | HlsAccessLeaseState::Expired)
            || lease.proxy_session_id != *proxy_session_id
            || lease.playback_mode != HlsLeasePlaybackMode::Ended
            || lease.pending_terminal_protection_release != Some(generation)
        {
            return false;
        }
        lease.pending_terminal_protection_release = None;
        true
    }
}
