use super::{
    evaluate_terminal_cutover, hls_acceptance_recovery_snapshot, hls_estimated_recovery_completion_at,
    hls_key_readiness_evidence_is_current, next_terminal_commit_retry, spawn_terminal_commit_retry_worker,
    HlsAcceptanceRecoverySnapshot, HlsAccessLeaseId, HlsAccessLeaseStore, HlsCurrentProxySessionAccess,
    HlsFiniteTailTrigger, HlsLeaseCutoverTiming, HlsManifestAcceptanceEpisodeStatus, HlsPreparedTerminalBundleKey,
    HlsPreparedTerminalBundleObservation, HlsPreparedTerminalBundleState, HlsProxyManager,
    HlsStandaloneCustomAccessEntry, HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError,
    HlsTerminalAssetRevisionValidation, HlsTerminalCommitAttempt, HlsTerminalCommitCommand, HlsTerminalCommitOutcome,
    HlsTerminalCommitOwnerKey, HlsTerminalCommitOwnerToken, HlsTerminalCommitRequest, HlsTerminalCommitRetryDecision,
    HlsTerminalCommitRetryScheduleDecision, HlsTerminalCommitSubmissionDecision, HlsTerminalCutoverCapability,
    HlsTerminalCutoverDecision, HlsTerminalCutoverInput, HlsTerminalLeaseDecision, HlsTerminalMediaAsset,
    HlsTerminalMediaPreparationState, HlsTerminalMediaRequirementOrigin, HlsTerminalMediaRequirementSource,
    HlsTerminalPendingCoordinator, HlsTerminalTailCompatibility, HlsTerminalTailPreparation,
    HlsTerminalTailPreparationInput, HlsTerminalTailPreparationRequest, HlsTerminalTailProtection,
    HlsTerminalTailProtectionInstall, ProxySessionId,
};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Copy)]
enum HlsTerminalPreparationPurpose {
    Cutover,
    UnavailableAfterOwnerFailure,
}

