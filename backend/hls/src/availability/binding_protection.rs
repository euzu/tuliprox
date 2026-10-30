//! Inactivity excludes only proven delivery blockades, never playlist traffic.
use crate::{
    manifest_acceptance::{manifest_acceptance_episode_status, HlsManifestAcceptanceEpisodeStatus},
    media_reserve::{evaluate_lease_reserve, HlsLeaseReserveInput, HlsManifestDeliveryMode, HlsReadyMediaState},
    origin::HlsOriginAccountDetachedReason,
    origin_account_runtime::HlsOriginAccountSnapshot,
    origin_progress::HlsOriginPathCondition,
    recovery_timing::{HlsLeaseCutoverTiming, HlsRecoveryTriggerBudgetMs},
    HlsAccessLease, HlsAccessLeaseStore, HlsAccountBindingProtection, HlsLifecycleEventKey, HlsManifestRenderer,
    HlsProxyManager, HlsSession, HlsSessionHandle,
};
use std::{
    sync::{Arc, Weak},
    time::Duration,
};
use tuliprox_session::{ActiveProviderManager, ProviderWorkReservation};

/// Clock bookkeeping for one admitted binding and one real media-activity anchor.
/// Availability itself remains derived from the existing lease and READY evidence.
#[derive(Default)]
pub(crate) struct HlsBindingInactivity {
    provider: Weak<ActiveProviderManager>,
    lifecycle: Weak<crate::HlsLifecycleManager>,
    identity: Option<HlsOriginAccountSnapshot>,
    activity_at_ms: u64,
    excluded_ms: u64,
    pause: Option<(u64, u64)>,
    recovery_deadline_ms: Option<u64>,
    reservation: Option<ProviderWorkReservation>,
    reservation_deadline_ms: u64,
}

impl std::fmt::Debug for HlsBindingInactivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlsBindingInactivity")
            .field("activity_at_ms", &self.activity_at_ms)
            .field("excluded_ms", &self.excluded_ms)
            .field("pause", &self.pause)
            .field("recovery_deadline_ms", &self.recovery_deadline_ms)
            .field("reservation_deadline_ms", &self.reservation_deadline_ms)
            .field("has_reservation", &self.reservation.is_some())
            .finish_non_exhaustive()
    }
}

impl HlsBindingInactivity {
    pub(crate) fn reset(&mut self) {
        let provider = self.provider.clone();
        let lifecycle = self.lifecycle.clone();
        *self = Self { provider, lifecycle, ..Self::default() };
    }

    fn matches(&self, session: &HlsSession, activity_at_ms: u64) -> bool {
        self.activity_at_ms == activity_at_ms
            && self.identity.as_ref().is_some_and(|identity| identity.matches_session(session))
    }

    pub(crate) fn excluded_ms(&self, session: &HlsSession, activity_at_ms: u64, now_ms: u64) -> u64 {
        if !self.matches(session, activity_at_ms) {
            return 0;
        }
        self.excluded_ms.saturating_add(self.pause.map_or(0, |(start, end)| now_ms.min(end).saturating_sub(start)))
    }

    pub(crate) fn is_paused(&self, session: &HlsSession, now_ms: u64) -> bool {
        session.account_binding_activity().is_some_and(|(activity, _)| self.matches(session, activity))
            && self.pause.is_some_and(|(_, end)| now_ms < end)
            && self.reservation.as_ref().is_some_and(ProviderWorkReservation::is_current)
    }

    fn close_pause(&mut self, now_ms: u64) {
        if let Some((start, end)) = self.pause.take() {
            self.excluded_ms = self.excluded_ms.saturating_add(now_ms.min(end).saturating_sub(start));
        }
    }
}

/// Does the next manifest-reachable unit advance this particular player?
/// READY dependencies and fast-start revision readiness use the same evidence as delivery.
fn further_media_available(session: &HlsSession, lease: &HlsAccessLease, now_ms: u64) -> bool {
    let Some(manifest) = lease.last_manifest_snapshot.as_ref() else { return false };
    let Some(completed) = lease.playback_cursor.highest_contiguous_completed_proxy_seq else { return false };
    if manifest.delivery_mode == HlsManifestDeliveryMode::TransientPassthrough {
        // Transient reserve deliberately has no inferred playback clock. Do not
        // manufacture cache-backed availability or a recovery deadline for it.
        return false;
    }
    let rendered = HlsManifestRenderer::render(session, now_ms).ok();
    let next = rendered
        .as_ref()
        .and_then(|window| window.segment_proxy_seqs.iter().copied().find(|seq| *seq > completed))
        .or_else(|| manifest.visible_proxy_seqs().find(|seq| *seq > completed));
    let Some(next) = next else { return false };
    let timeline = session.ready_timeline_snapshot(next, now_ms);
    timeline.units.first().is_some_and(|unit| {
        unit.required_map_ready
            && unit.required_key_ready
            && (unit.state == HlsReadyMediaState::Ready
                || session.startup.as_ref().is_some_and(|startup| {
                    startup.revisions.get(&next).is_some_and(|revision| revision.revision().is_publishable())
                }))
    })
}

