use super::{
    HlsAccessLeaseId, HlsAccessLeaseStore, HlsMediaActivityCommitOutcome, HlsMediaLeaseIdentity,
    HlsPlaybackRequestToken, HlsProxyManager, HlsSessionHandle, ProxySessionId,
    HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES, HLS_STATE_CAS_LOCK_RETRIES,
};
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
enum HlsMediaActivityCommitKind {
    Access,
    Delivered,
    LiveSegmentCompletion(HlsPlaybackRequestToken),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsMediaActivityCommitAttempt {
    LockBusy,
    Completed { outcome: HlsMediaActivityCommitOutcome, evidence_changed: bool },
}

pub(super) fn hls_key_readiness_evidence_is_current(valid_until_ms: Option<u64>, now_ms: u64) -> bool {
    valid_until_ms.is_none_or(|valid_until_ms| valid_until_ms >= now_ms)
}

impl HlsProxyManager {
    pub async fn record_access_lease_segment_request_started_if_identity_matches(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        proxy_seq: u64,
        requested_at_ms: u64,
    ) -> Option<HlsPlaybackRequestToken> {
        self.access_leases.write().await.record_segment_request_started_if_identity_matches(
            lease_id,
            proxy_session_id,
            lease_identity,
            proxy_seq,
            requested_at_ms,
        )
    }

    pub async fn record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        token: HlsPlaybackRequestToken,
        completed_at_ms: u64,
    ) -> HlsMediaActivityCommitOutcome {
        self.commit_media_activity_if_identity_matches(
            session,
            lease_id,
            proxy_session_id,
            lease_identity,
            completed_at_ms,
            HlsMediaActivityCommitKind::LiveSegmentCompletion(token),
        )
        .await
    }

    pub async fn mark_authorized_media_access_for_lease_if_identity_matches(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
    ) -> HlsMediaActivityCommitOutcome {
        self.commit_media_activity_if_identity_matches(
            session,
            lease_id,
            proxy_session_id,
            lease_identity,
            now_ms,
            HlsMediaActivityCommitKind::Access,
        )
        .await
    }

    pub async fn mark_delivered_media_for_lease(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
    ) -> HlsMediaActivityCommitOutcome {
        self.commit_media_activity_if_identity_matches(
            session,
            lease_id,
            proxy_session_id,
            lease_identity,
            now_ms,
            HlsMediaActivityCommitKind::Delivered,
        )
        .await
    }