pub(super) fn terminal_media_requirement_origin(
    recovery_snapshot: &HlsAcceptanceRecoverySnapshot,
) -> HlsTerminalMediaRequirementOrigin {
    match recovery_snapshot.status {
        HlsManifestAcceptanceEpisodeStatus::Missing => HlsTerminalMediaRequirementOrigin::CutoverSnapshot,
        HlsManifestAcceptanceEpisodeStatus::Committed { .. }
        | HlsManifestAcceptanceEpisodeStatus::InFlight { .. }
        | HlsManifestAcceptanceEpisodeStatus::Expired { .. }
        | HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { .. }
        | HlsManifestAcceptanceEpisodeStatus::Superseded { .. } => {
            HlsTerminalMediaRequirementOrigin::AcceptanceEpisode { generation: recovery_snapshot.expected_generation }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsTerminalCommitAuthorization {
    Authorized { protection_capacity_exceeded: bool },
    Rejected(HlsTerminalCommitOutcome),
}

fn terminal_media_requirement_is_bound_to_preparation(preparation: &HlsTerminalTailPreparation) -> bool {
    match preparation.terminal_media_requirement_source {
        HlsTerminalMediaRequirementSource::AcceptanceEpisode { generation } => {
            generation == preparation.expected_acceptance_generation
        }
        HlsTerminalMediaRequirementSource::CutoverSnapshotPending { decision_generation }
        | HlsTerminalMediaRequirementSource::CutoverSnapshot { decision_generation, .. } => {
            decision_generation == preparation.decision_generation
        }
    }
}

fn evaluate_terminal_commit_authorization(
    session: &super::super::HlsSession,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    decision: &HlsTerminalLeaseDecision,
    now_ms: u64,
) -> HlsTerminalCommitAuthorization {
    let recovery_snapshot = hls_acceptance_recovery_snapshot(session, now_ms);
    // Acceptance lifecycle generations describe recovery evidence, not media
    // continuity. A later episode must not invalidate a cutover snapshot while
    // the media progress, readiness, epoch, and lease generations guarded below
    // remain unchanged.
    if !terminal_media_requirement_is_bound_to_preparation(preparation) {
        return HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::SupersededGeneration);
    }
    let protection_capacity_exceeded = matches!(decision, HlsTerminalLeaseDecision::Tail(_))
        && !session.can_install_terminal_tail_protection(lease_id);
    let (terminal, terminal_preparation) = match (decision, protection_capacity_exceeded) {
        (HlsTerminalLeaseDecision::Tail(plan), false) => {
            let prepared_key = plan.media_preparation_key();
            if preparation.required_terminal_media_key != Some(prepared_key)
                || !preparation
                    .terminal_media_requirement_source
                    .authorizes_tail(preparation.decision_generation, prepared_key)
            {
                return HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleIncompatible);
            }
            (
                HlsTerminalCutoverCapability::TailCompatible { prepared_key },
                HlsTerminalMediaPreparationState::Ready { key: prepared_key },
            )
        }
        (HlsTerminalLeaseDecision::Tail(_), true) => (
            HlsTerminalCutoverCapability::TailUnavailable(HlsTerminalTailCompatibility::ProtectionCapacityExceeded),
            preparation.terminal_media_preparation,
        ),
        (
            HlsTerminalLeaseDecision::Unavailable(reason)
            | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason),
            _,
        ) => (HlsTerminalCutoverCapability::TailUnavailable(*reason), preparation.terminal_media_preparation),
    };
    if matches!(decision, HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_)) {
        if !session.origin_control.path_condition.is_degraded() {
            return HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::CutoverNoLongerRequired);
        }
        // This fallback publishes no media bytes. Submission arbitration and
        // the terminal precondition retain the preparation's original
        // exclusive deadline, so this authorization is reachable only while
        // the autonomous owner still has time to commit safely.
        return HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: false };
    }
    if preparation.trigger.is_runtime_policy() {
        return HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded };
    }
    let cutover = evaluate_terminal_cutover(&HlsTerminalCutoverInput {
        reserve: preparation.reserve,
        commit_window: preparation.commit_window,
        acceptance: recovery_snapshot.status,
        required_terminal_media_key: preparation.required_terminal_media_key,
        terminal_preparation,
        terminal,
    });
    match (decision, cutover) {
        (HlsTerminalLeaseDecision::Tail(_), HlsTerminalCutoverDecision::CommitTerminalTail)
            if !protection_capacity_exceeded =>
        {
            HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: false }
        }
        (
            HlsTerminalLeaseDecision::Tail(_),
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(
                HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
            ),
        ) if protection_capacity_exceeded => {
            HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: true }
        }
        (
            HlsTerminalLeaseDecision::Unavailable(expected)
            | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(expected),
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(actual),
        ) if *expected == actual => HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: false },
        (_, HlsTerminalCutoverDecision::NotRequired) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::CutoverNoLongerRequired)
        }
        (_, HlsTerminalCutoverDecision::RetrySupersededSnapshot) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::SupersededGeneration)
        }
        (_, HlsTerminalCutoverDecision::EvaluateTerminalCapability { .. }) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleNotReady)
        }
        (
            _,
            HlsTerminalCutoverDecision::CommitTerminalTail | HlsTerminalCutoverDecision::CommitTerminalUnavailable(_),
        ) => HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleIncompatible),
    }
}

impl HlsProxyManager {
    pub fn start_prepared_terminal_bundle(
        &self,
        asset: Arc<HlsTerminalMediaAsset>,
        target_duration_ms: u64,
        segment_count: u16,
    ) -> HlsPreparedTerminalBundleState {
        self.prepared_terminal_bundles.start_preparation(asset, target_duration_ms, segment_count)
    }

