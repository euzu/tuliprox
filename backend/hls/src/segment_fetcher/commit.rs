use super::{
    HlsSegmentFailureObject, HlsSegmentFailureTransition, HlsSegmentWorkerPool, ProxySessionId, ScheduledSegmentRetry,
    SegmentCacheStatus, SegmentEntry, SegmentFetchError, SegmentFetchPolicy, SegmentFetchSnapshot, SegmentRetryWake,
};
use log::{debug, warn};
use std::sync::Arc;
use tokio::sync::Notify;

pub(super) struct SegmentFetchCompletion {
    pub(super) generation_valid: bool,
    pub(super) stale_commit_cleanup_reserved: bool,
    pub(super) notifier: Option<Arc<Notify>>,
    pub(super) scheduled_retry: Option<ScheduledSegmentRetry>,
    pub(super) evidence_changed_for: Option<ProxySessionId>,
}

pub(super) struct SegmentFetchCommit {
    pub(super) content_length: u64,
    pub(super) generation_valid: bool,
}

#[derive(Clone, Copy)]
pub(super) struct SegmentOriginWorkFinish {
    pub(super) generation_valid: bool,
    pub(super) refresh_reservation: bool,
}

pub(super) fn segment_fetch_attempt_matches(entry: &SegmentEntry, snapshot: &SegmentFetchSnapshot) -> bool {
    entry.origin_key == snapshot.origin_key
        && entry.cache_key == snapshot.cache_key
        && matches!(
            entry.status,
            SegmentCacheStatus::Fetching { priority, started_at_ms }
                if priority == snapshot.priority && started_at_ms == snapshot.started_at_ms
        )
}

pub(super) fn segment_fetch_binding_matches(entry: &SegmentEntry, snapshot: &SegmentFetchSnapshot) -> bool {
    segment_fetch_attempt_matches(entry, snapshot)
        && entry.origin_fetch_ref.as_ref() == Some(&snapshot.fetch_ref)
        && entry.encryption == snapshot.encryption
}

impl HlsSegmentWorkerPool {
    pub(super) fn apply_segment_fetch_result(
        &self,
        session: &mut super::super::HlsSession,
        snapshot: &SegmentFetchSnapshot,
        policy: &SegmentFetchPolicy,
        result: Result<SegmentFetchCommit, SegmentFetchError>,
        finished_at_ms: u64,
    ) -> SegmentFetchCompletion {
        session.active_segment_fetches = session.active_segment_fetches.saturating_sub(1);
        let fetch_succeeded = result.is_ok();
        let attempt_matches = session
            .segments
            .get(&snapshot.proxy_seq)
            .is_some_and(|entry| segment_fetch_attempt_matches(entry, snapshot));
        let binding_matches = session
            .segments
            .get(&snapshot.proxy_seq)
            .is_some_and(|entry| segment_fetch_binding_matches(entry, snapshot));
        let generation_valid = binding_matches
            && session.activity.origin_work_generation == snapshot.origin_work_generation
            && result.as_ref().map_or(true, |commit| commit.generation_valid);
        // A successful origin/repair path has already atomically installed its physical object. Keep this fetch's
        // entry in `Fetching` until stale cleanup finishes so no rebound attempt can install newer bytes at the same
        // stable cache path and then lose them to this worker's cleanup.
        let stale_commit_cleanup_reserved = fetch_succeeded && !generation_valid && attempt_matches;
        let capacity_retry = if generation_valid {
            result.as_ref().err().and_then(|error| {
                error.capacity_revision().cloned().zip(error.projected_write_bytes()).map(
                    |(revision, projected_write_bytes)| ScheduledSegmentRetry {
                        wake: SegmentRetryWake::CapacityRevision { revision, projected_write_bytes },
                        priority: snapshot.priority,
                    },
                )
            })
        } else {
            None
        };
        if generation_valid {
            match result {
                Ok(commit) => self.commit_successful_segment_fetch(session, snapshot, &commit, finished_at_ms),
                Err(err) => commit_failed_segment_fetch(session, snapshot, policy, &err, finished_at_ms),
            }
        } else if !stale_commit_cleanup_reserved {
            if let Some(entry) = session
                .segments
                .get_mut(&snapshot.proxy_seq)
                .filter(|entry| segment_fetch_attempt_matches(entry, snapshot))
            {
                entry.status = SegmentCacheStatus::Discovered;
            }
        }
        let scheduled_retry = capacity_retry;
        if generation_valid {
            if let Err(err) = session.render_and_store_manifest(finished_at_ms) {
                if log::log_enabled!(log::Level::Debug) {
                    let identity = super::super::HlsLogIdentity::from_session(session);
                    debug!(
                        "HLS manifest render deferred after segment state change: session={} proxy_session={} resource={} error={err:?}",
                        identity.session(),
                        identity.proxy_session(),
                        snapshot.proxy_seq_log
                    );
                }
            }
        }
        let notifier = (!stale_commit_cleanup_reserved && scheduled_retry.is_none())
            .then(|| session.segment_fetch_notifiers.remove(&snapshot.proxy_seq))
            .flatten();
        let evidence_changed_for = (generation_valid && fetch_succeeded).then(|| session.proxy_session_id.clone());
        SegmentFetchCompletion {
            generation_valid,
            stale_commit_cleanup_reserved,
            notifier,
            scheduled_retry,
            evidence_changed_for,
        }
    }

