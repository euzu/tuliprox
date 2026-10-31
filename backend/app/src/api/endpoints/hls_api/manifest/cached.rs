use super::{
    hls_access_manifest_uses_startup_view, hls_initial_strip_publication_diagnostic, hls_response,
    hls_temporary_resource_unavailable_response, log_hls_initial_strip_publication,
    mark_successful_canonical_manifest_activity, materialize_shared_hls_access_manifest,
    normalize_hls_proxy_public_path_prefix, stripped_tail_segments, touch_pending_manifest_follow_up_window,
    HlsCachedTransientManifestRead, HlsInitialStripPublicationDiagnostic, HlsInitialStripPublicationStatus,
    HlsMaterializedSharedManifest, HLS_MANIFEST_WAIT_POLL_INTERVAL, HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
};
use crate::{
    api::model::{get_custom_stream_response_error_status, is_custom_video_stream_enabled, AppState},
    model::ConfigInput,
    processing::parser::hls::origin_manifest::HlsManifestWindowPolicy,
    repository::{persist_input_live_bitrate_bps, LiveBitratePersistenceOutcome},
};
use axum::response::IntoResponse;
use log::{debug, error, warn};
use shared::utils::sanitize_sensitive_info;
use std::{sync::Arc, time::Duration};
use tuliprox_hls::api::{
    derive_hls_lease_manifest_snapshot, hls_committed_manifest_body_for_request,
    hls_should_wait_for_initial_manifest_commit, hls_startup_admission_allows_snapshot, safe_hls_access_lease_id,
    safe_proxy_session_id, HlsAccessLeaseId, HlsAccessLeaseState, HlsBandwidthPersistenceOutcome,
    HlsCachedManifestOptions, HlsCommittedManifestBody, HlsLeaseManifestSnapshot, HlsLeaseManifestSnapshotInput,
    HlsLeaseManifestUriMaterialization, HlsManifestLimitViolation, HlsPublishedTransientResourceIds, HlsSessionHandle,
    ProxySessionId,
};

type HlsPreparedManifest =
    (HlsMaterializedSharedManifest, HlsLeaseManifestSnapshot, HlsPublishedTransientResourceIds, u64);

#[cfg(test)]
#[derive(Default)]
pub(in crate::api::endpoints::hls_api) struct HlsManifestPublicationBarrier {
    pub prepared: tokio::sync::Notify,
    pub resume: tokio::sync::Notify,
}

#[cfg(test)]
tokio::task_local! {
    pub(in crate::api::endpoints::hls_api) static HLS_MANIFEST_PUBLICATION_BARRIER:
        std::cell::RefCell<Option<Arc<HlsManifestPublicationBarrier>>>;
}

#[cfg(test)]
async fn hls_manifest_publication_checkpoint() {
    if let Ok(Some(barrier)) = HLS_MANIFEST_PUBLICATION_BARRIER.try_with(|slot| slot.borrow_mut().take()) {
        barrier.prepared.notify_one();
        barrier.resume.notified().await;
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_exhausted_startup_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    lease: &tuliprox_hls::api::HlsAccessLease,
) -> Option<axum::response::Response> {
    if lease
        .manifest_start_attempt
        .as_ref()
        .is_some_and(|attempt| attempt.failed() && attempt.provider_failure().is_some())
    {
        return None;
    }
    let reason = tuliprox_hls::api::HlsRuntimeCustomTailReason::ChannelUnavailable;
    // Idle re-entry may have published media from its previous playback. The
    // standalone response must leave those snapshots and revision bindings intact.
    let idle_reentry = lease.state == HlsAccessLeaseState::Idle
        && lease.playback_mode == tuliprox_hls::api::HlsLeasePlaybackMode::Live
        && lease.runtime_policy_revocation.is_none();
    // Retained entries can have been evicted before this new start has even
    // refreshed the origin. Only its settled failure makes exhaustion terminal.
    if idle_reentry && !lease.manifest_start_attempt.as_ref().is_some_and(|attempt| attempt.failed()) {
        return None;
    }
    if !(lease.permits_unpublished_standalone_tail(reason) || idle_reentry) {
        return None;
    }
    let exhausted = {
        let state = session.read().await;
        // A fresh manifest or media preparation may still replace an exhausted
        // retained window. End the start only after that work has settled.
        !state.initial_manifest_commit_work_pending()
            && state.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire) == 0
            && state.start_segments_exhausted()
    };
    if !exhausted {
        return None;
    }
    let fallback_status = if is_custom_video_stream_enabled(&app_state.app_config) {
        axum::http::StatusCode::NOT_FOUND
    } else {
        get_custom_stream_response_error_status(&app_state.app_config)
    };
    Some(
        super::hls_custom_video_manifest_response_for_lease(app_state, lease, reason.video_type(), fallback_status)
            .await,
    )
}

