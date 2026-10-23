use crate::{
    lease::{HlsRevisionRecoveryGuard, HlsRevisionRecoveryOutcome},
    HlsAccessLeaseId, HlsMediaLeaseIdentity, HlsProxyManager, ProxySessionId,
};
use std::sync::Arc;

/// Manager-owned resources used by fetch completion, even when an origin I/O
/// context is absent. Sharing this handle avoids a second lease cleanup path.
#[derive(Clone)]
pub(crate) struct HlsLeaseRuntime {
    pub(super) sessions: Arc<crate::HlsSessionStore>,
    pub(super) lease_completion: Arc<super::lease_completion::HlsLeaseCompletion>,
    pub(super) access_leases: Arc<tokio::sync::RwLock<crate::HlsAccessLeaseStore>>,
    pub(super) standalone_custom_access: Arc<crate::runtime_custom_tail::HlsStandaloneCustomAccessStore>,
    pub(super) segment_repair: Arc<crate::HlsSegmentRepairManager>,
    pub(super) startup_observability: Arc<crate::HlsStartupObservability>,
    pub(super) qos: Arc<crate::HlsQosRegistry>,
    pub(super) terminal_pending: Arc<crate::terminal_pending::HlsTerminalPendingCoordinator>,
    pub(super) terminal_commit_retries: Arc<crate::terminal_commit::HlsTerminalCommitRetryCoordinator>,
}

impl HlsProxyManager {
    pub(super) fn revision_recovery_runtime(&self) -> HlsLeaseRuntime {
        HlsLeaseRuntime {
            sessions: Arc::clone(&self.sessions),
            lease_completion: Arc::clone(&self.lease_completion),
            access_leases: Arc::clone(&self.access_leases),
            standalone_custom_access: Arc::clone(&self.standalone_custom_access),
            segment_repair: Arc::clone(&self.segment_repair),
            startup_observability: Arc::clone(&self.startup_observability),
            qos: Arc::clone(&self.qos),
            terminal_pending: Arc::clone(&self.terminal_pending),
            terminal_commit_retries: Arc::clone(&self.terminal_commit_retries),
        }
    }

    pub(super) fn install_worker_revision_recovery(&self) {
        let _ = self.segment_worker_pool.revision_recovery.set(Arc::new(self.revision_recovery_runtime()));
    }