    fn commit_successful_segment_fetch(
        &self,
        session: &mut super::super::HlsSession,
        snapshot: &SegmentFetchSnapshot,
        commit: &SegmentFetchCommit,
        finished_at_ms: u64,
    ) {
        let content_length = commit.content_length;
        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
            entry.status = SegmentCacheStatus::Ready { content_length, ready_at_ms: finished_at_ms };
            session.advance_media_readiness_generation();
        }
        recompute_unpublished_live_head(session);
        let reset_failures = session.record_successful_segment_fetch();
        self.metrics.record_segment_cached();
        if log::log_enabled!(log::Level::Debug) {
            let identity = super::super::HlsLogIdentity::from_session(session);
            debug!(
                "HLS segment cached: session={} proxy_session={} source=normal resource={} content_length={content_length}",
                identity.session(),
                identity.proxy_session(),
                snapshot.proxy_seq_log
            );
            if let Some(reset_failures) = reset_failures {
                debug!(
                    "HLS segment temporary failure counter reset: session={} proxy_session={} previous_failures={reset_failures}",
                    identity.session(),
                    identity.proxy_session()
                );
            }
        }
    }
}

pub(super) fn commit_failed_segment_fetch(
    session: &mut super::super::HlsSession,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
    error: &SegmentFetchError,
    finished_at_ms: u64,
) {
    let missing_candidate = session.origin_source.archive_reference.is_none()
        && session.published_live_origin_baseline.is_none()
        && session.publishable_origin_head_proxy_seq.is_some_and(|head| {
            snapshot.proxy_seq >= head
                && snapshot.proxy_seq
                    <= session
                        .missing_head_limit
                        .unwrap_or_else(|| head.saturating_add(super::super::startup_policy::STARTUP_WINDOW_SUCCESSORS))
        })
        && error.permanent_status().is_some_and(is_missing_origin_status);
    if !error.is_local_cache_capacity() && !missing_candidate {
        session.origin_control.path_condition =
            super::super::origin_progress::HlsOriginPathCondition::SegmentReadinessFailure;
    }
    let (status, invalidate_queued_origin_work) =
        failed_segment_status(session, snapshot, policy, error, finished_at_ms);
    if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
        entry.status = status;
    }
    if missing_candidate {
        recompute_unpublished_live_head(session);
    }
    if invalidate_queued_origin_work {
        session.invalidate_queued_origin_work();
        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
            entry.status = SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: None };
        }
    }
}