enum HlsLiveWindowPreparation {
    Prepared(HlsPreparedManifest),
    RetainedOriginGeneration { lease: Box<tuliprox_hls::api::HlsAccessLease> },
    NoRenderableWindow { lease: Box<tuliprox_hls::api::HlsAccessLease>, plan: tuliprox_hls::api::HlsLiveWindowPlan },
    Unavailable(&'static str),
}

/// Retained bytes are never reconstructed from the successor's metadata. The
/// publication transaction revalidates this selection, including all bindings.
fn retained_live_window_response(
    lease: &tuliprox_hls::api::HlsAccessLease,
    options: HlsCachedManifestOptions,
    now_ms: u64,
) -> Result<HlsPreparedManifest, &'static str> {
    if lease.state != HlsAccessLeaseState::Activated
        || lease.playback_mode != tuliprox_hls::api::HlsLeasePlaybackMode::Live
    {
        return Err("lease_not_active_live");
    }
    if options.requires_newer_manifest() {
        return Err("fresh_commit_required");
    }
    let previous = lease.last_manifest_snapshot.as_ref().ok_or("no_previous_snapshot")?;
    published_live_window_response(previous, now_ms)
}

fn published_live_window_response(
    previous: &HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> Result<HlsPreparedManifest, &'static str> {
    if now_ms > previous.delivered_at_ms.saturating_add(previous.playlist_duration_ms) {
        return Err("previous_window_expired");
    }
    let published = previous
        .startup_revisions
        .as_ref()
        .and_then(|publication| publication.window_plan.as_ref())
        .ok_or("previous_window_unavailable")?;
    Ok((
        HlsMaterializedSharedManifest { body: published.body.to_string(), mode: "normal", initial_strip_outcome: None },
        previous.clone(),
        HlsPublishedTransientResourceIds::default(),
        now_ms,
    ))
}

#[allow(clippy::too_many_arguments)]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the typed retained selection before the ordinary origin and render checks."
)]
async fn prepare_live_window_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    view: &HlsCachedManifestViewContext<'_>,
    fingerprint: Option<&crate::model::Fingerprint>,
    options: HlsCachedManifestOptions,
    started_at_ms: u64,
) -> HlsLiveWindowPreparation {
    use tuliprox_hls::api::{HlsManifestRenderer, RenderError};

    let now_ms = app_state.hls.proxy.now_ms();
    let Some(lease) =
        app_state.hls.proxy.access_lease_response_snapshot(view.access_lease_id, view.proxy_session_id, now_ms).await
    else {
        return HlsLiveWindowPreparation::Unavailable("lease_unavailable");
    };
    if let Some(lease) = app_state
        .hls
        .proxy
        .retained_live_window_after_origin_change(session, view.access_lease_id, options, started_at_ms, now_ms)
        .await
    {
        return HlsLiveWindowPreparation::RetainedOriginGeneration { lease: Box::new(lease) };
    }
    let (prepared, queued) = {
        let mut state = session.write().await;
        state.live_window_strip = app_state.hls.proxy.strip();
        if !tuliprox_hls::api::hls_live_window_origin_allowed_for_request(&state, options, started_at_ms, now_ms) {
            return HlsLiveWindowPreparation::Unavailable("origin_freshness_or_identity");
        }
        // Keep ordinary forward session commits current when their own window
        // is ready. A lease's independent entry never depends on this attempt.
        let _ = state.render_and_store_manifest(now_ms);
        let Some(desired) = state.plan_live_window(tuliprox_hls::api::HlsLiveWindowView::for_lease(&lease)) else {
            return HlsLiveWindowPreparation::Unavailable("no_window_plan");
        };
        if let Some(previous) = lease
            .last_manifest_snapshot
            .as_ref()
            .filter(|previous| desired.visible.first().is_some_and(|first| *first < previous.first_proxy_seq))
        {
            return match published_live_window_response(previous, now_ms) {
                Ok(prepared) => HlsLiveWindowPreparation::Prepared(prepared),
                Err(reason) => HlsLiveWindowPreparation::Unavailable(reason),
            };
        }
        let Some(plan) = state.plan_live_window_for_lease(&lease) else {
            return HlsLiveWindowPreparation::Unavailable("no_lease_window_plan");
        };
        let backpressure = app_state.hls.proxy.segment_worker_pool().classify_backpressure_for_session(&state);
        let report = state.queue_manifest_fetch_candidates(now_ms, backpressure.allows_prefetch());
        let queued = report.render_window_queued + report.prefetch_queued > 0;
        let prepared = match HlsManifestRenderer::render_live_window(&state, &plan, now_ms) {
            Err(RenderError::NoRenderableWindow) => {
                HlsLiveWindowPreparation::NoRenderableWindow { lease: Box::new(lease), plan }
            }
            Err(_) => HlsLiveWindowPreparation::Unavailable("render_invalid_state"),
            Ok(rendered) => {
                let materialized = HlsMaterializedSharedManifest {
                    body: super::materialize_hls_access_manifest(
                        &rendered.body,
                        view.access_lease_id,
                        view.server_path,
                    ),
                    mode: "normal",
                    initial_strip_outcome: hls_access_manifest_uses_startup_view(lease.state).then_some(
                        tuliprox_hls::initial_strip::HlsInitialStripOutcome::Applied {
                            mode: match view.strip.mode {
                                shared::model::HlsStripMode::Segments => "segments",
                                shared::model::HlsStripMode::Seconds => "seconds",
                            },
                            configured: view.strip.value,
                            effective: plan.effective_strip_segments,
                            visible_segments: plan.visible.len(),
                        },
                    ),
                };
                match observe_hls_lease_manifest_snapshot_derivation(
                    app_state,
                    view.proxy_session_id,
                    view.access_lease_id,
                    derive_hls_lease_manifest_snapshot(
                        &HlsLeaseManifestSnapshotInput::FastStartWindow {
                            session: &state,
                            plan: &plan,
                            rendered: &rendered,
                            materialized_body: &materialized.body,
                        },
                        now_ms,
                    ),
                ) {
                    Ok(Some(snapshot)) => HlsLiveWindowPreparation::Prepared((
                        materialized,
                        snapshot,
                        HlsPublishedTransientResourceIds::default(),
                        now_ms,
                    )),
                    _ => HlsLiveWindowPreparation::Unavailable("snapshot_derivation_failed"),
                }
            }
        };
        (prepared, queued)
    };
    if queued {
        if let Some(fingerprint) = fingerprint {
            let context =
                super::super::build_hls_segment_fetch_context(app_state, session, None, fingerprint, None).await;
            let worker = Arc::clone(app_state.hls.proxy.segment_worker_pool());
            let hls_proxy = Arc::clone(&app_state.hls.proxy);
            tokio::spawn(async move {
                worker.wake_scheduler(context, hls_proxy.now_ms()).await;
            });
        }
    }
    prepared
}