    pub fn prepared_terminal_bundle_state(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleState> {
        self.prepared_terminal_bundles.state(key)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn install_controlled_terminal_bundle_flight_for_test(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<super::super::prepared_terminal_bundle::HlsPreparedTerminalBundleCompletionPublisher> {
        self.prepared_terminal_bundles.install_controlled_flight_for_test(key)
    }

    pub fn observe_prepared_terminal_bundle(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> HlsPreparedTerminalBundleObservation {
        self.prepared_terminal_bundles.observe_exact(key)
    }

    pub fn register_standalone_custom_access(&self, entry: HlsStandaloneCustomAccessEntry, now_ms: u64) {
        self.standalone_custom_access.register(entry, now_ms);
    }

    pub fn resolve_standalone_custom_segment(
        &self,
        lease_id: &HlsAccessLeaseId,
        asset_fingerprint: &str,
        index: u16,
        now_ms: u64,
    ) -> Result<HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError> {
        self.standalone_custom_access.resolve(lease_id, asset_fingerprint, index, now_ms)
    }

    pub fn terminal_pending(&self) -> Arc<HlsTerminalPendingCoordinator> { Arc::clone(&self.terminal_pending) }

    /// Cancels terminal work frozen before newly committed shared media
    /// progress. Callers must not hold the session or lease-store lock; late
    /// registrations remain protected by the final progress-generation CAS.
    pub fn cancel_superseded_terminal_work_for_session(&self, proxy_session_id: &ProxySessionId) {
        self.terminal_pending.cancel_session(proxy_session_id);
        self.terminal_commit_retries.cancel_session(proxy_session_id);
    }

    pub fn terminal_commit_now_ms(&self) -> u64 { self.terminal_commit_clock.now_ms() }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn wait_for_prepared_terminal_bundle(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleState> {
        self.prepared_terminal_bundles.wait_for_completion(key).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_terminal_commit_retry_capacity_for_test(&self, capacity: usize) {
        self.terminal_commit_retries.set_capacity_for_test(capacity);
    }

    pub async fn prepare_access_lease_terminal_tail(
        &self,
        request: HlsTerminalTailPreparationRequest<'_>,
    ) -> Option<HlsTerminalTailPreparation> {
        self.prepare_access_lease_terminal_decision(request, HlsTerminalPreparationPurpose::Cutover).await
    }

    pub async fn prepare_access_lease_terminal_unavailable_after_owner_failure(
        &self,
        request: HlsTerminalTailPreparationRequest<'_>,
    ) -> Option<HlsTerminalTailPreparation> {
        self.prepare_access_lease_terminal_decision(
            request,
            HlsTerminalPreparationPurpose::UnavailableAfterOwnerFailure,
        )
        .await
    }

    async fn prepare_access_lease_terminal_decision(
        &self,
        request: HlsTerminalTailPreparationRequest<'_>,
        purpose: HlsTerminalPreparationPurpose,
    ) -> Option<HlsTerminalTailPreparation> {
        let HlsTerminalTailPreparationRequest {
            lease_id,
            proxy_session_id,
            manifest_snapshot_generation,
            cursor_generation,
            reserve,
            cutover_timing,
            commit_window,
            now_ms,
            origin_progress_generation: expected_origin_progress_generation,
            media_readiness_generation: expected_media_readiness_generation,
            last_media_progress_at_ms: expected_last_media_progress_at_ms,
        } = request;
        let session = self.sessions.get_by_proxy_session_id(proxy_session_id).await?;
        let (
            origin_progress_generation,
            media_readiness_generation,
            origin_epoch,
            last_media_progress_at_ms,
            origin_path_degraded,
            recovery_snapshot,
        ) = {
            let session = session.read().await;
            let recovery_snapshot = hls_acceptance_recovery_snapshot(&session, now_ms);
            (
                session.origin_control.progress_generation,
                session.activity.media_readiness_generation,
                session.origin_control.origin_epoch,
                session.origin_control.last_media_progress_at_ms,
                session.origin_control.path_condition.is_degraded(),
                recovery_snapshot,
            )
        };
        let expected_cutover_timing =
            HlsLeaseCutoverTiming::from_reserve(now_ms, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
        if origin_progress_generation != expected_origin_progress_generation
            || media_readiness_generation != expected_media_readiness_generation
            || last_media_progress_at_ms != expected_last_media_progress_at_ms
            || cutover_timing != expected_cutover_timing
            || !hls_key_readiness_evidence_is_current(reserve.key_readiness_valid_until_ms, now_ms)
        {
            return None;
        }
        if matches!(purpose, HlsTerminalPreparationPurpose::UnavailableAfterOwnerFailure) && !origin_path_degraded {
            return None;
        }
        let cutover_timing = cutover_timing
            .with_estimated_recovery_completion_at(hls_estimated_recovery_completion_at(recovery_snapshot.recovery));
        if matches!(purpose, HlsTerminalPreparationPurpose::Cutover)
            && !matches!(
                evaluate_terminal_cutover(&HlsTerminalCutoverInput {
                    reserve,
                    commit_window,
                    acceptance: recovery_snapshot.status,
                    required_terminal_media_key: recovery_snapshot.required_terminal_media_key,
                    terminal_preparation: recovery_snapshot.terminal_media_preparation,
                    terminal: HlsTerminalCutoverCapability::NotEvaluated,
                }),
                HlsTerminalCutoverDecision::EvaluateTerminalCapability { .. }
            )
        {
            return None;
        }
        let terminal_media_requirement_origin = terminal_media_requirement_origin(&recovery_snapshot);
        self.access_leases.read().await.prepare_terminal_tail(
            lease_id,
            proxy_session_id,
            &HlsTerminalTailPreparationInput {
                trigger: HlsFiniteTailTrigger::AvailabilityReserve,
                expected_manifest_snapshot_generation: manifest_snapshot_generation,
                expected_cursor_generation: cursor_generation,
                origin_progress_generation,
                media_readiness_generation,
                origin_epoch,
                last_media_progress_at_ms,
                expected_acceptance_generation: recovery_snapshot.expected_generation,
                terminal_media_requirement_origin,
                cutover_timing,
                commit_window,
                required_terminal_media_key: recovery_snapshot.required_terminal_media_key,
                terminal_media_preparation: recovery_snapshot.terminal_media_preparation,
                reserve,
            },
        )
    }

    pub fn commit_access_lease_terminal_if_generation_matches(
        &self,
        request: HlsTerminalCommitRequest<'_>,
    ) -> HlsTerminalCommitOutcome {
        let HlsTerminalCommitRequest {
            session,
            lease_id,
            proxy_session_id,
            preparation,
            now_ms,
            payload,
            asset_revision_guard,
        } = request;
        let (decision, media_guard) = payload.into_parts();
        let key = HlsTerminalCommitOwnerKey::from_preparation(proxy_session_id, lease_id, preparation);
        let Some(session_incarnation) = self.sessions.session_incarnation(session) else {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        };
        let Some((cancellation_epoch, submission_token)) = self.terminal_commit_retries.reserve_submission() else {
            return HlsTerminalCommitOutcome::RetryCapacityExceeded;
        };
        let command = HlsTerminalCommitCommand {
            key,
            session: Arc::clone(session),
            session_incarnation,
            preparation: preparation.clone(),
            decision,
            media_guard,
            asset_revision_guard,
            cancellation_epoch,
            submission_token,
        };
        let attempt_now_ms = self.terminal_commit_clock.initial_attempt_now_ms(now_ms);
        let (authorized_command, owner_token) = match self.terminal_commit_retries.submit(command, attempt_now_ms) {
            HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token } => (command, owner_token),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms } => {
                return HlsTerminalCommitOutcome::LockBusy { retry_before_ms };
            }
            HlsTerminalCommitSubmissionDecision::Failed(outcome) => return outcome,
            HlsTerminalCommitSubmissionDecision::Cancelled => {
                return HlsTerminalCommitOutcome::SupersededGeneration;
            }
            HlsTerminalCommitSubmissionDecision::CapacityExceeded => {
                return HlsTerminalCommitOutcome::RetryCapacityExceeded;
            }
        };
        let authorized_key = authorized_command.key.clone();
        let attempt = match self.sessions.try_with_current_proxy_session(
            &authorized_key.proxy_session_id,
            &authorized_command.session,
            || {
                self.terminal_commit_retries.with_current_owner(
                    &authorized_key,
                    owner_token,
                    |command, latest_safe_terminal_commit_at_ms| {
                        let attempt_now_ms = self.terminal_commit_clock.initial_attempt_now_ms(now_ms);
                        let attempt = Self::try_commit_access_lease_terminal_decision(
                            &self.access_leases,
                            command,
                            attempt_now_ms,
                            latest_safe_terminal_commit_at_ms,
                        );
                        (attempt_now_ms, latest_safe_terminal_commit_at_ms, attempt)
                    },
                )
            },
        ) {
            HlsCurrentProxySessionAccess::Acquired(attempt) => attempt,
            HlsCurrentProxySessionAccess::Superseded => {
                self.terminal_commit_retries.discard_owner(&authorized_key, owner_token);
                None
            }
            HlsCurrentProxySessionAccess::LockBusy => self.terminal_commit_retries.with_current_owner(
                &authorized_key,
                owner_token,
                |_command, latest_safe_terminal_commit_at_ms| {
                    (
                        self.terminal_commit_clock.initial_attempt_now_ms(now_ms),
                        latest_safe_terminal_commit_at_ms,
                        HlsTerminalCommitAttempt::LockBusy,
                    )
                },
            ),
        };
        let Some((attempts_completed, (attempt_now_ms, deadline_ms, attempt))) = attempt else {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        };
        match attempt {
            HlsTerminalCommitAttempt::Completed(outcome) => {
                self.terminal_commit_retries.complete_owner(&authorized_key, owner_token);
                outcome
            }
            HlsTerminalCommitAttempt::LockBusy => self.schedule_terminal_commit_retry(
                &authorized_key,
                owner_token,
                attempts_completed,
                attempt_now_ms,
                deadline_ms,
            ),
        }
    }

    pub(crate) fn try_commit_access_lease_terminal_decision(
        access_leases: &Arc<RwLock<HlsAccessLeaseStore>>,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        latest_safe_commit_at_ms: u64,
    ) -> HlsTerminalCommitAttempt {
        // Submission arbitration releases the owner mutex before this path.
        // This follows the cross-store order documented by
        // with_current_recovery_pressure_session: session index -> retry owner
        // -> lease store -> session. The outer two guards prevent replacement
        // or cancellation TOCTOU; all inner locks are non-blocking and no
        // guard crosses an await or I/O boundary.
        let Ok(mut leases) = access_leases.try_write() else {
            return HlsTerminalCommitAttempt::LockBusy;
        };
        let Ok(mut session) = command.session.try_write() else {
            return HlsTerminalCommitAttempt::LockBusy;
        };
        if let Some(outcome) =
            Self::terminal_commit_precondition_outcome(&mut leases, &session, command, now_ms, latest_safe_commit_at_ms)
        {
            return HlsTerminalCommitAttempt::Completed(outcome);
        }
        let lease_id = &command.key.lease_id;
        let preparation = &command.preparation;
        let decision = &command.decision;
        // Asset identity gates only a new tail mutation. An exact terminal
        // replay was handled above and remains immutable. If a prepared tail's
        // asset changed while a LockBusy owner was queued, fail closed in this
        // same generation-bound CAS instead of abandoning the autonomous
        // intent and leaving the lease live.
        match command.asset_revision_guard.validate_current() {
            HlsTerminalAssetRevisionValidation::Current => {}
            HlsTerminalAssetRevisionValidation::Changed { current } => {
                if command.preparation.trigger.is_runtime_policy() {
                    return HlsTerminalCommitAttempt::Completed(HlsTerminalCommitOutcome::SupersededGeneration);
                }
                let reason = current.asset.map_or(HlsTerminalTailCompatibility::MissingAsset, |_| {
                    HlsTerminalTailCompatibility::AssetRevisionMismatch
                });
                return HlsTerminalCommitAttempt::Completed(Self::commit_terminal_unavailable_fallback(
                    &mut leases,
                    &mut session,
                    command,
                    now_ms,
                    reason,
                ));
            }
        }
        let protection_capacity_exceeded =
            match evaluate_terminal_commit_authorization(&session, lease_id, preparation, decision, now_ms) {
                HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded } => {
                    protection_capacity_exceeded
                }
                HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleIncompatible)
                    if matches!(decision, HlsTerminalLeaseDecision::Tail(_)) =>
                {
                    return HlsTerminalCommitAttempt::Completed(Self::commit_terminal_unavailable_fallback(
                        &mut leases,
                        &mut session,
                        command,
                        now_ms,
                        HlsTerminalTailCompatibility::TerminalMediaNotReady,
                    ));
                }
                HlsTerminalCommitAuthorization::Rejected(outcome) => {
                    return HlsTerminalCommitAttempt::Completed(outcome);
                }
            };
        HlsTerminalCommitAttempt::Completed(Self::publish_terminal_commit(
            &mut leases,
            &mut session,
            command,
            now_ms,
            protection_capacity_exceeded,
        ))
    }

    fn terminal_commit_precondition_outcome(
        leases: &mut HlsAccessLeaseStore,
        session: &super::super::HlsSession,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        latest_safe_commit_at_ms: u64,
    ) -> Option<HlsTerminalCommitOutcome> {
        let lease_id = &command.key.lease_id;
        let proxy_session_id = &command.key.proxy_session_id;
        let preparation = &command.preparation;
        let replay = match &command.decision {
            HlsTerminalLeaseDecision::Tail(_) => {
                leases.terminal_tail_replay_outcome(lease_id, proxy_session_id, preparation, now_ms)
            }
            HlsTerminalLeaseDecision::Unavailable(_) | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_) => {
                leases.terminal_unavailable_replay_outcome(lease_id, proxy_session_id, preparation, now_ms)
            }
        };
        if replay.is_some() {
            return replay;
        }
        if now_ms >= latest_safe_commit_at_ms {
            return Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
        }
        if preparation.trigger.is_runtime_policy() {
            return None;
        }
        if session.origin_control.progress_generation == preparation.origin_progress_generation
            && session.activity.media_readiness_generation == preparation.media_readiness_generation
            && session.origin_control.origin_epoch == preparation.origin_epoch
            && session.origin_control.last_media_progress_at_ms == preparation.last_media_progress_at_ms
            && hls_key_readiness_evidence_is_current(preparation.reserve.key_readiness_valid_until_ms, now_ms)
        {
            return None;
        }
        Some(if session.origin_control.last_media_progress_at_ms == preparation.last_media_progress_at_ms {
            HlsTerminalCommitOutcome::SupersededGeneration
        } else {
            HlsTerminalCommitOutcome::RecoveryCommitted
        })
    }

    fn publish_terminal_commit(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::super::HlsSession,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        protection_capacity_exceeded: bool,
    ) -> HlsTerminalCommitOutcome {
        let lease_id = &command.key.lease_id;
        let proxy_session_id = &command.key.proxy_session_id;
        let preparation = &command.preparation;
        let decision = &command.decision;
        let protection = match (decision, protection_capacity_exceeded) {
            (HlsTerminalLeaseDecision::Tail(plan), false) => Some(HlsTerminalTailProtection {
                generation: plan.generation,
                base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
                key_bindings: plan.key_bindings(),
            }),
            (HlsTerminalLeaseDecision::Tail(_), true)
            | (
                HlsTerminalLeaseDecision::Unavailable(_) | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_),
                _,
            ) => None,
        };
        let previous_protection = session.remove_terminal_tail_protection(lease_id);
        if let Some(protection) = protection {
            if session.install_terminal_tail_protection(lease_id.clone(), protection)
                != HlsTerminalTailProtectionInstall::Installed
            {
                session.rollback_terminal_tail_protection(lease_id.clone(), previous_protection);
                return Self::commit_terminal_unavailable_fallback(
                    leases,
                    session,
                    command,
                    now_ms,
                    HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
                );
            }
        }
        let outcome = match (decision, protection_capacity_exceeded) {
            (HlsTerminalLeaseDecision::Tail(plan), false) => leases.commit_terminal_tail_if_generation_matches(
                lease_id,
                proxy_session_id,
                preparation,
                now_ms,
                Arc::clone(plan),
            ),
            (HlsTerminalLeaseDecision::Tail(_), true) => leases.commit_terminal_unavailable_if_generation_matches(
                lease_id,
                proxy_session_id,
                preparation,
                now_ms,
                HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
            ),
            (
                HlsTerminalLeaseDecision::Unavailable(reason)
                | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason),
                _,
            ) => leases.commit_terminal_unavailable_if_generation_matches(
                lease_id,
                proxy_session_id,
                preparation,
                now_ms,
                *reason,
            ),
        };
        if outcome != HlsTerminalCommitOutcome::Committed {
            session.rollback_terminal_tail_protection(lease_id.clone(), previous_protection);
            if outcome == HlsTerminalCommitOutcome::BundleIncompatible
                && matches!(decision, HlsTerminalLeaseDecision::Tail(_))
            {
                return Self::commit_terminal_unavailable_fallback(
                    leases,
                    session,
                    command,
                    now_ms,
                    HlsTerminalTailCompatibility::TerminalMediaNotReady,
                );
            }
            return outcome;
        }
        Self::finish_terminal_commit(leases, session, proxy_session_id);
        HlsTerminalCommitOutcome::Committed
    }