fn is_missing_origin_status(status: axum::http::StatusCode) -> bool {
    matches!(status, axum::http::StatusCode::NOT_FOUND | axum::http::StatusCode::GONE)
}

fn is_missing_origin_segment(status: &SegmentCacheStatus) -> bool {
    matches!(status, SegmentCacheStatus::FailedPermanent { status: Some(status), .. } if is_missing_origin_status(*status))
}

pub(crate) fn recompute_unpublished_live_head(session: &mut super::super::HlsSession) {
    if session.origin_source.archive_reference.is_some() || session.published_live_origin_baseline.is_some() {
        return;
    }
    let Some(head) = session.publishable_origin_head_proxy_seq else { return };
    let Some(tail) = session.publishable_origin_tail_proxy_seq else { return };
    let bound = session
        .missing_head_limit
        .unwrap_or_else(|| head.saturating_add(super::super::startup_policy::STARTUP_WINDOW_SUCCESSORS))
        .min(tail);
    if session.missing_head_limit.is_none()
        && session.segments.range(head..=bound).any(|(_, entry)| is_missing_origin_segment(&entry.status))
    {
        session.missing_head_limit = Some(bound);
    }
    let mut next_head = head;
    for seq in head..=bound {
        let Some(entry) = session.segments.get(&seq) else { break };
        match entry.status {
            ref status if is_missing_origin_segment(status) => {
                let Some(next) = seq.checked_add(1) else { return };
                next_head = next;
            }
            SegmentCacheStatus::Ready { .. } => {
                if session.segments.range(seq..=bound).any(|(_, entry)| is_missing_origin_segment(&entry.status)) {
                    session.origin_control.path_condition =
                        super::super::origin_progress::HlsOriginPathCondition::SegmentReadinessFailure;
                }
                break;
            }
            _ => break,
        }
    }
    if next_head > head {
        session.publishable_origin_head_proxy_seq = Some(next_head);
        debug!(
            "HLS missing live head recovered: proxy_session={} previous_head={head} new_head={next_head}",
            super::super::safe_proxy_session_id(&session.proxy_session_id)
        );
    }
}

fn failed_segment_status(
    session: &mut super::super::HlsSession,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
    error: &SegmentFetchError,
    finished_at_ms: u64,
) -> (SegmentCacheStatus, bool) {
    if error.is_local_cache_capacity() {
        return (
            SegmentCacheStatus::CapacityDeferred { priority: snapshot.priority, deferred_at_ms: finished_at_ms },
            false,
        );
    }
    if !error.retryable_failure() {
        return (
            SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: error.permanent_status() },
            false,
        );
    }
    let threshold = policy.permanent_failure_segment_threshold.max(1);
    let transition = session.record_temporary_segment_fetch_failure(
        finished_at_ms,
        HlsSegmentFailureObject::Normal { proxy_seq: snapshot.proxy_seq, origin_seq: snapshot.origin_seq },
        threshold,
    );
    match transition {
        HlsSegmentFailureTransition::StillRetryable { failures, threshold } => {
            if log::log_enabled!(log::Level::Debug) {
                let identity = super::super::HlsLogIdentity::from_session(session);
                debug!(
                    "HLS segment temporary failure counted: session={} proxy_session={} object={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    snapshot.proxy_seq_log,
                    failures,
                    threshold
                );
            }
            (SegmentCacheStatus::FailedRetryable { failed_at_ms: finished_at_ms, retry_after_ms: 1_000 }, false)
        }
        HlsSegmentFailureTransition::BecamePermanentlyFailed { failures, threshold } => {
            if log::log_enabled!(log::Level::Warn) {
                let identity = super::super::HlsLogIdentity::from_session(session);
                warn!(
                    "HLS segment temporary failure threshold reached: session={} proxy_session={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    failures,
                    threshold
                );
            }
            (SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: None }, true)
        }
    }
}
