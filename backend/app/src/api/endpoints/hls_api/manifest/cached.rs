use super::{
    hls_access_manifest_uses_startup_view, hls_initial_strip_publication_diagnostic, hls_response,
    hls_temporary_resource_unavailable_response, log_hls_initial_strip_publication,
    mark_successful_canonical_manifest_activity, materialize_shared_hls_access_manifest,
    normalize_hls_proxy_public_path_prefix, stripped_tail_segments, touch_pending_manifest_follow_up_window,
    HlsCachedTransientManifestRead, HlsInitialStripPublicationDiagnostic, HlsInitialStripPublicationStatus,
    HlsMaterializedSharedManifest, HLS_MANIFEST_WAIT_POLL_INTERVAL, HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
};
use crate::{
    api::model::AppState,
    model::ConfigInput,
    processing::parser::hls::origin_manifest::HlsManifestWindowPolicy,
    repository::{persist_input_live_bitrate_bps, LiveBitratePersistenceOutcome},
};
use axum::response::IntoResponse;
use log::{debug, error, warn};
use shared::utils::sanitize_sensitive_info;
use std::{sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    derive_hls_lease_manifest_snapshot, hls_committed_manifest_body_for_request,
    hls_should_wait_for_initial_manifest_commit, hls_startup_admission_allows_snapshot, safe_hls_access_lease_id,
    safe_proxy_session_id, HlsAccessLeaseId, HlsAccessLeaseState, HlsBandwidthPersistenceOutcome,
    HlsCachedManifestOptions, HlsCommittedManifestBody, HlsLeaseManifestSnapshot, HlsLeaseManifestSnapshotInput,
    HlsLeaseManifestUriMaterialization, HlsManifestLimitViolation, HlsPublishedTransientResourceIds, HlsSessionHandle,
    ProxySessionId,
};

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
) -> HlsCachedManifestRead {
    let session = session.read().await;
    let now_ms = current_time_millis();
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
        mark_successful_canonical_manifest_activity(app_state, session, current_time_millis()).await;
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
        let bitrate_bps = session_guard.begin_bandwidth_persistence(current_time_millis())?;
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
        current_session.write().await.finish_bandwidth_persistence(bitrate_bps, outcome, current_time_millis());
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
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
    options: HlsCachedManifestOptions,
    bandwidth_learning: HlsRuntimeBandwidthLearningContext<'_>,
) -> Option<axum::response::Response> {
    let started_at = tokio::time::Instant::now();
    let started_at_ms = current_time_millis();
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let Some(publication_guard) = app_state
        .hls
        .proxy
        .prepare_access_lease_manifest_publication(access_lease_id, &proxy_session_id, started_at_ms)
        .await
    else {
        return Some(hls_cached_manifest_temporarily_unavailable());
    };
    let view = HlsCachedManifestViewContext::new(
        &proxy_session_id,
        access_lease_id,
        access_lease_state,
        strip,
        server_path,
        bandwidth_learning,
    );
    loop {
        let cached = read_hls_cached_manifest(session, options, started_at_ms).await;
        if !cached.wait_for_initial_commit {
            let prepared = if let Some(transient) = cached.transient_body {
                let materialized = view.materialize(&transient.body, "transient", transient.window_policy);
                let published_resource_ids = if transient.window_policy.preserves_full_manifest() {
                    transient.published_resource_ids.clone()
                } else {
                    HlsPublishedTransientResourceIds::from_manifest_body(&materialized.body)
                };
                let delivered_at_ms = current_time_millis();
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
                let delivered_at_ms = current_time_millis();
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
                    if wait_for_hls_startup_evidence(started_at, options.wait_timeout).await {
                        continue;
                    }
                    return Some(hls_cached_manifest_temporarily_unavailable());
                }
                let startup_snapshot = snapshot.clone();
                let admission_at_ms = current_time_millis();
                let outcome = app_state
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
                    .await;
                if let Some(snapshot_generation) = outcome.snapshot_generation() {
                    let published_at_ms = current_time_millis();
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
                            startup_snapshot,
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
                return Some(view.finish(app_state, session, materialized, strip_diagnostic).await);
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