pub(in crate::api::endpoints::hls_api) struct HlsCachedManifestRead {
    pub(in crate::api::endpoints::hls_api) transient_body: Option<HlsCachedTransientManifestRead>,
    pub(in crate::api::endpoints::hls_api) rendered_body: Option<String>,
    pub(in crate::api::endpoints::hls_api) should_wait: bool,
    pub(in crate::api::endpoints::hls_api) wait_for_initial_commit: bool,
}

pub(in crate::api::endpoints::hls_api) async fn read_hls_cached_manifest(
    session: &HlsSessionHandle,
    options: HlsCachedManifestOptions,
    started_at_ms: u64,
    now_ms: u64,
) -> HlsCachedManifestRead {
    let session = session.read().await;
    let should_wait = session.initial_manifest_commit_work_pending();
    let committed_body = hls_committed_manifest_body_for_request(&session, options, started_at_ms, now_ms);
    let (transient_body, rendered_body) = match committed_body {
        Some(HlsCommittedManifestBody::Transient(body)) => (
            session.transient.last_manifest_template().zip(session.transient.last_manifest_commit_identity()).map(
                |(template, source_commit_identity)| HlsCachedTransientManifestRead {
                    body,
                    template,
                    source_commit_identity,
                    window_policy: session.transient.last_manifest_window_policy(),
                    finalized_manifest_generation: session.transient.current_finalized_manifest_generation(),
                    published_resource_ids: session.transient.last_manifest_published_resource_ids(),
                },
            ),
            None,
        ),
        Some(HlsCommittedManifestBody::Normal(body)) => (None, Some(body)),
        None => (None, None),
    };
    let wait_for_initial_commit = hls_should_wait_for_initial_manifest_commit(
        &session,
        transient_body.is_some() || rendered_body.is_some(),
        should_wait,
        options,
        now_ms,
    );
    HlsCachedManifestRead { transient_body, rendered_body, should_wait, wait_for_initial_commit }
}

pub(in crate::api::endpoints::hls_api) struct HlsCachedManifestViewContext<'a> {
    pub(in crate::api::endpoints::hls_api) proxy_session_id: &'a ProxySessionId,
    pub(in crate::api::endpoints::hls_api) access_lease_id: &'a HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api) access_lease_state: HlsAccessLeaseState,
    pub(in crate::api::endpoints::hls_api) strip: &'a crate::model::StripConfig,
    pub(in crate::api::endpoints::hls_api) server_path: Option<&'a str>,
    pub(in crate::api::endpoints::hls_api) bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
}