fn blockade_deadline(
    leases: &mut HlsAccessLeaseStore,
    session: &HlsSession,
    continuing_pause: bool,
    now_ms: u64,
) -> Option<u64> {
    let proven_failure = !matches!(
        session.origin_control.path_condition,
        HlsOriginPathCondition::ProgressExpected | HlsOriginPathCondition::PublicationLate
    );
    // Publication lateness alone is ordinary waiting. An existing acceptance
    // episode records whether Availability actually authorized reserve recovery.
    let qualified_recovery =
        session.origin_control.acceptance_episode.as_ref().is_some_and(|episode| {
            episode.trigger().recovery_required()
                && matches!(
                    manifest_acceptance_episode_status(
                        Some(episode),
                        session.origin_control.acceptance_generation,
                        now_ms,
                    ),
                    HlsManifestAcceptanceEpisodeStatus::InFlight { .. }
                        | HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { .. }
                )
        });
    if !proven_failure && !qualified_recovery && !continuing_pause {
        return None;
    }
    if session.established_manifest_recovery_binding().is_none() && !continuing_pause {
        return None;
    }
    // An unchanged successful sample may clear the transport error, but cannot
    // close a proven delivery blockade. Only reachable new media closes it.
    leases
        .terminal_playback_snapshots_for_session(&session.proxy_session_id, now_ms)
        .iter()
        .filter_map(|lease| {
            if !lease.permits_origin_work(now_ms) {
                return None;
            }
            let manifest = lease.last_manifest_snapshot.as_ref()?;
            lease.playback_cursor.highest_contiguous_completed_proxy_seq?;
            if further_media_available(session, lease, now_ms) {
                return None;
            }
            let timeline = session.ready_timeline_snapshot(
                lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
                now_ms,
            );
            let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
                manifest,
                cursor: &lease.playback_cursor,
                ready_timeline: &timeline,
                now_ms,
                recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(0),
                origin_path_degraded: true,
                recovery_committed: false,
            });
            if !reserve.playback_position_known {
                return None;
            }
            let cutover = HlsLeaseCutoverTiming::from_reserve(
                now_ms,
                reserve.guaranteed_reserve_ms,
                reserve.transition_margin,
                None,
            );
            let deadline = cutover.lease_exhaustion_at.as_millis_since_epoch().min(lease.valid_until_ms);
            (deadline > now_ms).then_some(deadline)
        })
        .max()
}

pub(crate) fn reconcile_in_snapshot(
    provider: &Arc<ActiveProviderManager>,
    leases: &mut HlsAccessLeaseStore,
    session: &mut HlsSession,
    now_ms: u64,
) -> Option<u64> {
    if session.origin_playback_kind() != tuliprox_core::model::PlaybackKind::LiveHls {
        return None;
    }
    session.binding_inactivity.provider = Arc::downgrade(provider);
    // Startup has its own admission and Pending deadlines. Recovery protection
    // observes established playback only; a manifest alone supplies no cursor.
    session.activity.last_delivered_media_at_ms?;
    let binding = session.origin_account_binding.as_ref()?;
    if !binding.is_active() || session.is_gc_marked_for_removal() {
        session.binding_inactivity.reset();
        return None;
    }
    let binding = binding.clone();
    let (activity, timing) = session.account_binding_activity()?;
    let normally_expired = matches!(session.account_binding_protection(now_ms), HlsAccountBindingProtection::Expired);
    let mut clock = std::mem::take(&mut session.binding_inactivity);
    if !clock.identity.as_ref().is_some_and(|identity| identity.matches_session(session)) {
        clock.reset();
    }
    if !clock.matches(session, activity) {
        // Real media starts a fresh inactivity clock, but the old right must
        // survive until its real-media confirmation has completed.
        clock.close_pause(now_ms);
        clock.activity_at_ms = activity;
        clock.excluded_ms = 0;
        clock.recovery_deadline_ms = None;
        clock.identity = Some(HlsOriginAccountSnapshot::from_session(session));
    }
    let has_live = leases.has_usable_access_lease_for_session(&session.proxy_session_id, now_ms);
    let deadline = has_live.then(|| blockade_deadline(leases, session, clock.pause.is_some(), now_ms)).flatten();
    if clock.pause.is_some_and(|(_, end)| now_ms >= end) {
        clock.close_pause(now_ms);
    }
    if let Some(deadline) = deadline {
        let deadline = clock.recovery_deadline_ms.map_or(deadline, |previous| previous.min(deadline));
        if let Some((_, end)) = clock.pause.as_mut() {
            // Neither additional polls nor a later viewer starts another budget.
            *end = (*end).min(deadline);
        } else if !normally_expired && deadline > now_ms {
            if let Some(tag) = binding
                .provider_binding_tag
                .filter(|_| clock.reservation.is_none() || clock.recovery_deadline_ms.is_none())
            {
                let physical_deadline = deadline.saturating_add(timing.reservation_ttl_secs().saturating_mul(1_000));
                let until =
                    tokio::time::Instant::now() + Duration::from_millis(physical_deadline.saturating_sub(now_ms));
                if let Some(right) =
                    provider.reserve_hls_origin_work(&binding.session_owner, &binding.account_name, tag, until)
                {
                    clock.reservation = Some(right);
                    clock.reservation_deadline_ms = physical_deadline;
                    clock.recovery_deadline_ms = Some(deadline);
                }
            }
            if clock.recovery_deadline_ms.is_some()
                && clock.reservation.as_ref().is_some_and(ProviderWorkReservation::is_current)
            {
                clock.pause = Some((now_ms.max(activity), deadline));
            }
        }
    } else {
        clock.close_pause(now_ms);
    }
    let had_recovery_protection = clock.reservation.is_some() || clock.pause.is_some() || clock.excluded_ms > 0;
    let next = if !has_live {
        clock.reset();
        None
    } else if let Some((_, end)) = clock.pause {
        Some(end)
    } else {
        let expiry = activity
            .saturating_add(clock.excluded_ms)
            .saturating_add(timing.reservation_ttl_secs().saturating_mul(1_000));
        if now_ms >= expiry || !clock.reservation.as_ref().is_some_and(ProviderWorkReservation::is_current) {
            clock.reservation = None;
            None
        } else {
            Some(expiry.min(clock.reservation_deadline_ms))
        }
    };
    session.binding_inactivity = clock;
    if !has_live && had_recovery_protection {
        release_unused_origin(leases, session, now_ms);
    }
    next
}