    async fn commit_media_activity_if_identity_matches(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
        kind: HlsMediaActivityCommitKind,
    ) -> HlsMediaActivityCommitOutcome {
        let Some(current_session) = self.sessions.get_by_proxy_session_id(proxy_session_id).await else {
            return HlsMediaActivityCommitOutcome::StaleLeaseIdentity;
        };
        if !Arc::ptr_eq(&current_session, session) {
            return HlsMediaActivityCommitOutcome::StaleLeaseIdentity;
        }
        for attempt in 0..HLS_STATE_CAS_LOCK_RETRIES {
            match self.try_commit_media_activity(session, lease_id, proxy_session_id, lease_identity, now_ms, kind) {
                HlsMediaActivityCommitAttempt::Completed { outcome, evidence_changed } => {
                    return self
                        .finish_media_activity_commit(session, proxy_session_id, outcome, evidence_changed)
                        .await;
                }
                HlsMediaActivityCommitAttempt::LockBusy => {
                    if attempt.saturating_add(1) < HLS_STATE_CAS_LOCK_RETRIES {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        // Media activity must not be dropped under sustained lock contention.
        // Wait for one contended lock at a time, then retry the established
        // lease-store -> session acquisition without awaiting under either
        // write guard.
        let mut committed = None;
        for attempt in 0..HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES {
            let mut leases = self.access_leases.write().await;
            let Ok(mut session_guard) = session.try_write() else {
                drop(leases);
                let session_wait = session.write().await;
                drop(session_wait);
                if attempt.saturating_add(1) < HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES {
                    tokio::task::yield_now().await;
                }
                continue;
            };
            let HlsMediaActivityCommitAttempt::Completed { outcome, evidence_changed } =
                Self::commit_media_activity_locked(
                    &mut leases,
                    &mut session_guard,
                    lease_id,
                    proxy_session_id,
                    lease_identity,
                    now_ms,
                    kind,
                )
            else {
                drop(session_guard);
                drop(leases);
                continue;
            };
            drop(session_guard);
            drop(leases);
            committed = Some((outcome, evidence_changed));
            break;
        }
        let Some((outcome, evidence_changed)) = committed else {
            return HlsMediaActivityCommitOutcome::DeferredLockContention;
        };
        self.finish_media_activity_commit(session, proxy_session_id, outcome, evidence_changed).await
    }

    async fn finish_media_activity_commit(
        &self,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        outcome: HlsMediaActivityCommitOutcome,
        evidence_changed: bool,
    ) -> HlsMediaActivityCommitOutcome {
        match outcome {
            HlsMediaActivityCommitOutcome::Committed => {
                if evidence_changed {
                    self.segment_cache.notify_capacity_protection_changed();
                    self.notify_session_evidence_changed(proxy_session_id);
                }
                self.schedule_session_idle_for_handle(session).await;
            }
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity
            | HlsMediaActivityCommitOutcome::DeferredLockContention => {}
        }
        outcome
    }

    fn try_commit_media_activity(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
        kind: HlsMediaActivityCommitKind,
    ) -> HlsMediaActivityCommitAttempt {
        // Media-completion lock order matches terminal publication: lease
        // store -> session. Both locks are non-blocking and no async work runs
        // while either guard is held.
        let Ok(mut leases) = self.access_leases.try_write() else {
            return HlsMediaActivityCommitAttempt::LockBusy;
        };
        let Ok(mut session) = session.try_write() else {
            return HlsMediaActivityCommitAttempt::LockBusy;
        };
        let result = Self::commit_media_activity_locked(
            &mut leases,
            &mut session,
            lease_id,
            proxy_session_id,
            lease_identity,
            now_ms,
            kind,
        );
        drop(session);
        drop(leases);
        result
    }

    fn commit_media_activity_locked(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::super::HlsSession,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
        kind: HlsMediaActivityCommitKind,
    ) -> HlsMediaActivityCommitAttempt {
        if session.proxy_session_id != *proxy_session_id {
            return HlsMediaActivityCommitAttempt::Completed {
                outcome: HlsMediaActivityCommitOutcome::StaleLeaseIdentity,
                evidence_changed: false,
            };
        }
        let (current, capacity_protection_released) = match kind {
            HlsMediaActivityCommitKind::Access | HlsMediaActivityCommitKind::Delivered => {
                (leases.media_identity_is_current(lease_id, proxy_session_id, lease_identity, now_ms), false)
            }
            HlsMediaActivityCommitKind::LiveSegmentCompletion(token) => {
                let completion = leases.record_segment_request_completed_if_identity_matches(
                    lease_id,
                    proxy_session_id,
                    lease_identity,
                    token,
                    now_ms,
                );
                (completion.is_some(), matches!(completion, Some(super::super::HlsPlaybackCompletionOutcome::Advanced)))
            }
        };
        if !current {
            return HlsMediaActivityCommitAttempt::Completed {
                outcome: HlsMediaActivityCommitOutcome::StaleLeaseIdentity,
                evidence_changed: false,
            };
        }
        session.mark_authorized_media_access(now_ms);
        if lease_identity.is_live() && matches!(kind, HlsMediaActivityCommitKind::Delivered) {
            session.activity.last_delivered_media_at_ms = Some(now_ms);
        }
        HlsMediaActivityCommitAttempt::Completed {
            outcome: HlsMediaActivityCommitOutcome::Committed,
            evidence_changed: capacity_protection_released,
        }
    }
}
