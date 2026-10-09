use super::{
    HlsAccessLease, HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsAccessLeaseStore, HlsDeniedTerminalTailRelease, HlsFiniteTailTrigger, HlsLeaseManifestSnapshot,
    HlsLeasePlaybackCursor, HlsLeasePlaybackMode, HlsRuntimeCustomTailReason, HlsRuntimePolicyRevocation,
    HlsRuntimePolicyRevocationOutcome, HlsTerminalCommitOutcome, HlsTerminalTailPreparation, ProxySessionId,
};
use std::sync::Arc;

impl HlsAccessLease {
    pub fn runtime_policy_revocation_outcome(&self) -> Option<HlsRuntimePolicyRevocationOutcome> {
        match &self.playback_mode {
            HlsLeasePlaybackMode::TerminalTail(plan) => {
                Some(HlsRuntimePolicyRevocationOutcome::AlreadyCommitted { plan: Arc::clone(plan) })
            }
            HlsLeasePlaybackMode::Live => self
                .runtime_policy_revocation
                .clone()
                .map(|token| HlsRuntimePolicyRevocationOutcome::AlreadyPending { token }),
            HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => None,
        }
    }

    pub fn runtime_policy_denial_reason(&self) -> Option<HlsRuntimeCustomTailReason> {
        match &self.playback_mode {
            HlsLeasePlaybackMode::TerminalTail(plan) => Some(plan.reason),
            HlsLeasePlaybackMode::Live => self.runtime_policy_revocation.as_ref().map(|token| token.reason),
            HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
                self.runtime_policy_denial_reason
            }
        }
    }

    pub(super) fn runtime_policy_tail_commit_preconditions_match(
        &mut self,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        revocation: &HlsRuntimePolicyRevocation,
    ) -> Result<(), HlsTerminalCommitOutcome> {
        if self.state != HlsAccessLeaseState::PolicyRevoking
            || self.proxy_session_id != *proxy_session_id
            || self.issued_at_ms != revocation.lease_issued_at_ms
            || self.playback_mode != HlsLeasePlaybackMode::Live
            || self.runtime_policy_revocation.as_ref() != Some(revocation)
            || preparation.runtime_policy_revocation.as_ref() != Some(revocation)
            || preparation.trigger != HlsFiniteTailTrigger::RuntimePolicy(revocation.reason)
        {
            return Err(HlsTerminalCommitOutcome::LeaseNoLongerEligible);
        }
        if self.admission_generation != revocation.expected_admission_generation
            || preparation.expected_admission_generation != revocation.expected_admission_generation
            || self.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
                != Some(revocation.manifest_snapshot_generation)
            || preparation.manifest_snapshot_generation != revocation.manifest_snapshot_generation
            || self.playback_cursor.cursor_generation != revocation.cursor_generation
            || preparation.cursor_generation != revocation.cursor_generation
        {
            return Err(HlsTerminalCommitOutcome::SupersededGeneration);
        }
        Ok(())
    }
}

