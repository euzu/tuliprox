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

#[allow(clippy::too_many_arguments)]
fn validate_retained_live_window(
    cache: &crate::HlsSegmentCache,
    leases: &mut crate::HlsAccessLeaseStore,
    session: &crate::HlsSession,
    lease_id: &HlsAccessLeaseId,
    snapshot: &crate::media_reserve::HlsLeaseManifestSnapshot,
    options: crate::manifest_commit::HlsCachedManifestOptions,
    started_at_ms: u64,
    now_ms: u64,
) -> Result<(), crate::lease::HlsLeaseManifestPublicationRejectReason> {
    use crate::lease::HlsLeaseManifestPublicationRejectReason as Reject;
    let Some(lease) = leases.response_snapshot(lease_id, &session.proxy_session_id, now_ms) else {
        return Err(Reject::LeaseUnavailable);
    };
    if lease.last_manifest_snapshot.as_ref() != Some(snapshot) {
        return Err(Reject::SnapshotChanged);
    }
    if now_ms > snapshot.delivered_at_ms.saturating_add(snapshot.playlist_duration_ms)
        || lease.state != crate::HlsAccessLeaseState::Activated
        || lease.playback_mode != crate::terminal_tail::HlsLeasePlaybackMode::Live
        || options.requires_newer_manifest()
        || session.fresh_manifest_commit_required.is_some()
    {
        return Err(Reject::ManifestGenerationUnavailable);
    }
    let Some(publication) = snapshot.startup_revisions.as_ref() else {
        return Err(Reject::RevisionConflict);
    };
    let generation_changed = publication
        .window_plan
        .as_ref()
        .is_some_and(|window| window.plan.origin_work_generation != session.activity.origin_work_generation);
    if publication.window_plan.is_none()
        || (!generation_changed
            && !crate::manifest_commit::hls_live_window_origin_allowed_for_request(
                session,
                options,
                started_at_ms,
                now_ms,
            ))
    {
        return Err(Reject::ManifestGenerationUnavailable);
    }
    // GC can prune a snapshot's revision map. Every advertised URI must
    // still resolve to the exact lease binding before recovery runs.
    if snapshot.visible_segments.is_empty()
        || snapshot.visible_proxy_seqs().any(|seq| {
            publication
                .revisions
                .get(&seq)
                .is_none_or(|revision| leases.published_segment_revision(lease_id, seq).as_ref() != Some(revision))
        })
    {
        return Err(Reject::RevisionConflict);
    }
    if generation_changed {
        validate_retained_generation_reserve(cache, session, &lease, snapshot, publication, now_ms)?;
    }
    Ok(())
}

