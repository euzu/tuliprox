use super::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseState, HlsAccessLeaseStore,
    HlsAvailabilityEvidenceAdvanceError, HlsLeasePlaybackCursor, HlsLeasePlaybackMode, HlsLeaseStartupAdmissionState,
    HlsPlaybackFamilyKey, HlsPublishedTransientResourceIds, ProxySessionId,
};
use std::collections::BTreeSet;
use tuliprox_session::ConnectionKind;

impl HlsAccessLeasePendingDeadline {
    pub const fn deadline_ms(self) -> u64 {
        match self {
            Self::Bootstrap { deadline_ms } | Self::FollowUp { deadline_ms } => deadline_ms,
        }
    }
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
            known_bitrate_bps: None,
            origin_connection_kind: ConnectionKind::Normal,
            origin_priority: 0,
            state: HlsAccessLeaseState::Pending,
            issued_at_ms: now_ms,
            last_seen_at_ms: now_ms,
            active_until_ms: None,
            pending_deadline: Some(HlsAccessLeasePendingDeadline::Bootstrap {
                deadline_ms: now_ms.saturating_add(valid_window_ms),
            }),
            valid_until_ms: now_ms.saturating_add(valid_window_ms),
            epg_reference_ts: None,
            archive_origin_url: None,
            playback_mode: HlsLeasePlaybackMode::Live,
            startup_admission: HlsLeaseStartupAdmissionState::Pending,
            playback_cursor: HlsLeasePlaybackCursor::default(),
            revision_bindings: None,
            progressive_startup_claimed: false,
            last_manifest_snapshot: None,
            published_transient_resource_ids: HlsPublishedTransientResourceIds::default(),
            published_finalized_manifest_generations: BTreeSet::new(),
            manifest_snapshot_generation: 0,
            admission_generation: 0,
            runtime_policy_revocation: None,
            runtime_policy_denial_reason: None,
            pending_terminal_protection_release: None,
        }
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 { now_ms.saturating_sub(self.issued_at_ms) }

    pub fn pending_deadline_ms(&self) -> Option<u64> {
        self.pending_deadline.map(HlsAccessLeasePendingDeadline::deadline_ms)
    }

    pub(super) fn validity_due_at_ms(&self) -> u64 {
        if self.state == HlsAccessLeaseState::Pending {
            self.pending_deadline_ms().unwrap_or(self.valid_until_ms)
        } else {
            self.valid_until_ms
        }
    }

    pub(super) fn apply_pending_deadline(&mut self, deadline: HlsAccessLeasePendingDeadline) -> bool {
        let previous = self.pending_deadline;
        let deadline = self.pending_deadline.map_or(deadline, |current| current.tightened_with(deadline));
        self.pending_deadline = Some(deadline);
        self.valid_until_ms = deadline.deadline_ms();
        previous != self.pending_deadline
    }

    fn refresh_validity(&mut self, now_ms: u64) -> bool {
        if self.state != HlsAccessLeaseState::Expired && self.validity_due_at_ms() <= now_ms {
            self.state = HlsAccessLeaseState::Expired;
            if self.playback_mode == HlsLeasePlaybackMode::Live {
                self.end_playback();
            }
            return true;
        }
        false
    }
}

impl HlsAccessLeaseStore {
    pub(super) fn refresh_access_lease_validity(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Result<bool, HlsAvailabilityEvidenceAdvanceError> {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return Ok(false);
        };
        if lease.state == HlsAccessLeaseState::Expired || lease.validity_due_at_ms() > now_ms {
            return Ok(false);
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        self.availability_evidence_can_advance()?;
        let changed = self.by_lease_id.get_mut(lease_id).is_some_and(|lease| lease.refresh_validity(now_ms));
        if changed {
            self.advance_availability_evidence(&proxy_session_id)?;
        }
        Ok(changed)
    }

    pub(super) fn refresh_access_lease_validities_for_session(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Result<(), HlsAvailabilityEvidenceAdvanceError> {
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in lease_ids {
            self.refresh_access_lease_validity(&lease_id, now_ms)?;
        }
        Ok(())
    }
}