impl HlsAccessLeaseStore {
    pub fn begin_runtime_policy_revocation(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
        now_ms: u64,
    ) -> HlsRuntimePolicyRevocationOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        };
        if lease.proxy_session_id != *proxy_session_id {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        };
        if let HlsLeasePlaybackMode::TerminalTail(plan) = &lease.playback_mode {
            return HlsRuntimePolicyRevocationOutcome::AlreadyCommitted { plan: Arc::clone(plan) };
        }
        if lease.state == HlsAccessLeaseState::PolicyRevoking {
            return lease
                .runtime_policy_revocation
                .clone()
                .filter(|token| token.reason == reason)
                .map_or(HlsRuntimePolicyRevocationOutcome::NoLongerEligible, |token| {
                    HlsRuntimePolicyRevocationOutcome::AlreadyPending { token }
                });
        }
        let Some(manifest) = lease.last_manifest_snapshot.as_ref() else {
            return HlsRuntimePolicyRevocationOutcome::NoPublishedManifest;
        };
        if lease.state != HlsAccessLeaseState::Activated || lease.playback_mode != HlsLeasePlaybackMode::Live {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        let token = HlsRuntimePolicyRevocation {
            reason,
            lease_issued_at_ms: lease.issued_at_ms,
            expected_admission_generation: lease.admission_generation,
            manifest_snapshot_generation: manifest.snapshot_generation,
            cursor_generation: lease.playback_cursor.cursor_generation,
            started_at_ms: now_ms,
        };
        if self.availability_evidence_can_advance().is_err() {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        let lease_proxy_session_id = lease.proxy_session_id.clone();
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        };
        lease.state = HlsAccessLeaseState::PolicyRevoking;
        lease.runtime_policy_revocation = Some(token.clone());
        lease.runtime_policy_denial_reason = None;
        if self.advance_availability_evidence(&lease_proxy_session_id).is_err() {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        HlsRuntimePolicyRevocationOutcome::Started { token }
    }

    pub fn fail_runtime_policy_revocation(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        token: &HlsRuntimePolicyRevocation,
    ) -> HlsAccessLeaseDenialOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        if lease.proxy_session_id != *proxy_session_id
            || lease.state != HlsAccessLeaseState::PolicyRevoking
            || lease.runtime_policy_revocation.as_ref() != Some(token)
        {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        let lease_proxy_session_id = lease.proxy_session_id.clone();
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        lease.state = HlsAccessLeaseState::Denied;
        lease.runtime_policy_denial_reason = Some(token.reason);
        lease.end_playback();
        if self.advance_availability_evidence(&lease_proxy_session_id).is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    }

    pub fn deny_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        mode: HlsAccessLeaseDenialMode,
    ) -> HlsAccessLeaseDenialOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        if lease.state == HlsAccessLeaseState::PolicyRevoking {
            return HlsAccessLeaseDenialOutcome::PolicyRevocationPending;
        }
        let evidence_changed = lease.state != HlsAccessLeaseState::Denied;
        if evidence_changed && self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        if evidence_changed {
            lease.state = HlsAccessLeaseState::Denied;
        }
        let preserve_finite_decision = matches!(
            lease.playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. }
        );
        if preserve_finite_decision {
            if evidence_changed {
                lease.runtime_policy_denial_reason = match &lease.playback_mode {
                    HlsLeasePlaybackMode::TerminalTail(plan) => Some(plan.reason),
                    HlsLeasePlaybackMode::TerminalUnavailable { .. } => lease.runtime_policy_denial_reason,
                    HlsLeasePlaybackMode::Live | HlsLeasePlaybackMode::Ended => None,
                };
            }
            if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
                return HlsAccessLeaseDenialOutcome::UnknownLease;
            }
            return HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved;
        }
        if evidence_changed {
            lease.end_playback();
            lease.runtime_policy_denial_reason = match mode {
                HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd { reason } => Some(reason),
                HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail => None,
                #[cfg(test)]
                HlsAccessLeaseDenialMode::ImmediateEnd => None,
            };
        }
        let generation = lease.pending_terminal_protection_release;
        if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        HlsAccessLeaseDenialOutcome::Ended {
            terminal_release: generation
                .map(|generation| HlsDeniedTerminalTailRelease { proxy_session_id, generation }),
        }
    }
}

pub(super) fn runtime_policy_authorized_manifest_prefix(
    mut manifest: HlsLeaseManifestSnapshot,
    cursor: &HlsLeasePlaybackCursor,
) -> Option<HlsLeaseManifestSnapshot> {
    let authorized_last_proxy_seq =
        match (cursor.highest_contiguous_completed_proxy_seq, cursor.last_requested_proxy_seq) {
            (Some(completed), Some(requested)) => completed.max(requested),
            (Some(completed), None) => completed,
            (None, Some(requested)) => requested,
            (None, None) => return None,
        }
        .min(manifest.last_proxy_seq);
    let end = manifest
        .visible_segments
        .iter()
        .position(|segment| segment.proxy_seq > authorized_last_proxy_seq)
        .unwrap_or(manifest.visible_segments.len());
    let selected = manifest.visible_segments.get(..end)?.to_vec();
    let first = selected.first()?;
    let last = selected.last()?;
    let first_proxy_seq = first.proxy_seq;
    let last_proxy_seq = last.proxy_seq;
    let active_encryption = last.encryption.clone();
    let playlist_duration_ms = selected.iter().fold(0_u64, |total, segment| total.saturating_add(segment.duration_ms));
    manifest.first_proxy_seq = first_proxy_seq;
    manifest.last_proxy_seq = last_proxy_seq;
    manifest.visible_segments = Arc::from(selected);
    manifest.playlist_duration_ms = playlist_duration_ms;
    manifest.last_visible_media_end_ms = playlist_duration_ms;
    manifest.active_encryption = active_encryption;
    Some(manifest)
}