fn validate_retained_generation_reserve(
    cache: &crate::HlsSegmentCache,
    session: &crate::HlsSession,
    lease: &crate::HlsAccessLease,
    snapshot: &crate::media_reserve::HlsLeaseManifestSnapshot,
    publication: &crate::HlsManifestRevisions,
    now_ms: u64,
) -> Result<(), crate::lease::HlsLeaseManifestPublicationRejectReason> {
    use crate::{
        lease::HlsLeaseManifestPublicationRejectReason as Reject,
        media_reserve::{evaluate_lease_reserve, HlsLeaseReserveInput},
    };
    // SegmentEntry's immutable media clock is also bound to playback
    // requests. The manifest's duration-relative horizon is not that clock.
    let timeline = session.ready_timeline_snapshot(
        lease.playback_cursor.ready_timeline_start_proxy_seq(snapshot.first_proxy_seq),
        now_ms,
    );
    if session.origin_control.path_condition == crate::origin_progress::HlsOriginPathCondition::HardFetchFailure {
        return Err(Reject::ManifestGenerationUnavailable);
    }
    for segment in snapshot.visible_segments.iter() {
        let revision = &publication.revisions[&segment.proxy_seq];
        let crate::SegmentRevisionState::Complete(metadata) = revision.revision().state() else {
            return Err(Reject::RevisionConflict);
        };
        let Some(unit) = timeline.units.iter().find(|unit| unit.proxy_seq == segment.proxy_seq) else {
            return Err(Reject::RevisionConflict);
        };
        let entry = &session.segments[&segment.proxy_seq];
        if let Some(map_id) = entry.map_ref {
            let Some(map) = session.maps.get(&map_id) else { return Err(Reject::RevisionConflict) };
            let crate::MapCacheStatus::Ready { content_length, .. } = map.status else {
                return Err(Reject::RevisionConflict);
            };
            if !std::fs::metadata(cache.object_path(&map.cache_key))
                .is_ok_and(|file| file.is_file() && file.len() == content_length)
            {
                return Err(Reject::RevisionConflict);
            }
        }
        if let Some(encryption) = &entry.encryption {
            let key = crate::TransientPassthroughState::transient_object_key(
                &session.proxy_session_id,
                &encryption.resource_id,
                encryption.resource_extension.clone(),
            );
            if !std::fs::metadata(cache.object_path(&key)).is_ok_and(|file| file.is_file() && file.len() == 16) {
                return Err(Reject::RevisionConflict);
            }
        }
        if !std::fs::metadata(&metadata.path).is_ok_and(|file| file.is_file() && file.len() == metadata.size)
            || !segment.map_ref_ready
            || unit.duration_ms != segment.duration_ms
            || !unit.required_map_ready
            || !unit.required_key_ready
        {
            return Err(Reject::RevisionConflict);
        }
    }
    let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
        manifest: snapshot,
        cursor: &lease.playback_cursor,
        ready_timeline: &timeline,
        now_ms,
        recovery_trigger_budget: crate::recovery_timing::HlsRecoveryTriggerBudgetMs::from_millis(0),
        origin_path_degraded: true,
        recovery_committed: false,
    });
    if !reserve.playback_position_known || reserve.guaranteed_reserve_ms == 0 {
        return Err(Reject::ManifestGenerationUnavailable);
    }
    Ok(())
}

impl HlsProxyManager {
    /// Select only the fully local published view retired by an origin generation change.
    pub async fn retained_live_window_after_origin_change(
        &self,
        session: &crate::HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        options: crate::manifest_commit::HlsCachedManifestOptions,
        started_at_ms: u64,
        now_ms: u64,
    ) -> Option<crate::HlsAccessLease> {
        let proxy_id = session.read().await.proxy_session_id.clone();
        let current = self.sessions.hold_current_proxy_session(&proxy_id).await?;
        if !Arc::ptr_eq(current.session(), session) {
            return None;
        }
        let mut leases = self.access_leases.write().await;
        let state = session.read().await;
        if state.is_gc_marked_for_removal()
            || state.live_window_origin_work_generation == Some(state.activity.origin_work_generation)
        {
            return None;
        }
        let lease = leases.response_snapshot(lease_id, &proxy_id, now_ms)?;
        let snapshot = lease.last_manifest_snapshot.as_ref()?;
        if snapshot
            .startup_revisions
            .as_ref()?
            .window_plan
            .as_ref()
            .is_none_or(|window| window.plan.origin_work_generation == state.activity.origin_work_generation)
        {
            return None;
        }
        validate_retained_live_window(
            &self.segment_cache,
            &mut leases,
            &state,
            lease_id,
            snapshot,
            options,
            started_at_ms,
            now_ms,
        )
        .ok()?;
        Some(lease)
    }
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