#[derive(Clone, Copy)]
pub(in crate::api::endpoints::hls_api) enum HlsRuntimeBandwidthLearningContext<'a> {
    Disabled,
    Eligible(&'a ConfigInput),
}

impl HlsCachedManifestViewContext<'_> {
    pub(in crate::api::endpoints::hls_api) fn new<'a>(
        proxy_session_id: &'a ProxySessionId,
        access_lease_id: &'a HlsAccessLeaseId,
        access_lease_state: HlsAccessLeaseState,
        strip: &'a crate::model::StripConfig,
        server_path: Option<&'a str>,
        bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
    ) -> HlsCachedManifestViewContext<'a> {
        HlsCachedManifestViewContext {
            proxy_session_id,
            access_lease_id,
            access_lease_state,
            strip,
            server_path,
            bandwidth_learning,
        }
    }

    pub(in crate::api::endpoints::hls_api) fn materialize(
        &self,
        body: &str,
        mode: &'static str,
        window_policy: HlsManifestWindowPolicy,
    ) -> HlsMaterializedSharedManifest {
        materialize_shared_hls_access_manifest(
            body,
            self.access_lease_id,
            self.access_lease_state,
            self.strip,
            window_policy,
            mode,
            self.server_path,
        )
    }

    pub(in crate::api::endpoints::hls_api) async fn finish(
        &self,
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        materialized: HlsMaterializedSharedManifest,
        strip_diagnostic: HlsInitialStripPublicationDiagnostic,
    ) -> axum::response::Response {
        touch_pending_manifest_follow_up_window(app_state, session, self.access_lease_id, self.access_lease_state)
            .await;
        drop(spawn_hls_runtime_bandwidth_persistence(app_state, session, self.bandwidth_learning));
        mark_successful_canonical_manifest_activity(app_state, session, app_state.hls.proxy.now_ms()).await;
        log_hls_initial_strip_publication(self.proxy_session_id, self.access_lease_id, strip_diagnostic);
        hls_response(materialized.body).into_response()
    }
}

pub(in crate::api::endpoints::hls_api) fn spawn_hls_runtime_bandwidth_persistence(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    context: HlsRuntimeBandwidthLearningContext<'_>,
) -> Option<tokio::task::JoinHandle<()>> {
    let input = match context {
        HlsRuntimeBandwidthLearningContext::Disabled => return None,
        HlsRuntimeBandwidthLearningContext::Eligible(input) => input.clone(),
    };
    let (bitrate_bps, proxy_session_id, stream_ref) = {
        let Ok(mut session_guard) = session.try_write() else {
            return None;
        };
        let bitrate_bps = session_guard.begin_bandwidth_persistence(app_state.hls.proxy.now_ms())?;
        (bitrate_bps, session_guard.proxy_session_id.clone(), session_guard.origin_source.stream_ref.clone())
    };
    let app_config = Arc::clone(&app_state.app_config);
    let hls_proxy = Arc::clone(&app_state.hls.proxy);
    let session = Arc::clone(session);

    Some(tokio::spawn(async move {
        let outcome = match persist_input_live_bitrate_bps(&app_config, &input, &stream_ref, bitrate_bps).await {
            Ok(repository_outcome) => hls_bandwidth_persistence_outcome(repository_outcome, &proxy_session_id),
            Err(err) => {
                error!(
                    "HLS runtime bandwidth persistence failed: proxy_session={} error={}",
                    safe_proxy_session_id(&proxy_session_id),
                    sanitize_sensitive_info(&err.to_string())
                );
                HlsBandwidthPersistenceOutcome::RetryAfter
            }
        };
        let Some(current_session) = hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await else {
            return;
        };
        if !Arc::ptr_eq(&current_session, &session) {
            return;
        }
        current_session.write().await.finish_bandwidth_persistence(bitrate_bps, outcome, hls_proxy.now_ms());
    }))
}