/// Called in the same lease-store/session transaction as terminal publication.
pub(crate) fn release_unused_origin(leases: &mut HlsAccessLeaseStore, session: &mut HlsSession, now_ms: u64) {
    if leases.has_usable_access_lease_for_session(&session.proxy_session_id, now_ms) {
        return;
    }
    let Some(provider) = session.binding_inactivity.provider.upgrade() else { return };
    let Some(binding) = session.origin_account_binding.as_ref().filter(|binding| binding.is_active()).cloned() else {
        return;
    };
    session.origin_work_cancellation.cancel();
    session.invalidate_queued_origin_work();
    if let Some(io) = session.origin_account_io_lease.take() {
        io.cancel();
    }
    if let Some(current) = session.origin_account_binding.as_mut() {
        current.detach(HlsOriginAccountDetachedReason::Cleanup, now_ms);
    }
    provider.clear_identified_provider_reservation(
        &binding.session_owner,
        &binding.account_name,
        binding.provider_binding_tag,
    );
}

pub(crate) fn after_terminal_commit(leases: &mut HlsAccessLeaseStore, session: &mut HlsSession, now_ms: u64) {
    after_lease_removal(leases, session, now_ms);
    release_unused_origin(leases, session, now_ms);
}

pub(crate) fn after_lease_removal(leases: &mut HlsAccessLeaseStore, session: &mut HlsSession, now_ms: u64) {
    let Some(provider) = session.binding_inactivity.provider.upgrade() else { return };
    reconcile_in_snapshot(&provider, leases, session, now_ms);
    if let Some(lifecycle) = session.binding_inactivity.lifecycle.upgrade() {
        let key = HlsLifecycleEventKey::OriginBindingProtection { proxy_session_id: session.proxy_session_id.clone() };
        // Recompute after the terminal transaction has released its locks. An
        // immediate wake cannot postpone a successor's earlier expiry event.
        tokio::spawn(async move {
            lifecycle.schedule(key, now_ms).await;
        });
    }
}

impl HlsProxyManager {
    /// One authoritative decision for admission, overlap, availability and expiry.
    pub async fn sync_recovery_binding_protection(
        &self,
        provider: &Arc<ActiveProviderManager>,
        session: &HlsSessionHandle,
        now_ms: u64,
    ) -> bool {
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let Some(current) = self.sessions().hold_current_proxy_session(&proxy_session_id).await else { return false };
        if !Arc::ptr_eq(current.session(), session) {
            return false;
        }
        let mut leases = self.segment_worker_pool().access_leases().write().await;
        let mut state = session.write().await;
        let now_ms = now_ms.max(self.now_ms());
        state.binding_inactivity.lifecycle = Arc::downgrade(self.lifecycle());
        let next = reconcile_in_snapshot(provider, &mut leases, &mut state, now_ms);
        let protected = state.origin_binding_recovery_protected(now_ms);
        let key = HlsLifecycleEventKey::OriginBindingProtection { proxy_session_id };
        // Publish the timer in the same index/lease/session transaction as the
        // clock change. An older completion cannot cancel or postpone a newer
        // binding's deadline. The lifecycle mutex never waits for HLS state.
        if let Some(next) = next {
            self.lifecycle().schedule(key, next).await;
        } else {
            self.lifecycle().cancel(&key).await;
        }
        protected
    }
}