    /// Reuses the publication transaction, fencing a retained response to the
    /// session handle and request freshness policy that selected it.
    #[allow(clippy::too_many_arguments)]
    pub async fn republish_access_lease_live_window(
        &self,
        session: &crate::HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        expected: crate::lease::HlsLeaseManifestPublicationGuard,
        snapshot: crate::media_reserve::HlsLeaseManifestSnapshot,
        options: crate::manifest_commit::HlsCachedManifestOptions,
        started_at_ms: u64,
        now_ms: u64,
    ) -> crate::lease::HlsLeaseManifestPublicationOutcome {
        self.commit_recovered_startup_manifest(
            lease_id,
            session_id,
            expected,
            snapshot,
            crate::HlsPublishedTransientResourceIds::default(),
            now_ms,
            Some((session, options, started_at_ms)),
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "Keep generation checks, revision recovery and publication under the same transaction locks."
    )]
    pub(super) async fn commit_recovered_startup_manifest(
        &self,
        lease_id: &HlsAccessLeaseId,
        session_id: &ProxySessionId,
        expected: crate::lease::HlsLeaseManifestPublicationGuard,
        mut snapshot: crate::media_reserve::HlsLeaseManifestSnapshot,
        resources: crate::HlsPublishedTransientResourceIds,
        now_ms: u64,
        retained_request: Option<(&crate::HlsSessionHandle, crate::manifest_commit::HlsCachedManifestOptions, u64)>,
    ) -> crate::lease::HlsLeaseManifestPublicationOutcome {
        use crate::lease::{
            HlsLeaseManifestPublicationOutcome as Outcome, HlsLeaseManifestPublicationRejectReason as Reject,
        };
        let Some(current) = self.sessions.hold_current_proxy_session(session_id).await else {
            return Outcome::Rejected(Reject::SessionMismatch);
        };
        if retained_request.is_some_and(|(expected_session, _, _)| !Arc::ptr_eq(current.session(), expected_session)) {
            return Outcome::Rejected(Reject::SessionMismatch);
        }
        let mut leases = self.access_leases.write().await;
        let mut session = current.session().write().await;
        if leases
            .response_snapshot(lease_id, session_id, now_ms)
            .is_none_or(|lease| !session.live_entry_snapshot_allowed_for_lease(&lease, Some(&snapshot)))
        {
            return Outcome::Rejected(Reject::ManifestGenerationUnavailable);
        }
        // A retained request may have waited for these locks. Recheck its
        // deadline at the transaction without moving behind the request observation.
        let now_ms = if retained_request.is_some() { now_ms.max(self.now_ms()) } else { now_ms };
        if !leases.media_identity_is_current(lease_id, session_id, expected.media_identity(), now_ms)
            || session.is_gc_marked_for_removal()
        {
            return Outcome::Rejected(Reject::LeaseUnavailable);
        }
        if snapshot.startup_revisions.as_ref().is_some_and(|publication| publication.window_plan.is_some()) {
            session.live_window_strip = self.strip();
        }
        let retained = (retained_request.is_some()
            || snapshot
                .startup_revisions
                .as_ref()
                .and_then(|publication| publication.window_plan.as_ref())
                .is_some_and(|window| window.plan.origin_work_generation == session.activity.origin_work_generation))
            && now_ms <= snapshot.delivered_at_ms.saturating_add(snapshot.playlist_duration_ms)
            && leases
                .response_snapshot(lease_id, session_id, now_ms)
                .and_then(|lease| lease.last_manifest_snapshot)
                .as_ref()
                == Some(&snapshot);
        if let Some((_, options, started_at_ms)) = retained_request {
            if let Err(reason) = validate_retained_live_window(
                &self.segment_cache,
                &mut leases,
                &session,
                lease_id,
                &snapshot,
                options,
                started_at_ms,
                now_ms,
            ) {
                return Outcome::Rejected(reason);
            }
        }
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
        if retained_request.is_some()
            && snapshot.startup_revisions.as_ref().is_none_or(|publication| {
                publication
                    .revisions
                    .get(&snapshot.first_proxy_seq)
                    .is_none_or(|head| !head.revision().is_startup_head_ready(publication.mode))
            })
        {
            return Outcome::Rejected(Reject::RevisionConflict);
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
                session.record_live_window_publication(lease_id, expected.lease_issued_at_ms(), &snapshot);
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