pub(in crate::api::endpoints::hls_api) fn hls_bandwidth_persistence_outcome(
    repository_outcome: LiveBitratePersistenceOutcome,
    proxy_session_id: &ProxySessionId,
) -> HlsBandwidthPersistenceOutcome {
    match repository_outcome {
        LiveBitratePersistenceOutcome::Updated | LiveBitratePersistenceOutcome::AlreadyEqualOrHigher => {
            HlsBandwidthPersistenceOutcome::Persisted
        }
        LiveBitratePersistenceOutcome::MissingDatabase => {
            debug!(
                "HLS runtime bandwidth persistence deferred: proxy_session={} reason=missing_database",
                safe_proxy_session_id(proxy_session_id)
            );
            HlsBandwidthPersistenceOutcome::RetryAfter
        }
        LiveBitratePersistenceOutcome::MissingStreamItem => {
            debug!(
                "HLS runtime bandwidth persistence deferred: proxy_session={} reason=missing_stream_item",
                safe_proxy_session_id(proxy_session_id)
            );
            HlsBandwidthPersistenceOutcome::RetryAfter
        }
        LiveBitratePersistenceOutcome::PermanentlyInapplicable(reason) => {
            debug!(
                "HLS runtime bandwidth persistence skipped: proxy_session={} reason={}",
                safe_proxy_session_id(proxy_session_id),
                reason.log_label()
            );
            HlsBandwidthPersistenceOutcome::PermanentlyInapplicable
        }
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_cached_manifest_temporarily_unavailable() -> axum::response::Response {
    hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
}

pub(in crate::api::endpoints::hls_api) fn observe_hls_lease_manifest_snapshot_derivation(
    app_state: &AppState,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    derivation: Result<Option<HlsLeaseManifestSnapshot>, HlsManifestLimitViolation>,
) -> Result<Option<HlsLeaseManifestSnapshot>, ()> {
    match derivation {
        Ok(snapshot) => {
            if let Some(snapshot) = snapshot.as_ref() {
                app_state.hls.proxy.metrics().record_lease_snapshot_segments(snapshot.visible_segments.len());
            }
            Ok(snapshot)
        }
        Err(violation) => {
            app_state.hls.proxy.metrics().record_manifest_limit_rejection();
            warn!(
                "HLS lease manifest snapshot rejected: proxy_session={} lease={} reason=manifest-representation-limit kind={} actual={} limit={}",
                safe_proxy_session_id(proxy_session_id),
                safe_hls_access_lease_id(access_lease_id),
                violation.kind.as_log_value(),
                violation.actual,
                violation.limit
            );
            Err(())
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn try_hls_cached_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: Option<&crate::model::Fingerprint>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
    options: HlsCachedManifestOptions,
    bandwidth_learning: HlsRuntimeBandwidthLearningContext<'_>,
) -> Option<axum::response::Response> {
    let started_at = tokio::time::Instant::now();
    let started_at_ms = app_state.hls.proxy.now_ms();
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let Some(publication_guard) = app_state
        .hls
        .proxy
        .prepare_access_lease_manifest_publication(access_lease_id, &proxy_session_id, started_at_ms)
        .await
    else {
        // A terminal commit or start failure can close live publication before
        // preparation. Resolve the current lease as in the publication loop.
        let lease = app_state
            .hls
            .proxy
            .access_lease_response_snapshot(access_lease_id, &proxy_session_id, app_state.hls.proxy.now_ms())
            .await;
        if let Some(response) = lease.as_ref().and_then(|lease| {
            super::hls_terminal_playback_response(app_state, lease, &proxy_session_id, access_lease_id)
        }) {
            return Some(response);
        }
        if lease.as_ref().is_some_and(|lease| {
            lease.state == HlsAccessLeaseState::Pending
                && lease.last_manifest_snapshot.is_none()
                && lease.manifest_start_attempt.as_ref().is_some_and(|attempt| attempt.failed())
        }) {
            return None;
        }
        debug!(
            "HLS live window unavailable: proxy_session={} lease={} status=503 reason=publication_guard_unavailable",
            safe_proxy_session_id(&proxy_session_id),
            safe_hls_access_lease_id(access_lease_id),
        );
        return Some(hls_cached_manifest_temporarily_unavailable());
    };
    let mut view = HlsCachedManifestViewContext::new(
        &proxy_session_id,
        access_lease_id,
        access_lease_state,
        strip,
        server_path,
        bandwidth_learning,
    );
    let mut last_wait_reason = None;
    loop {
        let lease = app_state
            .hls
            .proxy
            .access_lease_response_snapshot(access_lease_id, &proxy_session_id, app_state.hls.proxy.now_ms())
            .await?;
        let access_lease_state = view.access_lease_state;
        if let Some(response) =
            super::hls_terminal_playback_response(app_state, &lease, &proxy_session_id, access_lease_id)
        {
            return Some(response);
        }
        if let Some(response) = hls_exhausted_startup_response(app_state, session, &lease).await {
            return Some(response);
        }
        if lease.manifest_start_attempt.as_ref().is_some_and(|attempt| attempt.failed()) {
            return None;
        }
        let (entry_allowed, refresh_in_flight) = {
            let state = session.read().await;
            (state.live_entry_allowed_for_lease(&lease), state.origin_refresh.in_flight)
        };
        if !entry_allowed {
            if refresh_in_flight && wait_for_hls_startup_evidence(started_at, options.wait_timeout).await {
                continue;
            }
            return None;
        }
        let cached = read_hls_cached_manifest(session, options, started_at_ms, app_state.hls.proxy.now_ms()).await;
        let fast_start = session.read().await.uses_fast_start_live_window();
        if cached.wait_for_initial_commit || (cached.rendered_body.is_none() && cached.transient_body.is_none()) {
            log_hls_manifest_wait_transition(&mut last_wait_reason, "fresh_render_required", session, access_lease_id)
                .await;
        }
        if fast_start || !cached.wait_for_initial_commit {
            let mut retained_plan = None;
            let mut retained_origin_generation = false;
            let prepared = if fast_start {
                let preparation =
                    prepare_live_window_response(app_state, session, &view, fingerprint, options, started_at_ms).await;
                let selection = match preparation {
                    HlsLiveWindowPreparation::Prepared(prepared) => Ok(prepared),
                    HlsLiveWindowPreparation::RetainedOriginGeneration { lease } => {
                        retained_origin_generation = true;
                        retained_live_window_response(&lease, options, app_state.hls.proxy.now_ms())
                    }
                    HlsLiveWindowPreparation::NoRenderableWindow { lease, plan } => {
                        retained_plan = Some(plan);
                        retained_live_window_response(&lease, options, app_state.hls.proxy.now_ms())
                    }
                    HlsLiveWindowPreparation::Unavailable(reason) => Err(reason),
                };
                match selection {
                    Ok(prepared) => Some(prepared),
                    Err(reason) => {
                        if wait_for_hls_startup_evidence(started_at, options.wait_timeout).await {
                            continue;
                        }
                        if access_lease_state == HlsAccessLeaseState::Pending {
                            return None;
                        }
                        debug!(
                            "HLS live window unavailable: proxy_session={} lease={} status=503 reason={reason}",
                            safe_proxy_session_id(&proxy_session_id),
                            safe_hls_access_lease_id(access_lease_id),
                        );
                        return Some(hls_cached_manifest_temporarily_unavailable());
                    }
                }
            } else if let Some(transient) = cached.transient_body {
                let materialized = view.materialize(&transient.body, "transient", transient.window_policy);
                let published_resource_ids = if transient.window_policy.preserves_full_manifest() {
                    transient.published_resource_ids.clone()
                } else {
                    HlsPublishedTransientResourceIds::from_manifest_body(&materialized.body)
                };
                let delivered_at_ms = app_state.hls.proxy.now_ms();
                let snapshot_input = if transient.window_policy.preserves_full_manifest() {
                    HlsLeaseManifestSnapshotInput::TransientPassthroughTemplate {
                        template: &transient.template,
                        source_commit_identity: transient.source_commit_identity,
                        uri_materialization: HlsLeaseManifestUriMaterialization::new(
                            access_lease_id,
                            normalize_hls_proxy_public_path_prefix(server_path).map(Arc::from),
                        ),
                        finalized_manifest_generation: transient.finalized_manifest_generation,
                    }
                } else {
                    HlsLeaseManifestSnapshotInput::TransientPassthrough {
                        materialized_body: &materialized.body,
                        source_commit_identity: transient.source_commit_identity,
                        finalized_manifest_generation: transient.finalized_manifest_generation,
                    }
                };
                let snapshot = observe_hls_lease_manifest_snapshot_derivation(
                    app_state,
                    &proxy_session_id,
                    access_lease_id,
                    derive_hls_lease_manifest_snapshot(&snapshot_input, delivered_at_ms),
                );
                let Ok(snapshot) = snapshot else {
                    return Some(hls_cached_manifest_temporarily_unavailable());
                };
                let Some(snapshot) = snapshot else {
                    return Some(hls_cached_manifest_temporarily_unavailable());
                };
                Some((materialized, snapshot, published_resource_ids, delivered_at_ms))
            } else if let Some(body) = cached.rendered_body {
                let materialized = view.materialize(&body, "normal", HlsManifestWindowPolicy::ApplyLiveWindow);
                let published_resource_ids = HlsPublishedTransientResourceIds::from_manifest_body(&materialized.body);
                let delivered_at_ms = app_state.hls.proxy.now_ms();
                let snapshot = {
                    let session = session.read().await;
                    observe_hls_lease_manifest_snapshot_derivation(
                        app_state,
                        &proxy_session_id,
                        access_lease_id,
                        derive_hls_lease_manifest_snapshot(
                            &HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
                                session: &session,
                                committed_body: &body,
                                materialized_body: &materialized.body,
                                stripped_tail_segments: stripped_tail_segments(&materialized),
                            },
                            delivered_at_ms,
                        ),
                    )
                };
                let Ok(snapshot) = snapshot else {
                    return Some(hls_cached_manifest_temporarily_unavailable());
                };
                let Some(snapshot) = snapshot else {
                    if access_lease_state != HlsAccessLeaseState::Pending {
                        return None;
                    }
                    log_hls_manifest_wait_transition(
                        &mut last_wait_reason,
                        "missing_revision_or_head_readiness",
                        session,
                        access_lease_id,
                    )
                    .await;
                    if wait_for_hls_startup_evidence(started_at, options.wait_timeout).await {
                        continue;
                    }
                    return Some(hls_cached_manifest_temporarily_unavailable());
                };
                Some((materialized, snapshot, published_resource_ids, delivered_at_ms))
            } else {
                None
            };
            if let Some((materialized, snapshot, published_resource_ids, delivered_at_ms)) = prepared {
                if access_lease_state == HlsAccessLeaseState::Pending
                    && !hls_startup_admission_allows_snapshot(&app_state.hls_ctx(), session, &snapshot, delivered_at_ms)
                        .await
                {
                    log_hls_manifest_wait_transition(
                        &mut last_wait_reason,
                        "admission_rejected",
                        session,
                        access_lease_id,
                    )
                    .await;
                    if wait_for_hls_startup_evidence(started_at, options.wait_timeout).await {
                        continue;
                    }
                    return Some(hls_cached_manifest_temporarily_unavailable());
                }
                let startup_snapshot = snapshot.clone();
                #[cfg(test)]
                hls_manifest_publication_checkpoint().await;
                let admission_at_ms = app_state.hls.proxy.now_ms();
                let outcome = if retained_plan.is_some() || retained_origin_generation {
                    app_state
                        .hls
                        .proxy
                        .republish_access_lease_live_window(
                            session,
                            access_lease_id,
                            &proxy_session_id,
                            publication_guard,
                            snapshot,
                            options,
                            started_at_ms,
                            admission_at_ms,
                        )
                        .await
                } else {
                    app_state
                        .hls
                        .proxy
                        .commit_access_lease_manifest_publication_with_resources(
                            access_lease_id,
                            &proxy_session_id,
                            publication_guard,
                            snapshot,
                            published_resource_ids,
                            admission_at_ms,
                        )
                        .await
                };
                if fast_start && !outcome.is_committed() {
                    let current = app_state
                        .hls
                        .proxy
                        .access_lease_response_snapshot(
                            access_lease_id,
                            &proxy_session_id,
                            app_state.hls.proxy.now_ms(),
                        )
                        .await;
                    if let Some(current) = current.as_ref() {
                        if let Some(response) = super::hls_terminal_playback_response(
                            app_state,
                            current,
                            &proxy_session_id,
                            access_lease_id,
                        ) {
                            return Some(response);
                        }
                    }
                    if outcome
                        == tuliprox_hls::api::HlsLeaseManifestPublicationOutcome::Rejected(
                            tuliprox_hls::api::HlsLeaseManifestPublicationRejectReason::SnapshotChanged,
                        )
                    {
                        if let Some(current) = current {
                            view.access_lease_state = current.state;
                        }
                        continue;
                    }
                    if wait_for_hls_startup_evidence(started_at, options.wait_timeout).await {
                        continue;
                    }
                    debug!(
                        "HLS live window unavailable: proxy_session={} lease={} status=503 reason=publication_rejected outcome={outcome:?}",
                        safe_proxy_session_id(&proxy_session_id), safe_hls_access_lease_id(access_lease_id),
                    );
                    return Some(hls_cached_manifest_temporarily_unavailable());
                }
                if let Some(plan) = retained_plan {
                    debug!(
                        "HLS live window retained: proxy_session={} lease={} reason=no_renderable_window status=200 previous_first={} previous_last={} desired_first={:?} desired_last={:?} remaining_validity_ms={}",
                        safe_proxy_session_id(&proxy_session_id), safe_hls_access_lease_id(access_lease_id),
                        startup_snapshot.first_proxy_seq, startup_snapshot.last_proxy_seq,
                        plan.visible.first(), plan.visible.last(),
                        startup_snapshot.delivered_at_ms.saturating_add(startup_snapshot.playlist_duration_ms).saturating_sub(app_state.hls.proxy.now_ms()),
                    );
                }
                if retained_origin_generation {
                    debug!(
                        "HLS live window retained: proxy_session={} lease={} reason=origin_generation status=200",
                        safe_proxy_session_id(&proxy_session_id),
                        safe_hls_access_lease_id(access_lease_id),
                    );
                }
                if let Some(snapshot_generation) = outcome.snapshot_generation() {
                    let published_at_ms = app_state.hls.proxy.now_ms();
                    let first_startup_publication =
                        app_state.hls.proxy.startup_observability().record_manifest_publication(
                            access_lease_id,
                            snapshot_generation,
                            admission_at_ms,
                            published_at_ms,
                            Arc::from(startup_snapshot.visible_proxy_seqs().collect::<Vec<_>>()),
                        );
                    if first_startup_publication && hls_access_manifest_uses_startup_view(access_lease_state) {
                        app_state.hls.proxy.spawn_access_lease_repair_prewarm(
                            Arc::clone(session),
                            access_lease_id.clone(),
                            startup_snapshot.clone(),
                            snapshot_generation,
                        );
                    }
                }
                let publication_status = if outcome.is_committed() {
                    HlsInitialStripPublicationStatus::Committed
                } else {
                    HlsInitialStripPublicationStatus::NotCommitted
                };
                let Some(strip_diagnostic) =
                    hls_initial_strip_publication_diagnostic(publication_status, access_lease_state, &materialized)
                else {
                    return Some(hls_cached_manifest_temporarily_unavailable());
                };
                let response = view.finish(app_state, session, materialized, strip_diagnostic).await;
                if fast_start {
                    let current_session =
                        app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await?;
                    if !Arc::ptr_eq(&current_session, session) {
                        return Some(hls_cached_manifest_temporarily_unavailable());
                    }
                    let current = app_state
                        .hls
                        .proxy
                        .access_lease_response_snapshot(
                            access_lease_id,
                            &proxy_session_id,
                            app_state.hls.proxy.now_ms(),
                        )
                        .await?;
                    if let Some(terminal) =
                        super::hls_terminal_playback_response(app_state, &current, &proxy_session_id, access_lease_id)
                    {
                        return Some(terminal);
                    }
                    if current.issued_at_ms != lease.issued_at_ms {
                        return Some(hls_cached_manifest_temporarily_unavailable());
                    }
                    let current_generation = current.last_manifest_snapshot.as_ref()?.snapshot_generation;
                    if current_generation != outcome.snapshot_generation()? {
                        let mut expected = startup_snapshot;
                        expected.snapshot_generation = current_generation;
                        if current.last_manifest_snapshot.as_ref() != Some(&expected) {
                            // Another view won while activity was recorded.
                            continue;
                        }
                    }
                }
                return Some(response);
            }
        }
        if options.wait_timeout.is_zero() || !cached.should_wait || started_at.elapsed() >= options.wait_timeout {
            return None;
        }
        let remaining = options.wait_timeout.saturating_sub(started_at.elapsed());
        tokio::time::sleep(remaining.min(HLS_MANIFEST_WAIT_POLL_INTERVAL)).await;
    }
}

pub(in crate::api::endpoints::hls_api) async fn wait_for_hls_startup_evidence(
    started_at: tokio::time::Instant,
    wait_timeout: Duration,
) -> bool {
    let elapsed = started_at.elapsed();
    if wait_timeout.is_zero() || elapsed >= wait_timeout {
        return false;
    }
    tokio::time::sleep(wait_timeout.saturating_sub(elapsed).min(Duration::from_millis(25))).await;
    true
}

/// Per-request state prevents a 25-ms polling loop from repeating diagnostics.
async fn log_hls_manifest_wait_transition(
    previous: &mut Option<(&'static str, bool, bool)>,
    reason: &'static str,
    session: &HlsSessionHandle,
    lease_id: &HlsAccessLeaseId,
) {
    let session = session.read().await;
    let degraded = session.origin_control.path_condition.is_degraded();
    let head = session.publishable_origin_head_proxy_seq.and_then(|seq| session.startup.as_ref()?.revisions.get(&seq));
    let head_ready = head.is_some_and(|head| head.revision().is_startup_head_ready(session.startup_mode()));
    let state = (reason, degraded, head_ready);
    if *previous == Some(state) {
        return;
    }
    *previous = Some(state);
    debug!(
        "HLS playlist wait: lease={} proxy_session={} reason={reason} degraded={degraded} head_ready={head_ready} origin_generation={} readiness_generation={} head_revision={:?} head_prefix={}",
        safe_hls_access_lease_id(lease_id), safe_proxy_session_id(&session.proxy_session_id),
        session.activity.origin_work_generation, session.activity.media_readiness_generation,
        head.map(|head| head.revision().id()),
        head.map_or(0, |head| head.revision().prefix_available.load(std::sync::atomic::Ordering::Acquire)),
    );
}