    fn commit_terminal_unavailable_fallback(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::super::HlsSession,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        reason: HlsTerminalTailCompatibility,
    ) -> HlsTerminalCommitOutcome {
        let lease_id = &command.key.lease_id;
        let proxy_session_id = &command.key.proxy_session_id;
        let decision = if matches!(&command.decision, HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_)) {
            HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason)
        } else {
            HlsTerminalLeaseDecision::Unavailable(reason)
        };
        match evaluate_terminal_commit_authorization(session, lease_id, &command.preparation, &decision, now_ms) {
            HlsTerminalCommitAuthorization::Authorized { .. } => {}
            HlsTerminalCommitAuthorization::Rejected(outcome) => return outcome,
        }
        let previous_protection = session.remove_terminal_tail_protection(lease_id);
        let outcome = leases.commit_terminal_unavailable_if_generation_matches(
            lease_id,
            proxy_session_id,
            &command.preparation,
            now_ms,
            reason,
        );
        if outcome != HlsTerminalCommitOutcome::Committed {
            session.rollback_terminal_tail_protection(lease_id.clone(), previous_protection);
            return outcome;
        }
        Self::finish_terminal_commit(leases, session, proxy_session_id);
        HlsTerminalCommitOutcome::Committed
    }

    fn finish_terminal_commit(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::super::HlsSession,
        proxy_session_id: &ProxySessionId,
    ) {
        let all_terminal = leases.all_live_leases_terminal_for_session(proxy_session_id);
        session.origin_control.progress_phase = if all_terminal {
            super::super::origin_progress::HlsOriginProgressPhase::Terminal
        } else {
            super::super::origin_progress::HlsOriginProgressPhase::TerminalPartial
        };
    }

    fn schedule_terminal_commit_retry(
        &self,
        key: &HlsTerminalCommitOwnerKey,
        owner_token: HlsTerminalCommitOwnerToken,
        attempts_completed: u8,
        last_attempt_at_ms: u64,
        latest_safe_terminal_commit_at_ms: u64,
    ) -> HlsTerminalCommitOutcome {
        let (retry_at_ms, attempts_completed) =
            match next_terminal_commit_retry(attempts_completed, last_attempt_at_ms, latest_safe_terminal_commit_at_ms)
            {
                HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed } => {
                    (retry_at_ms, attempts_completed)
                }
                HlsTerminalCommitRetryDecision::AttemptsExhausted => {
                    self.terminal_commit_retries.fail_owner(
                        key,
                        owner_token,
                        HlsTerminalCommitOutcome::RetryAttemptsExhausted,
                    );
                    return HlsTerminalCommitOutcome::RetryAttemptsExhausted;
                }
                HlsTerminalCommitRetryDecision::SafeDeadlineElapsed => {
                    self.terminal_commit_retries.fail_owner(
                        key,
                        owner_token,
                        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed,
                    );
                    return HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed;
                }
            };
        let registration_now_ms = self.terminal_commit_clock.now_ms().max(last_attempt_at_ms);
        match self.terminal_commit_retries.schedule_current(
            key,
            owner_token,
            attempts_completed,
            retry_at_ms,
            registration_now_ms,
        ) {
            HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token } => {
                if let Some(worker_token) = worker_token {
                    spawn_terminal_commit_retry_worker(
                        Arc::clone(&self.sessions),
                        Arc::clone(&self.access_leases),
                        Arc::clone(&self.terminal_commit_retries),
                        Arc::clone(&self.terminal_commit_clock),
                        worker_token,
                        Self::try_commit_access_lease_terminal_decision,
                    );
                }
                HlsTerminalCommitOutcome::LockBusy { retry_before_ms: retry_at_ms }
            }
            HlsTerminalCommitRetryScheduleDecision::Failed(outcome) => outcome,
            HlsTerminalCommitRetryScheduleDecision::Cancelled => HlsTerminalCommitOutcome::SupersededGeneration,
            HlsTerminalCommitRetryScheduleDecision::WorkerUnavailable => {
                HlsTerminalCommitOutcome::RetryWorkerUnavailable
            }
        }
    }
}