    pub(super) async fn commit_recovered_startup_manifest(
        &self,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        expected: crate::lease::HlsLeaseManifestPublicationGuard,
        mut snapshot: crate::media_reserve::HlsLeaseManifestSnapshot,
        resources: crate::HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> crate::lease::HlsLeaseManifestPublicationOutcome {
        use crate::lease::{
            HlsLeaseManifestPublicationOutcome as Outcome, HlsLeaseManifestPublicationRejectReason as Reject,
        };
        let Some(current) = self.sessions.hold_current_proxy_session(session_id).await else {
            return Outcome::Rejected(Reject::SessionMismatch);
        };
        let mut leases = self.access_leases.write().await;
        let mut session = current.session().write().await;
        if !leases.media_identity_is_current(lease_id, session_id, expected.media_identity(), now_ms)
            || session.is_gc_marked_for_removal()
        {
            return Outcome::Rejected(Reject::LeaseUnavailable);
        }
        if snapshot.startup_revisions.as_ref().is_some_and(|publication| publication.window_plan.is_some()) {
            session.live_window_strip = self.strip();
        }
        let retained = now_ms <= snapshot.delivered_at_ms.saturating_add(snapshot.playlist_duration_ms)
            && leases
                .response_snapshot(lease_id, session_id, now_ms)
                .and_then(|lease| lease.last_manifest_snapshot)
                .as_ref()
                == Some(&snapshot);
        if let Some(publication) = snapshot.startup_revisions.as_mut() {
            if let Some(window) = &publication.window_plan {
                let plan = &window.plan;
                let view_matches = leases.response_snapshot(lease_id, session_id, now_ms).is_some_and(|lease| {
                    session.plan_live_window(crate::live_window::HlsLiveWindowView::for_lease(&lease)).as_ref()
                        == Some(plan)
                });
                if (!plan.is_current(&session) || !view_matches) && !retained {
                    return Outcome::Rejected(Reject::ManifestGenerationUnavailable);
                }
            }
            for (seq, revision) in &mut std::sync::Arc::make_mut(publication).revisions {
                if let Some(guard) = leases.revision_recovery_guard(lease_id, *seq) {
                    match leases.recover_revision_binding(lease_id, &guard, &session, now_ms) {
                        HlsRevisionRecoveryOutcome::ReentryRequired => {
                            drop(session);
                            drop(leases);
                            drop(current);
                            self.end_access_lease_for_failed_revision(lease_id, session_id, &guard, now_ms).await;
                            return Outcome::Rejected(Reject::LeaseUnavailable);
                        }
                        HlsRevisionRecoveryOutcome::RetryPending => return Outcome::Rejected(Reject::RevisionConflict),
                        HlsRevisionRecoveryOutcome::Unchanged | HlsRevisionRecoveryOutcome::ReplacementBound => {}
                    }
                    if let Some(bound) = leases.published_segment_revision(lease_id, *seq) {
                        if revision.revision().id() < bound.revision().id()
                            && matches!(revision.revision().state(), crate::SegmentRevisionState::Failed)
                        {
                            *revision = bound;
                        }
                    }
                }
                // Publication cannot introduce a revision left by a retired
                // fetch, nor use a caller-supplied candidate outside startup.
                if session.startup.as_ref().and_then(|startup| startup.revisions.get(seq)) != Some(revision)
                    && leases.published_segment_revision(lease_id, *seq).as_ref() != Some(revision)
                {
                    return Outcome::Rejected(Reject::RevisionConflict);
                }
            }
        }
        let published = snapshot
            .startup_revisions
            .as_ref()
            .and_then(|publication| publication.window_plan.as_ref())
            .map(|_| snapshot.clone());
        let outcome = leases
            .commit_manifest_publication_with_resources(lease_id, session_id, expected, snapshot, resources, now_ms);
        if outcome.is_committed() {
            if let Some(snapshot) = published {
                session.record_live_window_publication(lease_id, &snapshot);
            }
        }
        outcome
    }

    /// Resolves one published URI against the exact current session and lease.
    /// The index -> lease store -> session order matches publication/cutover.
    pub(crate) async fn recover_access_lease_revision(
        &self,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        identity: HlsMediaLeaseIdentity,
        seq: u64,
        now_ms: u64,
    ) -> HlsRevisionRecoveryOutcome {
        let Some(current) = self.sessions.hold_current_proxy_session(session_id).await else {
            return HlsRevisionRecoveryOutcome::Unchanged;
        };
        let mut leases = self.access_leases.write().await;
        let session = current.session().read().await;
        if !leases.media_identity_is_current(lease_id, session_id, identity, now_ms) {
            return HlsRevisionRecoveryOutcome::Unchanged;
        }
        let Some(guard) = leases.revision_recovery_guard(lease_id, seq) else {
            return HlsRevisionRecoveryOutcome::Unchanged;
        };
        let outcome = leases.recover_revision_binding(lease_id, &guard, &session, now_ms);
        drop(session);
        drop(leases);
        drop(current);
        if outcome == HlsRevisionRecoveryOutcome::ReentryRequired {
            self.end_access_lease_for_failed_revision(lease_id, session_id, &guard, now_ms).await;
        }
        outcome
    }

    pub(crate) async fn end_access_lease_for_failed_revision(
        &self,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        guard: &HlsRevisionRecoveryGuard,
        now_ms: u64,
    ) -> bool {
        self.revision_recovery_runtime().end_access_lease_for_failed_revision(lease_id, session_id, guard, now_ms).await
    }
}

impl HlsLeaseRuntime {
    /// Retire only the observed binding/snapshot/lease incarnation. Repeated
    /// callers and completions from older snapshots are intentionally harmless.
    pub(crate) async fn end_access_lease_for_failed_revision(
        &self,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        guard: &HlsRevisionRecoveryGuard,
        now_ms: u64,
    ) -> bool {
        let (issued_at_ms, snapshot_generation) = guard.completion_identity();
        let key = super::lease_completion::LeaseCompletionKey {
            lease_id: lease_id.clone(),
            session_id: session_id.clone(),
            issued_at_ms,
            snapshot_generation: Some(snapshot_generation),
        };
        let runtime = self.clone();
        let lease_id = lease_id.clone();
        let session_id = session_id.clone();
        let guard = guard.clone();
        self.lease_completion
            .run(key, async move { runtime.end_failed_revision_owned(&lease_id, &session_id, &guard, now_ms).await })
            .await
    }

    async fn end_failed_revision_owned(
        &self,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        guard: &HlsRevisionRecoveryGuard,
        now_ms: u64,
    ) -> bool {
        let _users = self.lease_completion.users().await;
        let Some(current) = self.sessions.hold_current_proxy_session(session_id).await else { return false };
        let mut leases = self.access_leases.write().await;
        let session = current.session().read().await;
        let ended_at_ms = tuliprox_core::utils::current_time_millis().max(now_ms);
        let preparation = leases.end_revision_playback_if_current(lease_id, guard, &session, ended_at_ms);
        drop(session);
        drop(leases);
        drop(current);
        let Some(preparation) = preparation else { return false };
        self.complete_prepared_lease(lease_id, &preparation).await
    }

    pub(crate) async fn recover_segment_leases_after_fetch(
        &self,
        session_id: &ProxySessionId,
        seq: u64,
        origin_generation: u64,
        now_ms: u64,
    ) {
        let Some(current) = self.sessions.hold_current_proxy_session(session_id).await else { return };
        let mut leases = self.access_leases.write().await;
        let session = current.session().read().await;
        if session.activity.origin_work_generation != origin_generation || session.is_gc_marked_for_removal() {
            return;
        }
        let ids = leases.revision_bound_lease_ids(session_id, seq);
        let mut endings = Vec::new();
        for id in ids {
            if let Some(guard) = leases.revision_recovery_guard(&id, seq) {
                if leases.recover_revision_binding(&id, &guard, &session, now_ms)
                    == HlsRevisionRecoveryOutcome::ReentryRequired
                {
                    endings.push((id, guard));
                }
            }
        }
        drop(session);
        drop(leases);
        drop(current);
        for (id, guard) in endings {
            self.end_access_lease_for_failed_revision(&id, session_id, &guard, now_ms).await;
        }
    }
}
