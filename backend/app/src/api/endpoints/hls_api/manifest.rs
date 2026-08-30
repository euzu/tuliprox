#![allow(clippy::wildcard_imports)]
use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct HlsProxyTerminalSegmentPathParams {
    pub(super) proxy_session_id: String,
    pub(super) hls_access_lease_id: String,
    pub(super) generation: String,
    pub(super) terminal_file: String,
}

pub(super) fn hls_custom_video_type_for_failure_reason(reason: ConnectFailureReason) -> CustomVideoStreamType {
    match reason {
        ConnectFailureReason::UserAccountExpired => CustomVideoStreamType::UserAccountExpired,
        ConnectFailureReason::UserConnectionsExhausted => CustomVideoStreamType::UserConnectionsExhausted,
        ConnectFailureReason::ProviderConnectionsExhausted => CustomVideoStreamType::ProviderConnectionsExhausted,
        ConnectFailureReason::Preempted => CustomVideoStreamType::LowPriorityPreempted,
        ConnectFailureReason::Provisioning => CustomVideoStreamType::Provisioning,
        ConnectFailureReason::SessionExpired => CustomVideoStreamType::HlsSessionOrLeaseExpired,
        ConnectFailureReason::ProviderError
        | ConnectFailureReason::ProviderClosed
        | ConnectFailureReason::ChannelUnavailable => CustomVideoStreamType::ChannelUnavailable,
    }
}

pub(crate) async fn hls_custom_video_manifest_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    hls_custom_video_manifest_response_with_virtual_id(app_state, user, video_type, fallback_status, None).await
}

pub(crate) async fn hls_admission_failure_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    stream_channel: StreamChannel,
    provider_name: Arc<str>,
    req_headers: &HeaderMap,
    reason: ConnectFailureReason,
) -> axum::response::Response {
    record_connect_failed_attempt(ConnectFailedAttempt {
        app_state,
        fingerprint,
        user,
        stream_channel,
        provider_name,
        req_headers,
        reason,
        failure_stage: FailureStage::Admission,
    });
    hls_custom_video_manifest_response(
        app_state,
        user,
        hls_custom_video_type_for_failure_reason(reason),
        StatusCode::FORBIDDEN,
    )
    .await
}

pub(super) fn apply_hls_proxy_public_path_prefix(hls_content: String, server_path: Option<&str>) -> String {
    let Some(path_prefix) = normalize_hls_proxy_public_path_prefix(server_path) else {
        return hls_content;
    };

    let uri_attr_prefix = format!("URI=\"{path_prefix}/hls/shared/live/");
    let hls_content = hls_content.replace("URI=\"/hls/shared/live/", &uri_attr_prefix);
    if hls_content.is_empty() {
        return hls_content;
    }
    let mut prefixed = String::with_capacity(hls_content.len().saturating_add(path_prefix.len().saturating_mul(4)));

    for part in hls_content.split_inclusive('\n') {
        let (line, line_ending) = split_hls_line_ending(part);
        if line.starts_with("/hls/shared/live/") {
            prefixed.push_str(&path_prefix);
        }
        prefixed.push_str(line);
        prefixed.push_str(line_ending);
    }

    prefixed
}

pub(super) fn normalize_hls_proxy_public_path_prefix(server_path: Option<&str>) -> Option<String> {
    let path = server_path?.trim().trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(format!("/{path}"))
}

pub(super) fn split_hls_line_ending(part: &str) -> (&str, &str) {
    if let Some(line) = part.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = part.strip_suffix('\n') {
        (line, "\n")
    } else {
        (part, "")
    }
}

pub(super) fn materialize_hls_access_manifest(
    hls_content: &str,
    lease_id: &HlsAccessLeaseId,
    server_path: Option<&str>,
) -> String {
    let hls_content = hls_content.replace(HLS_ACCESS_LEASE_ID_PLACEHOLDER, &lease_id.0);
    apply_hls_proxy_public_path_prefix(hls_content, server_path)
}

pub(super) fn hls_access_manifest_uses_startup_view(lease_state: HlsAccessLeaseState) -> bool {
    matches!(lease_state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Idle)
}

pub(super) fn materialize_shared_hls_access_manifest(
    hls_content: &str,
    lease_id: &HlsAccessLeaseId,
    lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    window_policy: HlsManifestWindowPolicy,
    mode: &'static str,
    server_path: Option<&str>,
) -> HlsMaterializedSharedManifest {
    let (response_body, initial_strip_outcome) = if hls_access_manifest_uses_startup_view(lease_state) {
        let view = materialize_initial_hls_strip_view(hls_content, strip, window_policy);
        (view.body, Some(view.outcome))
    } else {
        (Cow::Borrowed(hls_content), None)
    };
    HlsMaterializedSharedManifest {
        body: materialize_hls_access_manifest(&response_body, lease_id, server_path),
        mode,
        initial_strip_outcome,
    }
}

pub(super) struct HlsMaterializedSharedManifest {
    pub(super) body: String,
    pub(super) mode: &'static str,
    pub(super) initial_strip_outcome: Option<HlsInitialStripOutcome>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsInitialStripLeaseSkipReason {
    LeaseActivated,
    LeaseNotStartupView,
}

impl HlsInitialStripLeaseSkipReason {
    pub(super) const fn as_log_reason(self) -> &'static str {
        match self {
            Self::LeaseActivated => "lease-activated",
            Self::LeaseNotStartupView => "lease-not-startup-view",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsInitialStripPublicationDiagnostic {
    Applied { mode: &'static str, strip_mode: &'static str, configured: u64, effective: usize, visible_segments: usize },
    Skipped { mode: &'static str, reason: HlsInitialStripSkipReason, visible_segments: usize },
    SkippedForLeaseState { mode: &'static str, reason: HlsInitialStripLeaseSkipReason },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsInitialStripPublicationStatus {
    NotCommitted,
    Committed,
}

pub(super) fn hls_initial_strip_publication_diagnostic(
    publication_status: HlsInitialStripPublicationStatus,
    lease_state: HlsAccessLeaseState,
    materialized: &HlsMaterializedSharedManifest,
) -> Option<HlsInitialStripPublicationDiagnostic> {
    match publication_status {
        HlsInitialStripPublicationStatus::NotCommitted => return None,
        HlsInitialStripPublicationStatus::Committed => {}
    }
    Some(match &materialized.initial_strip_outcome {
        Some(HlsInitialStripOutcome::Applied { mode: strip_mode, configured, effective, visible_segments }) => {
            HlsInitialStripPublicationDiagnostic::Applied {
                mode: materialized.mode,
                strip_mode,
                configured: *configured,
                effective: *effective,
                visible_segments: *visible_segments,
            }
        }
        Some(HlsInitialStripOutcome::Skipped { reason, visible_segments }) => {
            HlsInitialStripPublicationDiagnostic::Skipped {
                mode: materialized.mode,
                reason: *reason,
                visible_segments: *visible_segments,
            }
        }
        None => HlsInitialStripPublicationDiagnostic::SkippedForLeaseState {
            mode: materialized.mode,
            reason: if lease_state == HlsAccessLeaseState::Activated {
                HlsInitialStripLeaseSkipReason::LeaseActivated
            } else {
                HlsInitialStripLeaseSkipReason::LeaseNotStartupView
            },
        },
    })
}

pub(super) fn hls_entry_master_playlist_response(
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    bandwidth: HlsMasterBandwidth,
    server_path: Option<&str>,
) -> HlsEntryMasterPlaylistResponse {
    let path_prefix = normalize_hls_proxy_public_path_prefix(server_path).unwrap_or_default();
    let media_playlist_uri = format!("{path_prefix}{}", hls_canonical_manifest_path(proxy_session_id, access_lease_id));
    let body = HlsSingleVariantMasterPlaylist::new(bandwidth, media_playlist_uri).render().into_bytes();
    let content_length = body.len();
    let response = try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "private, no-store, no-cache, must-revalidate")
        .header(header::CONTENT_LENGTH, content_length)
        .body(Body::from(body)));
    HlsEntryMasterPlaylistResponse { response, content_length }
}

pub(super) fn hls_canonical_manifest_path(
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> String {
    format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, access_lease_id.0)
}

pub(super) fn hls_canonical_retry_after_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        .header(axum::http::header::RETRY_AFTER, cold_start_retry_after_seconds().to_string())
        .body(axum::body::Body::empty()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsCanonicalOwnerRegistration {
    Join(HlsCanonicalOwnerRegistrationKind),
    FailClosed(HlsCanonicalOwnerRegistrationFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsCanonicalOwnerRegistrationKind {
    Scheduled,
    AlreadyOwned,
}

impl HlsCanonicalOwnerRegistrationKind {
    pub(super) const fn as_label(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::AlreadyOwned => "already_owned",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsCanonicalOwnerRegistrationFailure {
    CapacityExceeded,
    RuntimeUnavailable,
}

impl HlsCanonicalOwnerRegistrationFailure {
    pub(super) const fn as_label(self) -> &'static str {
        match self {
            Self::CapacityExceeded => "capacity_exceeded",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

pub(super) const fn hls_canonical_owner_registration(
    registration: HlsAvailabilityReevaluationRegistration,
) -> HlsCanonicalOwnerRegistration {
    match registration {
        HlsAvailabilityReevaluationRegistration::Scheduled => {
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::Scheduled)
        }
        HlsAvailabilityReevaluationRegistration::AlreadyOwned | HlsAvailabilityReevaluationRegistration::Superseded => {
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::AlreadyOwned)
        }
        HlsAvailabilityReevaluationRegistration::CapacityExceeded => {
            HlsCanonicalOwnerRegistration::FailClosed(HlsCanonicalOwnerRegistrationFailure::CapacityExceeded)
        }
        HlsAvailabilityReevaluationRegistration::RuntimeUnavailable => {
            HlsCanonicalOwnerRegistration::FailClosed(HlsCanonicalOwnerRegistrationFailure::RuntimeUnavailable)
        }
    }
}

pub(super) fn hls_availability_reevaluation_registration_failure_response(
    failure: HlsCanonicalOwnerRegistrationFailure,
) -> axum::response::Response {
    warn!("HLS availability reevaluation not registered: reason={}", failure.as_label());
    hls_canonical_retry_after_response()
}

pub(super) enum HlsCanonicalOwnerResolution {
    Live(axum::response::Response),
    Terminal(axum::response::Response),
    Standalone(axum::response::Response),
    FailedClosed { reason: HlsCanonicalOwnerFailureReason, response: axum::response::Response },
}

impl HlsCanonicalOwnerResolution {
    pub(super) const fn outcome_label(&self) -> &'static str {
        match self {
            Self::Live(_) => "live",
            Self::Terminal(_) => "terminal",
            Self::Standalone(_) => "standalone",
            Self::FailedClosed { reason, .. } => reason.as_label(),
        }
    }

    pub(super) fn into_response(self) -> axum::response::Response {
        match self {
            Self::Live(response)
            | Self::Terminal(response)
            | Self::Standalone(response)
            | Self::FailedClosed { response, .. } => response,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsCanonicalOwnerFailureReason {
    Superseded,
    DeadlineElapsed,
    LeaseUnavailable,
}

impl HlsCanonicalOwnerFailureReason {
    pub(super) const fn as_label(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::DeadlineElapsed => "deadline_elapsed",
            Self::LeaseUnavailable => "lease_unavailable",
        }
    }

    pub(super) fn response(self) -> axum::response::Response {
        let reason = match self {
            Self::DeadlineElapsed => HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed,
            Self::Superseded | Self::LeaseUnavailable => HlsTerminalFailedClosedReason::LeaseStateUnavailable,
        };
        hls_terminal_failed_closed_response(reason)
    }
}

pub(super) struct HlsCanonicalOwnerPending {
    pub(super) deadline_ms: u64,
    pub(super) current_session_available: bool,
}

pub(super) enum HlsCanonicalOwnerEvaluation {
    Resolved(HlsCanonicalOwnerResolution),
    Pending(HlsCanonicalOwnerPending),
}

pub(super) fn hls_canonical_owner_failed(reason: HlsCanonicalOwnerFailureReason) -> HlsCanonicalOwnerEvaluation {
    HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::FailedClosed {
        reason,
        response: reason.response(),
    })
}

pub(super) struct HlsCanonicalOwnerHandoffContext<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) proxy_session_id: &'a ProxySessionId,
    pub(super) access_lease_id: &'a HlsAccessLeaseId,
    pub(super) expected_lease_issued_at_ms: Option<u64>,
    pub(super) strip: &'a crate::model::StripConfig,
    pub(super) server_path: Option<&'a str>,
    pub(super) manifest_commit_requirement: HlsManifestCommitRequirement,
    pub(super) manifest_boundary_rendered_at_ms: u64,
    pub(super) bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
    pub(super) request_deadline_ms: u64,
    pub(super) safe_session: String,
}

pub(super) fn hls_canonical_owner_lease_deadline_ms(lease: &HlsAccessLease) -> u64 {
    if lease.state == HlsAccessLeaseState::Pending {
        lease.pending_deadline_ms().unwrap_or(lease.valid_until_ms)
    } else {
        lease.valid_until_ms
    }
}

pub(super) fn hls_canonical_owner_request_deadline_ms(
    lease: &HlsAccessLease,
    wait_timeout: Duration,
    now_ms: u64,
) -> u64 {
    let lease_deadline_ms = hls_canonical_owner_lease_deadline_ms(lease);
    if wait_timeout.is_zero() && lease.state == HlsAccessLeaseState::Pending {
        lease_deadline_ms
    } else {
        lease_deadline_ms.min(now_ms.saturating_add(duration_to_millis_saturating(wait_timeout)))
    }
}

pub(super) async fn evaluate_hls_canonical_owner_handoff(
    context: &HlsCanonicalOwnerHandoffContext<'_>,
) -> HlsCanonicalOwnerEvaluation {
    let now_ms = current_time_millis();
    let Some(lease) = context
        .app_state
        .hls_proxy
        .access_lease_response_snapshot(context.access_lease_id, context.proxy_session_id, now_ms)
        .await
    else {
        return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
    };
    if context.expected_lease_issued_at_ms != Some(lease.issued_at_ms) {
        return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
    }
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_) => {
            let Some(response) =
                hls_terminal_playback_response(&lease, context.proxy_session_id, context.access_lease_id)
            else {
                return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
            };
            return HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::Terminal(response));
        }
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
            return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
        }
        HlsLeasePlaybackMode::Live => {}
    }
    if matches!(
        lease.state,
        HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied
    ) {
        return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
    }

    let current_session =
        context.app_state.hls_proxy.sessions().get_by_proxy_session_id(context.proxy_session_id).await;
    if let Some(current_session) = current_session.as_ref() {
        let options = hls_cached_manifest_options_for_requirement(
            Duration::ZERO,
            context.manifest_commit_requirement,
            context.manifest_boundary_rendered_at_ms,
        );
        if let Some(response) = try_hls_cached_manifest_response(
            context.app_state,
            current_session,
            context.access_lease_id,
            lease.state,
            context.strip,
            context.server_path,
            options,
            context.bandwidth_learning,
        )
        .await
        .filter(|response| response.status() == StatusCode::OK)
        {
            return HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::Live(response));
        }
    }
    HlsCanonicalOwnerEvaluation::Pending(HlsCanonicalOwnerPending {
        deadline_ms: hls_canonical_owner_lease_deadline_ms(&lease).min(context.request_deadline_ms),
        current_session_available: current_session.is_some(),
    })
}

pub(super) async fn finalize_hls_canonical_owner_handoff(
    context: &HlsCanonicalOwnerHandoffContext<'_>,
    pending: HlsCanonicalOwnerPending,
    deadline_elapsed: bool,
) -> HlsCanonicalOwnerResolution {
    match evaluate_hls_canonical_owner_handoff(context).await {
        HlsCanonicalOwnerEvaluation::Resolved(resolution) => resolution,
        HlsCanonicalOwnerEvaluation::Pending(current) => {
            let response = hls_unpublished_lease_channel_unavailable_response(
                context.app_state,
                context.proxy_session_id,
                context.access_lease_id,
            )
            .await;
            if response.status() == StatusCode::OK {
                return HlsCanonicalOwnerResolution::Standalone(response);
            }
            let reason = if deadline_elapsed {
                HlsCanonicalOwnerFailureReason::DeadlineElapsed
            } else if !current.current_session_available && !pending.current_session_available {
                HlsCanonicalOwnerFailureReason::Superseded
            } else {
                HlsCanonicalOwnerFailureReason::LeaseUnavailable
            };
            HlsCanonicalOwnerResolution::FailedClosed { reason, response: reason.response() }
        }
    }
}

pub(super) async fn join_hls_canonical_manifest_owner(
    context: HlsCanonicalOwnerHandoffContext<'_>,
    registration: HlsCanonicalOwnerRegistrationKind,
) -> axum::response::Response {
    let started_at = tokio::time::Instant::now();
    let coordinator = context.app_state.hls_proxy.availability_reevaluations();
    let resolution = loop {
        let mut observer = coordinator.observe_owner(context.proxy_session_id);
        let pending = match evaluate_hls_canonical_owner_handoff(&context).await {
            HlsCanonicalOwnerEvaluation::Resolved(resolution) => break resolution,
            HlsCanonicalOwnerEvaluation::Pending(pending) => pending,
        };
        let now_ms = current_time_millis();
        if now_ms >= pending.deadline_ms {
            break finalize_hls_canonical_owner_handoff(&context, pending, true).await;
        }
        let Some(observer) = observer.as_mut() else {
            break finalize_hls_canonical_owner_handoff(&context, pending, false).await;
        };
        let remaining_ms = pending.deadline_ms.saturating_sub(now_ms);
        match tokio::time::timeout(Duration::from_millis(remaining_ms), observer.changed()).await {
            Ok(
                HlsAvailabilityReevaluationObservation::EvidenceChanged
                | HlsAvailabilityReevaluationObservation::OwnerFinished,
            ) => {}
            Err(_) => break finalize_hls_canonical_owner_handoff(&context, pending, true).await,
        }
    };
    debug!(
        "HLS canonical manifest owner handoff completed: session={} proxy_session={} lease={} registration={} outcome={} wait_ms={}",
        context.safe_session,
        safe_proxy_session_id(context.proxy_session_id),
        safe_hls_access_lease_id(context.access_lease_id),
        registration.as_label(),
        resolution.outcome_label(),
        duration_to_millis_saturating(started_at.elapsed())
    );
    resolution.into_response()
}

pub(super) async fn hls_direct_refresh_follow_up(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    refresh_request: OriginRefreshRequest,
    outcome: HlsOriginRefreshTriggerOutcome,
) -> Option<axum::response::Response> {
    match outcome {
        HlsOriginRefreshTriggerOutcome::Started
        | HlsOriginRefreshTriggerOutcome::SessionUnavailable
        | HlsOriginRefreshTriggerOutcome::InFlight
        | HlsOriginRefreshTriggerOutcome::DebouncedUntil { .. } => return None,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded => {
            warn!("HLS direct origin refresh evidence superseded; scheduling current availability reevaluation");
        }
        HlsOriginRefreshTriggerOutcome::RecoveryPressureStateContention => {
            warn!("HLS direct origin refresh state contended; scheduling current availability reevaluation");
        }
    }
    let Some(owner_key) = app_state.hls_proxy.availability_reevaluation_owner_key(session, proxy_session_id).await
    else {
        warn!("HLS direct origin refresh follow-up unavailable: reason=session_superseded");
        return Some(hls_canonical_retry_after_response());
    };
    match register_hls_availability_reevaluation(app_state.hls_ctx(), Arc::clone(session), owner_key, refresh_request) {
        HlsAvailabilityReevaluationRegistration::Scheduled
        | HlsAvailabilityReevaluationRegistration::AlreadyOwned
        | HlsAvailabilityReevaluationRegistration::Superseded => None,
        HlsAvailabilityReevaluationRegistration::CapacityExceeded => {
            warn!("HLS direct origin refresh follow-up unavailable: reason=capacity_exceeded");
            Some(hls_canonical_retry_after_response())
        }
        HlsAvailabilityReevaluationRegistration::RuntimeUnavailable => {
            warn!("HLS direct origin refresh follow-up unavailable: reason=runtime_unavailable");
            Some(hls_canonical_retry_after_response())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsManifestRefreshOrdering {
    Background,
    AwaitBeforeTerminalEvaluation,
}

pub(super) async fn trigger_hls_canonical_manifest_refresh(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    refresh_request: OriginRefreshRequest,
    ordering: HlsManifestRefreshOrdering,
) -> Option<axum::response::Response> {
    match ordering {
        HlsManifestRefreshOrdering::Background => {
            let outcome = maybe_trigger_origin_refresh_with_outcome(refresh_request.clone()).await;
            hls_direct_refresh_follow_up(app_state, session, proxy_session_id, refresh_request, outcome).await
        }
        HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation => {
            // An already-owned refresh cannot be joined through this call, but
            // it must not bypass the lease-specific terminal decision. The
            // in-flight owner will still publish its eventual progress/failure.
            let _refresh_started = trigger_origin_refresh_sync(refresh_request).await;
            let now_ms = current_time_millis();
            let Some(lease) =
                app_state.hls_proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
            else {
                return Some(hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::LeaseStateUnavailable));
            };
            resolve_hls_terminal_manifest_state(app_state, session, proxy_session_id, access_lease_id, lease, now_ms)
                .await
                .err()
                .map(|response| *response)
        }
    }
}

pub(super) fn hls_canonical_status_response(status: StatusCode) -> axum::response::Response {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        hls_canonical_retry_after_response()
    } else {
        status.into_response()
    }
}

pub(super) struct HlsEntryOriginAccountReservation {
    pub(super) request_url: String,
    pub(super) session_token: String,
    pub(super) provider_handle: Option<ProviderHandle>,
    pub(super) selected_provider_config: Option<Arc<RuntimeProviderConfig>>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_reserve_hls_entry_origin_account_for_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    request_url: &str,
    user_session_token: &str,
    session_owner: &str,
    reservation_ttl_secs: u64,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    create_user_session: bool,
) -> Option<HlsEntryOriginAccountReservation> {
    let provider_handle = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            false,
            connection_priority_for_kind(user, connection_kind),
            connection_kind,
            Some(session_owner),
        )
        .await?;

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return None;
    };
    let Some(stream_url) = get_stream_alternative_url(request_url, input, &provider_config) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return None;
    };

    let session_token = if create_user_session {
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token: user_session_token,
                virtual_id,
                provider: &provider_config.name,
                stream_url: &stream_url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(connection_kind),
                socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
            })
            .await
    } else {
        user_session_token.to_string()
    };

    app_state
        .active_provider
        .refresh_provider_reservation(&provider_config.name, session_owner, reservation_ttl_secs)
        .await;

    Some(HlsEntryOriginAccountReservation {
        request_url: stream_url,
        session_token,
        provider_handle: Some(provider_handle),
        selected_provider_config: Some(provider_config),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_reserve_hls_virtual_entry_origin_account_for_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
) -> bool {
    let virtual_id = stream_identity.virtual_id();
    let session_token =
        create_playback_session_fingerprint(fingerprint, &user.username, virtual_id, PlaylistItemType::LiveHls, None);
    let (connection_admission, _, _) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        user,
        fingerprint,
        None,
        &session_token,
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(virtual_id) },
        false,
        false,
    )
    .await;
    if connection_admission.permission == UserConnectionPermission::Exhausted {
        return false;
    }

    let Some(channel) = get_stream_channel(app_state, target, virtual_id).await else {
        return false;
    };
    let Ok(origin_playlist_url) =
        resolve_hls_origin_playlist_url(app_state, target, input, virtual_id, channel.url.as_ref()).await
    else {
        return false;
    };
    let Some(hls_cache_origin) = build_hls_origin_resolution(input, &origin_playlist_url) else {
        return false;
    };
    let Some(connection_kind) = connection_admission.kind else {
        return false;
    };
    let (shared_hls_session_owner, reservation_ttl_secs) = if hls_cache_enabled_for_target(app_state, target) {
        let origin_source = build_hls_origin_source(input, stream_identity.stream_ref());
        let proxy_session_id = build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());
        let reservation_ttl_secs = match app_state.hls_proxy.sessions().get_by_key(&origin_source.session_key()).await {
            Some(session) => hls_origin_account_reservation_ttl_secs_for_session(&session).await,
            None => hls_origin_account_reservation_ttl_secs_fallback(),
        };
        (Some(build_hls_origin_session_owner(&proxy_session_id)), reservation_ttl_secs)
    } else {
        (None, get_hls_session_ttl_secs(app_state))
    };
    let session_owner = shared_hls_session_owner.as_deref().unwrap_or(session_token.as_str());

    let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
        app_state,
        fingerprint,
        user,
        input,
        virtual_id,
        hls_cache_origin.session_entry_url.as_str(),
        &session_token,
        session_owner,
        reservation_ttl_secs,
        connection_admission.permission,
        connection_kind,
        false,
    )
    .await
    else {
        return false;
    };

    app_state.connection_manager.release_provider_handle(reservation.provider_handle).await;
    true
}

pub(super) async fn mark_hls_provisioning_handoff_discontinuity(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
    access_lease_id: Option<&HlsAccessLeaseId>,
    now_ms: u64,
) -> bool {
    if !hls_cache_configured(app_state) {
        return false;
    }
    let origin_source = build_hls_origin_source(input, stream_identity.stream_ref());
    let Some(session) = app_state.hls_proxy.sessions().get_by_key(&origin_source.session_key()).await else {
        return false;
    };
    mark_hls_provisioning_handoff_discontinuity_once_for_session(
        app_state,
        &session,
        input,
        stream_identity.virtual_id(),
        access_lease_id,
        now_ms,
    )
    .await
}

pub(super) async fn mark_hls_provisioning_handoff_discontinuity_once_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: Option<&HlsAccessLeaseId>,
    now_ms: u64,
) -> bool {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    if !app_state.hls_provisioning.mark_handoff_once(
        &input.name,
        virtual_id,
        Some(&proxy_session_id),
        access_lease_id,
        now_ms,
    ) {
        debug!(
            "HLS provisioning handoff discontinuity already marked: proxy_session={}",
            safe_proxy_session_id(&proxy_session_id)
        );
        return false;
    }
    mark_hls_provisioning_handoff_discontinuity_for_session(session, now_ms).await;
    ensure_shared_hls_provisioning_handoff_gap(app_state, session, now_ms).await;
    true
}

pub(super) async fn mark_hls_provisioning_handoff_discontinuity_for_session(session: &HlsSessionHandle, now_ms: u64) {
    let discontinuity_sequence = hls_provisioning_discontinuity_sequence(now_ms);
    let proxy_session_id = {
        let mut session = session.write().await;
        session.mark_pending_handoff_discontinuity(discontinuity_sequence);
        session.proxy_session_id.clone()
    };
    debug!(
        "HLS provisioning handoff discontinuity marked: proxy_session={} discontinuity_sequence={}",
        safe_proxy_session_id(&proxy_session_id),
        discontinuity_sequence
    );
}

pub(super) fn clear_hls_provisioning_handoff_consumer(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    virtual_id: u32,
    now_ms: u64,
) {
    if !app_state.hls_provisioning.take_ready_slot_for_consumer(&input.name, virtual_id, now_ms) {
        app_state.hls_provisioning.clear_consumer(&input.name, virtual_id);
    }
}

pub(super) async fn maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Option<u64> {
    if !app_state.hls_provisioning.has_consumer(&input.name, virtual_id, now_ms) {
        return None;
    }
    let previous_manifest_rendered_at_ms = latest_shared_hls_manifest_rendered_at_ms(session).await;
    mark_hls_provisioning_handoff_discontinuity_once_for_session(
        app_state,
        session,
        input,
        virtual_id,
        Some(access_lease_id),
        now_ms,
    )
    .await
    .then_some(previous_manifest_rendered_at_ms)
}

pub(super) async fn latest_shared_hls_manifest_rendered_at_ms(session: &HlsSessionHandle) -> u64 {
    let session = session.read().await;
    session
        .last_rendered_manifest
        .as_ref()
        .map_or(0, |rendered| rendered.rendered_at_ms)
        .max(session.transient.last_manifest_rendered_at_ms.unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api) async fn hls_panel_provisioning_poll_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
    original_hls_entry_path: &str,
    server_path: Option<&str>,
) -> axum::response::Response {
    hls_panel_provisioning_poll_response(
        app_state,
        fingerprint,
        user,
        target,
        input,
        stream_identity,
        original_hls_entry_path,
        server_path,
        HlsProvisioningPollResponseKind::Legacy,
    )
    .await
}

pub(super) enum HlsProvisioningPollResponseKind {
    Legacy,
}

impl HlsProvisioningPollResponseKind {
    pub(super) fn access_lease_id(&self) -> Option<&HlsAccessLeaseId> {
        match self {
            Self::Legacy => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn hls_panel_provisioning_poll_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
    ready_redirect_path: &str,
    server_path: Option<&str>,
    response_kind: HlsProvisioningPollResponseKind,
) -> axum::response::Response {
    let virtual_id = stream_identity.virtual_id();
    let now_ms = current_time_millis();
    app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), virtual_id, now_ms);

    let existing_status = app_state.hls_provisioning.consumer_status(&input.name, virtual_id, now_ms);

    if try_reserve_hls_virtual_entry_origin_account_for_redirect(
        app_state,
        fingerprint,
        user,
        target,
        input,
        stream_identity,
    )
    .await
    {
        mark_hls_provisioning_handoff_discontinuity(
            app_state,
            input,
            stream_identity,
            response_kind.access_lease_id(),
            now_ms,
        )
        .await;
        clear_hls_provisioning_handoff_consumer(app_state, input, virtual_id, current_time_millis());
        return hls_virtual_entry_redirect_response(ready_redirect_path, server_path);
    }

    let provisioning_enabled = can_provision_on_exhausted(app_state.as_ref(), input);
    if provisioning_enabled {
        start_hls_panel_provisioning_once(app_state, input);
    }

    let status = existing_status.unwrap_or(if provisioning_enabled {
        HlsProvisioningStatus::InProgress
    } else {
        HlsProvisioningStatus::ProviderExhausted
    });

    match status {
        HlsProvisioningStatus::Ready | HlsProvisioningStatus::InProgress => {
            hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                user,
                CustomVideoStreamType::Provisioning,
                StatusCode::SERVICE_UNAVAILABLE,
                Some(virtual_id),
            )
            .await
        }
        HlsProvisioningStatus::ProviderExhausted => {
            hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                user,
                CustomVideoStreamType::ProviderConnectionsExhausted,
                StatusCode::SERVICE_UNAVAILABLE,
                Some(virtual_id),
            )
            .await
        }
    }
}

pub(super) async fn hls_panel_provisioning_or_status_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    _original_hls_entry_path: &str,
    server_path: Option<&str>,
    fallback_status: StatusCode,
) -> axum::response::Response {
    try_hls_panel_provisioning_manifest_response(
        app_state,
        user,
        input,
        virtual_id,
        HlsPanelProvisioningRedirectPaths { waiting_manifest_path: None },
        server_path,
        fallback_status,
    )
    .await
    .unwrap_or_else(|| fallback_status.into_response())
}

#[derive(Debug, Clone)]
pub(super) struct SharedHlsProvisioningSegmentPlan {
    pub(super) proxy_seq: u64,
    pub(super) physical_index: usize,
    pub(super) cache_key: SegmentCacheKey,
    pub(super) segment_kind: SharedHlsProvisioningLocalSegmentKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SharedHlsProvisioningLocalSegmentKind {
    Provisioning,
    Gap,
}

pub(super) fn shared_hls_provisioning_segment_plans(
    session: &HlsSession,
    physical_segment_count: usize,
) -> Vec<SharedHlsProvisioningSegmentPlan> {
    let existing_provisioning_segments =
        session.segments.values().filter(|entry| is_hls_provisioning_segment(entry)).count();
    let append_count = if existing_provisioning_segments == 0 { 3 } else { 1 };
    let start_proxy_seq = session.proxy_next_seq.unwrap_or(0);
    (0..append_count)
        .filter_map(|offset| {
            let proxy_seq = start_proxy_seq.checked_add(u64::try_from(offset).ok()?)?;
            if session.segments.contains_key(&proxy_seq) {
                return None;
            }
            Some(SharedHlsProvisioningSegmentPlan {
                proxy_seq,
                physical_index: (existing_provisioning_segments + offset) % physical_segment_count,
                cache_key: SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts"),
                segment_kind: SharedHlsProvisioningLocalSegmentKind::Provisioning,
            })
        })
        .collect()
}

pub(super) fn shared_hls_provisioning_segment_entry(
    plan: SharedHlsProvisioningSegmentPlan,
    content_length: u64,
    duration_ms: u64,
    now_ms: u64,
) -> SegmentEntry {
    let origin_epoch = match plan.segment_kind {
        SharedHlsProvisioningLocalSegmentKind::Provisioning => HLS_PROVISIONING_ORIGIN_EPOCH,
        SharedHlsProvisioningLocalSegmentKind::Gap => HLS_PROVISIONING_GAP_ORIGIN_EPOCH,
    };
    SegmentEntry {
        origin_key: OriginSegmentKey {
            origin_epoch,
            effective_host_id: 0,
            host_local_sequence: plan.proxy_seq,
            host_local_index: u32::try_from(plan.proxy_seq).unwrap_or(u32::MAX),
        },
        proxy_seq: plan.proxy_seq,
        duration_ms,
        proxy_file_ext: "ts".to_string(),
        content_type: "video/mp2t".to_string(),
        cache_key: plan.cache_key,
        discontinuity_before: false,
        program_date_time: None,
        daterange_tags_before: Vec::new(),
        origin_byte_range: None,
        map_ref: None,
        encryption: None,
        origin_fetch_ref: None,
        status: SegmentCacheStatus::Ready { content_length, ready_at_ms: now_ms },
        last_rendered_at_ms: None,
        access: Arc::new(CacheAccessState::new()),
    }
}

pub(super) async fn commit_shared_hls_provisioning_segments(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    plans: &[SharedHlsProvisioningSegmentPlan],
    provisioning_segments: &[TransportStreamBuffer],
) -> Option<Vec<(SharedHlsProvisioningSegmentPlan, u64, u64)>> {
    let mut committed = Vec::with_capacity(plans.len());
    for plan in plans {
        let video = provisioning_segments.get(plan.physical_index)?;
        let duration_ms = video.duration_ms().unwrap_or(HLS_PROVISIONING_SEGMENT_DURATION_MS);
        let metadata = match app_state
            .hls_proxy
            .segment_cache()
            .write_bytes_and_commit(&plan.cache_key, video.as_bytes())
            .await
        {
            Ok(metadata) => metadata,
            Err(err) => {
                let safe_proxy_session = {
                    let session_guard = session.read().await;
                    safe_proxy_session_id(&session_guard.proxy_session_id)
                };
                warn!(
                    "HLS provisioning segment cache commit failed for shared manifest: proxy_session={} seq={} error={err}",
                    safe_proxy_session, plan.proxy_seq
                );
                return None;
            }
        };
        committed.push((plan.clone(), metadata.size, duration_ms));
    }
    Some(committed)
}

pub(super) async fn ensure_shared_hls_provisioning_handoff_gap(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) -> bool {
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    let Some(provisioning_segments) = custom_stream_response
        .as_ref()
        .map(|response| response.panel_api_provisioning_hls_segments.clone())
        .filter(|segments| !segments.is_empty())
    else {
        return false;
    };
    let plan = {
        let session_guard = session.read().await;
        if !session_guard.segments.values().any(is_hls_provisioning_segment)
            || session_guard.segments.values().any(is_hls_provisioning_gap_segment)
        {
            return false;
        }
        let proxy_seq = session_guard.proxy_next_seq.unwrap_or(0);
        if session_guard.segments.contains_key(&proxy_seq) {
            return false;
        }
        let existing_provisioning_segments =
            session_guard.segments.values().filter(|entry| is_hls_provisioning_segment(entry)).count();
        SharedHlsProvisioningSegmentPlan {
            proxy_seq,
            physical_index: existing_provisioning_segments % provisioning_segments.len(),
            cache_key: SegmentCacheKey::new(session_guard.proxy_session_id.clone(), proxy_seq, "ts"),
            segment_kind: SharedHlsProvisioningLocalSegmentKind::Gap,
        }
    };
    let Some(committed) = commit_shared_hls_provisioning_segments(
        app_state,
        session,
        std::slice::from_ref(&plan),
        &provisioning_segments,
    )
    .await
    else {
        return false;
    };
    let mut session_guard = session.write().await;
    let mut inserted = false;
    for (plan, content_length, duration_ms) in committed {
        if session_guard.segments.contains_key(&plan.proxy_seq) {
            continue;
        }
        if session_guard.publishable_origin_head_proxy_seq.is_none() {
            session_guard.publishable_origin_head_proxy_seq = Some(plan.proxy_seq);
        }
        session_guard.publishable_origin_tail_proxy_seq = Some(plan.proxy_seq);
        session_guard.proxy_next_seq = Some(plan.proxy_seq.saturating_add(1));
        session_guard
            .segments
            .insert(plan.proxy_seq, shared_hls_provisioning_segment_entry(plan, content_length, duration_ms, now_ms));
        inserted = true;
    }
    if inserted {
        session_guard.target_duration = Some(HLS_PROVISIONING_TARGET_DURATION_SECS);
        session_guard.independent_segments = true;
    }
    inserted
}

pub(super) async fn hls_shared_provisioning_timeline_manifest_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
) -> Option<axum::response::Response> {
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    let provisioning_segments = custom_stream_response
        .as_ref()
        .map(|response| response.panel_api_provisioning_hls_segments.clone())
        .filter(|segments| !segments.is_empty())?;
    let now_ms = current_time_millis();
    let plans = {
        let session_guard = session.read().await;
        shared_hls_provisioning_segment_plans(&session_guard, provisioning_segments.len())
    };
    if plans.is_empty() {
        let mut session_guard = session.write().await;
        session_guard.render_and_store_manifest(now_ms).ok()?;
    } else {
        let committed =
            commit_shared_hls_provisioning_segments(app_state, session, &plans, &provisioning_segments).await?;
        let mut session_guard = session.write().await;
        for (plan, content_length, duration_ms) in committed {
            if session_guard.segments.contains_key(&plan.proxy_seq) {
                continue;
            }
            if session_guard.publishable_origin_head_proxy_seq.is_none() {
                session_guard.publishable_origin_head_proxy_seq = Some(plan.proxy_seq);
            }
            session_guard.publishable_origin_tail_proxy_seq = Some(plan.proxy_seq);
            session_guard.proxy_next_seq = Some(plan.proxy_seq.saturating_add(1));
            session_guard.segments.insert(
                plan.proxy_seq,
                shared_hls_provisioning_segment_entry(plan, content_length, duration_ms, now_ms),
            );
        }
        session_guard.target_duration = Some(HLS_PROVISIONING_TARGET_DURATION_SECS);
        session_guard.independent_segments = true;
        session_guard.render_and_store_manifest(now_ms).ok()?;
    }

    try_hls_cached_manifest_response(
        app_state,
        session,
        access_lease_id,
        access_lease_state,
        strip,
        server_path,
        HlsCachedManifestOptions::initial(Duration::ZERO),
        HlsRuntimeBandwidthLearningContext::Disabled,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn hls_shared_provisioning_or_provider_exhausted_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    username: &str,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
) -> axum::response::Response {
    let Some((_user, _target)) = app_state.app_config.get_target_for_username(username) else {
        return hls_canonical_retry_after_response();
    };
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let now_ms = current_time_millis();
    let provisioning_enabled = can_provision_on_exhausted(app_state.as_ref(), input);
    if provisioning_enabled {
        app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), virtual_id, now_ms);
        start_hls_panel_provisioning_once(app_state, input);
        if let Some(HlsProvisioningStatus::ProviderExhausted) =
            app_state.hls_provisioning.consumer_status(&input.name, virtual_id, now_ms)
        {
            return hls_runtime_or_standalone_custom_tail_response(
                app_state,
                session,
                &proxy_session_id,
                access_lease_id,
                HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        }
        if let Some(response) = hls_shared_provisioning_timeline_manifest_response(
            app_state,
            session,
            access_lease_id,
            access_lease_state,
            strip,
            server_path,
        )
        .await
        {
            return response;
        }
    }

    let provider_exhausted_custom_response_available = is_custom_video_stream_enabled(&app_state.app_config)
        && app_state
            .app_config
            .custom_stream_response
            .load()
            .as_ref()
            .and_then(|response| response.provider_connections_exhausted.as_ref())
            .is_some();
    if provider_exhausted_custom_response_available {
        return hls_runtime_or_standalone_custom_tail_response(
            app_state,
            session,
            &proxy_session_id,
            access_lease_id,
            HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    }
    hls_canonical_retry_after_response()
}

pub(super) enum HlsProviderExhaustedResolution {
    RetryAcquire,
    Response(axum::response::Response),
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn hls_provider_connections_exhausted_manifest_resolution(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    username: &str,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
    allow_grace_hold: bool,
) -> HlsProviderExhaustedResolution {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let grace_options = app_state.get_grace_options();
    if allow_grace_hold && grace_options.hold_stream && grace_options.period_millis > 0 {
        debug!(
            "HLS provider connections exhausted; holding canonical manifest for grace: proxy_session={} lease={} hold_ms={}",
            safe_proxy_session_id(&proxy_session_id),
            safe_hls_access_lease_id(access_lease_id),
            grace_options.period_millis
        );
        let capacity_notify = app_state.connection_manager.capacity_notified();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(grace_options.period_millis);
        let wake_reason = tokio::select! {
            () = capacity_notify.notified() => "capacity-notified",
            () = tokio::time::sleep_until(deadline) => "timeout",
        };
        debug!(
            "HLS provider connections exhausted grace hold completed: proxy_session={} lease={} reason={wake_reason}",
            safe_proxy_session_id(&proxy_session_id),
            safe_hls_access_lease_id(access_lease_id)
        );
        return HlsProviderExhaustedResolution::RetryAcquire;
    }

    HlsProviderExhaustedResolution::Response(
        hls_shared_provisioning_or_provider_exhausted_response(
            app_state,
            session,
            username,
            input,
            virtual_id,
            access_lease_id,
            access_lease_state,
            strip,
            server_path,
        )
        .await,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_hls_canonical_manifest_origin_runtime(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    context: &HlsAccessContext,
    origin: &HlsCacheManifestOrigin<'_>,
    path_proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    fingerprint: &Fingerprint,
    server_path: Option<&str>,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, Box<axum::response::Response>> {
    let mut allow_grace_hold = true;
    loop {
        let origin_policy = hls_effective_origin_acquire_policy(session).await;
        match prepare_hls_origin_runtime(
            app_state,
            session,
            origin.input,
            origin.raw_request_url,
            origin.session_entry_url.as_str(),
            path_proxy_session_id,
            fingerprint,
            origin_policy.connection_kind,
            origin_policy.priority,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            now_ms,
        )
        .await
        {
            Ok(prepared) => return Ok(prepared),
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
                reason: HlsOriginRuntimeNoAccountReason::OriginBindingPreempted,
            }) => {
                return Err(Box::new(
                    hls_runtime_or_standalone_custom_tail_response(
                        app_state,
                        session,
                        path_proxy_session_id,
                        access_lease_id,
                        HlsRuntimeCustomTailReason::LowPriorityPreempted,
                        StatusCode::SERVICE_UNAVAILABLE,
                    )
                    .await,
                ));
            }
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
                reason: HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted,
            }) => {
                let strip = app_state.hls_proxy.strip();
                match hls_provider_connections_exhausted_manifest_resolution(
                    app_state,
                    session,
                    &context.username,
                    origin.input,
                    context.virtual_id,
                    access_lease_id,
                    access_lease_state,
                    &strip,
                    server_path,
                    allow_grace_hold,
                )
                .await
                {
                    HlsProviderExhaustedResolution::RetryAcquire => {
                        allow_grace_hold = false;
                    }
                    HlsProviderExhaustedResolution::Response(response) => return Err(Box::new(response)),
                }
            }
            Err(HlsOriginRuntimeAcquireError::Fatal(status)) => {
                return Err(Box::new(hls_canonical_status_response(status)))
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn try_hls_cache_canonical_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    context: &HlsAccessContext,
    path_proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    origin: HlsCacheManifestOrigin<'_>,
    headers: HeaderMap,
    server_path: Option<&str>,
    _original_hls_entry_path: &str,
    refresh_ordering: HlsManifestRefreshOrdering,
) -> Option<axum::response::Response> {
    if !hls_cache_configured(app_state) {
        return None;
    }
    if origin.origin_source.input_id != context.input_id || origin.origin_source.stream_ref != context.stream_ref {
        return Some(StatusCode::NOT_FOUND.into_response());
    }

    let session_key = origin.origin_source.session_key();
    let expected_proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    if &expected_proxy_session_id != path_proxy_session_id {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    let now_ms = current_time_millis();
    let rewrite_secret = app_state.get_encrypt_secret();
    let (session, session_outcome) = app_state
        .hls_proxy
        .get_or_create_session_with_source_and_outcome(
            session_key,
            origin.origin_source.clone(),
            &rewrite_secret,
            now_ms,
        )
        .await;
    if access_lease_state == HlsAccessLeaseState::Activated {
        let timing = hls_access_lease_timing_for_session(app_state, &session).await;
        match app_state
            .hls_proxy
            .touch_manifest_access_lease(
                access_lease_id,
                path_proxy_session_id,
                now_ms,
                Some(timing),
                None,
                hls_access_lease_ttl_ms(app_state),
            )
            .await
        {
            HlsAccessLeaseTouch::Touched { .. } => {}
            HlsAccessLeaseTouch::Denied => {
                return Some(
                    hls_runtime_or_standalone_custom_tail_response(
                        app_state,
                        &session,
                        path_proxy_session_id,
                        access_lease_id,
                        HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                        StatusCode::FORBIDDEN,
                    )
                    .await,
                );
            }
            HlsAccessLeaseTouch::Expired | HlsAccessLeaseTouch::UnknownLease | HlsAccessLeaseTouch::SessionMismatch => {
                return Some(StatusCode::NOT_FOUND.into_response());
            }
        }
    }
    app_state
        .hls_proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            path_proxy_session_id,
            now_ms,
        )
        .await;
    let prepared_origin = match prepare_hls_canonical_manifest_origin_runtime(
        app_state,
        &session,
        context,
        &origin,
        path_proxy_session_id,
        access_lease_id,
        access_lease_state,
        fingerprint,
        server_path,
        now_ms,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(response) => return Some(*response),
    };
    let url_failover_provider = effective_hls_url_failover_provider_for_fetch_url(
        &prepared_origin.fetch_url,
        prepared_origin.url_failover_provider.clone(),
        origin.session_entry_url.url_failover_provider(),
    );
    let origin_entry =
        LiveHlsOriginEntry::parse_with_url_failover_provider(&prepared_origin.fetch_url, url_failover_provider)?;
    {
        let mut session_guard = session.write().await;
        if session_guard.is_gc_marked_for_removal() {
            return Some(hls_canonical_retry_after_response());
        }
        if prepared_origin.origin_account_binding_to_store.is_some() {
            session_guard.replace_origin_account_binding(prepared_origin.origin_account_binding_to_store);
        }
    }
    mark_hls_authorized_manifest_access(app_state, &session, now_ms).await;
    let selected_account = session.read().await.origin_account_binding.as_ref().map_or_else(
        || "<none>".to_string(),
        |binding| sanitize_sensitive_info(binding.account_name.as_ref()).to_string(),
    );
    debug!(
        "HLS origin account selected: proxy_session={} account={}",
        safe_proxy_session_id(path_proxy_session_id),
        selected_account
    );
    let reservation_ttl_secs = hls_origin_account_reservation_ttl_secs_for_session(&session).await;
    let previous_manifest_rendered_at_ms = latest_shared_hls_manifest_rendered_at_ms(&session).await;
    let handoff_previous_rendered_at_ms = maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
        app_state,
        &session,
        origin.input,
        context.virtual_id,
        access_lease_id,
        now_ms,
    )
    .await;
    let manifest_commit_requirement =
        hls_manifest_commit_requirement(&session, session_outcome, handoff_previous_rendered_at_ms, now_ms).await;
    let hls_ctx = app_state.hls_ctx();
    let acceptance_evaluation =
        hls_manifest_acceptance_directive_for_session(&hls_ctx, &session, path_proxy_session_id).await;
    let (acceptance_directive, availability_reevaluation_owner_key) = match acceptance_evaluation {
        HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => (directive, None),
        HlsManifestAcceptanceEvaluationOutcome::StateContention { owner_key } => {
            (HlsManifestAcceptanceDirective::none(), Some(owner_key))
        }
        HlsManifestAcceptanceEvaluationOutcome::SessionSuperseded => {
            app_state
                .connection_manager
                .release_provider_handle(prepared_origin.preacquired_origin_account_handle)
                .await;
            return Some(hls_canonical_retry_after_response());
        }
    };
    let manifest_boundary_rendered_at_ms = handoff_previous_rendered_at_ms.unwrap_or(previous_manifest_rendered_at_ms);
    let wait_timeout =
        hls_manifest_wait_timeout_for_requirement(app_state, &session, manifest_commit_requirement).await;
    let cached_manifest_options = hls_cached_manifest_options_for_requirement(
        wait_timeout,
        manifest_commit_requirement,
        manifest_boundary_rendered_at_ms,
    );
    let bandwidth_learning = match context.known_bitrate_bps {
        Some(_) => HlsRuntimeBandwidthLearningContext::Disabled,
        None => HlsRuntimeBandwidthLearningContext::Eligible(origin.input),
    };

    let origin_policy = hls_effective_origin_acquire_policy(&session).await;
    let origin_provider_session_headers = session.read().await.origin_provider_session_headers.clone();
    let mut preacquired_provider_handle = prepared_origin.preacquired_origin_account_handle;
    let mut origin_io = HlsOriginIoContext {
        ctx: hls_ctx.clone(),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::ManifestInteractive.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    };
    if availability_reevaluation_owner_key.is_none() {
        if let Some(provider_handle) = preacquired_provider_handle.take() {
            origin_io = origin_io.with_preacquired_provider_handle(provider_handle);
        }
    }

    let refresh_request = OriginRefreshRequest {
        app_config: Arc::clone(&app_state.app_config),
        session: Arc::clone(&session),
        origin_entry,
        headers,
        origin_provider_session_headers,
        client: app_state.http_client.load().as_ref().clone(),
        no_redirect_client: app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        segment_cache: Arc::clone(app_state.hls_proxy.segment_cache()),
        hls_proxy: Arc::clone(&app_state.hls_proxy),
        segment_repair: Arc::clone(app_state.hls_proxy.segment_repair()),
        segment_worker_pool: Arc::clone(app_state.hls_proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(app_state.hls_proxy.map_worker_pool()),
        origin_manifest_timeout_ms: app_state.hls_proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: app_state.hls_proxy.manifest_recovery_burst(),
        strip: app_state.hls_proxy.strip().clone(),
        retry_policy: RetryPolicy::default(),
        reverse_proxy_rewrite_secret: rewrite_secret.to_vec(),
        transient_resource_ttl_ms: app_state.hls_proxy.transient_resource_ttl_ms(),
        manifest_commit_requirement,
        fresh_manifest_requirement_generation: None,
        acceptance_directive,
        access_lease_id: Some(access_lease_id.clone()),
        disabled_headers: app_state.get_disabled_headers(),
        now_ms,
        origin_io: Some(origin_io),
        post_refresh_runtime: Some(HlsPostRefreshRuntime { ctx: hls_ctx.downgrade() }),
    };
    let refresh_ordering = if session_outcome == HlsSessionStoreOutcome::Reused {
        refresh_ordering
    } else {
        HlsManifestRefreshOrdering::Background
    };
    if let Some(owner_key) = availability_reevaluation_owner_key {
        app_state.connection_manager.release_provider_handle(preacquired_provider_handle).await;
        touch_initial_manifest_access_lease_window(
            app_state,
            access_lease_id,
            path_proxy_session_id,
            access_lease_state,
            wait_timeout,
            now_ms,
        )
        .await;
        let owner_wait_lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(access_lease_id, path_proxy_session_id, current_time_millis())
            .await;
        let expected_lease_issued_at_ms = owner_wait_lease.as_ref().map(|lease| lease.issued_at_ms);
        let request_deadline_ms = owner_wait_lease
            .as_ref()
            .map_or(now_ms, |lease| hls_canonical_owner_request_deadline_ms(lease, wait_timeout, now_ms));
        let safe_session = {
            let session = session.read().await;
            safe_session_key(&session.key)
        };
        let registration =
            register_hls_availability_reevaluation(hls_ctx, Arc::clone(&session), owner_key, refresh_request);
        return Some(match hls_canonical_owner_registration(registration) {
            HlsCanonicalOwnerRegistration::Join(registration) => {
                let strip = app_state.hls_proxy.strip();
                join_hls_canonical_manifest_owner(
                    HlsCanonicalOwnerHandoffContext {
                        app_state,
                        proxy_session_id: path_proxy_session_id,
                        access_lease_id,
                        expected_lease_issued_at_ms,
                        strip: &strip,
                        server_path,
                        manifest_commit_requirement,
                        manifest_boundary_rendered_at_ms,
                        bandwidth_learning,
                        request_deadline_ms,
                        safe_session,
                    },
                    registration,
                )
                .await
            }
            HlsCanonicalOwnerRegistration::FailClosed(failure) => {
                hls_availability_reevaluation_registration_failure_response(failure)
            }
        });
    }
    if handoff_previous_rendered_at_ms.is_some() {
        touch_initial_manifest_access_lease_window(
            app_state,
            access_lease_id,
            path_proxy_session_id,
            access_lease_state,
            wait_timeout,
            now_ms,
        )
        .await;
        if let Some(response) = trigger_hls_canonical_manifest_refresh(
            app_state,
            &session,
            path_proxy_session_id,
            access_lease_id,
            refresh_request,
            refresh_ordering,
        )
        .await
        {
            return Some(response);
        }
        let strip = app_state.hls_proxy.strip();
        if let Some(response) = try_hls_cached_manifest_response(
            app_state,
            &session,
            access_lease_id,
            access_lease_state,
            &strip,
            server_path,
            cached_manifest_options,
            bandwidth_learning,
        )
        .await
        {
            clear_hls_provisioning_handoff_consumer(app_state, origin.input, context.virtual_id, current_time_millis());
            return Some(response);
        }
        return Some(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    match session_outcome {
        HlsSessionStoreOutcome::Created => {
            touch_initial_manifest_access_lease_window(
                app_state,
                access_lease_id,
                path_proxy_session_id,
                access_lease_state,
                wait_timeout,
                now_ms,
            )
            .await;
            if let Some(response) = trigger_hls_canonical_manifest_refresh(
                app_state,
                &session,
                path_proxy_session_id,
                access_lease_id,
                refresh_request,
                refresh_ordering,
            )
            .await
            {
                return Some(response);
            }
            let strip = app_state.hls_proxy.strip();
            if let Some(response) = try_hls_cached_manifest_response(
                app_state,
                &session,
                access_lease_id,
                access_lease_state,
                &strip,
                server_path,
                cached_manifest_options,
                bandwidth_learning,
            )
            .await
            {
                return Some(response);
            }
        }
        HlsSessionStoreOutcome::Reused => {
            if let Some(response) = trigger_hls_canonical_manifest_refresh(
                app_state,
                &session,
                path_proxy_session_id,
                access_lease_id,
                refresh_request,
                refresh_ordering,
            )
            .await
            {
                if refresh_ordering == HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation
                    && response.status() == StatusCode::SERVICE_UNAVAILABLE
                {
                    let strip = app_state.hls_proxy.strip();
                    if let Some(live_response) = try_hls_cached_manifest_response(
                        app_state,
                        &session,
                        access_lease_id,
                        access_lease_state,
                        &strip,
                        server_path,
                        HlsCachedManifestOptions::initial(Duration::ZERO),
                        bandwidth_learning,
                    )
                    .await
                    .filter(|candidate| candidate.status() == StatusCode::OK)
                    {
                        return Some(live_response);
                    }
                }
                return Some(response);
            }
            touch_initial_manifest_access_lease_window(
                app_state,
                access_lease_id,
                path_proxy_session_id,
                access_lease_state,
                wait_timeout,
                now_ms,
            )
            .await;
            let strip = app_state.hls_proxy.strip();
            if let Some(response) = try_hls_cached_manifest_response(
                app_state,
                &session,
                access_lease_id,
                access_lease_state,
                &strip,
                server_path,
                cached_manifest_options,
                bandwidth_learning,
            )
            .await
            {
                return Some(response);
            }
        }
    }

    Some(hls_unpublished_lease_channel_unavailable_response(app_state, path_proxy_session_id, access_lease_id).await)
}

pub(super) fn hls_initial_manifest_decision_wait_timeout(app_state: &Arc<AppState>) -> Duration {
    Duration::from_secs(app_state.hls_proxy.initial_manifest_wait_timeout_secs())
}

pub(super) async fn hls_manifest_wait_timeout_for_requirement(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    requirement: HlsManifestCommitRequirement,
) -> Duration {
    match requirement {
        HlsManifestCommitRequirement::FreshCommitRequired { .. } => {
            hls_initial_manifest_decision_wait_timeout(app_state)
        }
        HlsManifestCommitRequirement::CommittedManifestAllowed => {
            hls_initial_manifest_wait_timeout(app_state, session).await
        }
    }
}

pub(super) async fn touch_initial_manifest_access_lease_window(
    app_state: &Arc<AppState>,
    access_lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    access_lease_state: HlsAccessLeaseState,
    wait_timeout: Duration,
    now_ms: u64,
) {
    if wait_timeout.is_zero() || access_lease_state != HlsAccessLeaseState::Pending {
        return;
    }
    let wait_timeout_ms = duration_to_millis_saturating(wait_timeout);
    let deadline_ms = now_ms.saturating_add(wait_timeout_ms.max(hls_pending_bootstrap_window_ms(app_state)));
    let touch = app_state
        .hls_proxy
        .touch_manifest_access_lease(
            access_lease_id,
            proxy_session_id,
            now_ms,
            None,
            Some(HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms }),
            hls_access_lease_ttl_ms(app_state),
        )
        .await;
    let failure = match touch {
        HlsAccessLeaseTouch::Touched { .. } => return,
        HlsAccessLeaseTouch::Expired => "expired",
        HlsAccessLeaseTouch::Denied => "denied",
        HlsAccessLeaseTouch::UnknownLease => "unknown-lease",
        HlsAccessLeaseTouch::SessionMismatch => "session-mismatch",
    };
    debug!(
        "HLS initial manifest lease window not extended: lease={} proxy_session={} outcome={failure}",
        safe_hls_access_lease_id(access_lease_id),
        safe_proxy_session_id(proxy_session_id)
    );
}

pub(super) async fn hls_initial_manifest_wait_timeout(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
) -> Duration {
    let session = session.read().await;
    if matches!(
        session.account_binding_protection(current_time_millis()),
        HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired
    ) {
        hls_initial_manifest_decision_wait_timeout(app_state)
    } else {
        Duration::ZERO
    }
}

pub(super) struct HlsCachedManifestRead {
    pub(super) transient_body: Option<HlsCachedTransientManifestRead>,
    pub(super) rendered_body: Option<String>,
    pub(super) should_wait: bool,
    pub(super) wait_for_initial_commit: bool,
}

pub(super) struct HlsCachedTransientManifestRead {
    pub(super) body: Arc<str>,
    pub(super) template: Arc<HlsTransientManifestTemplate>,
    pub(super) source_commit_identity: HlsManifestCommitIdentity,
    pub(super) window_policy: HlsManifestWindowPolicy,
    pub(super) finalized_manifest_generation: Option<TransientManifestGeneration>,
    published_resource_ids: HlsPublishedTransientResourceIds,
}

pub(super) async fn read_hls_cached_manifest(
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

pub(super) struct HlsCachedManifestViewContext<'a> {
    pub(super) proxy_session_id: &'a ProxySessionId,
    pub(super) access_lease_id: &'a HlsAccessLeaseId,
    pub(super) access_lease_state: HlsAccessLeaseState,
    pub(super) strip: &'a crate::model::StripConfig,
    pub(super) server_path: Option<&'a str>,
    pub(super) bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
}

#[derive(Clone, Copy)]
pub(super) enum HlsRuntimeBandwidthLearningContext<'a> {
    Disabled,
    Eligible(&'a ConfigInput),
}

impl HlsCachedManifestViewContext<'_> {
    pub(super) fn new<'a>(
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

    pub(super) fn materialize(
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

    pub(super) async fn finish(
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

pub(super) fn spawn_hls_runtime_bandwidth_persistence(
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
    let hls_proxy = Arc::clone(&app_state.hls_proxy);
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

pub(super) fn hls_bandwidth_persistence_outcome(
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

pub(super) fn hls_cached_manifest_temporarily_unavailable() -> axum::response::Response {
    hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
}

pub(super) fn observe_hls_lease_manifest_snapshot_derivation(
    app_state: &AppState,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    derivation: Result<Option<HlsLeaseManifestSnapshot>, HlsManifestLimitViolation>,
) -> Result<Option<HlsLeaseManifestSnapshot>, ()> {
    match derivation {
        Ok(snapshot) => {
            if let Some(snapshot) = snapshot.as_ref() {
                app_state.hls_proxy.metrics().record_lease_snapshot_segments(snapshot.visible_segments.len());
            }
            Ok(snapshot)
        }
        Err(violation) => {
            app_state.hls_proxy.metrics().record_manifest_limit_rejection();
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
pub(super) async fn try_hls_cached_manifest_response(
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
        .hls_proxy
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
                    .hls_proxy
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
                        app_state.hls_proxy.startup_observability().record_manifest_publication(
                            access_lease_id,
                            snapshot_generation,
                            admission_at_ms,
                            published_at_ms,
                            Arc::from(startup_snapshot.visible_proxy_seqs().collect::<Vec<_>>()),
                        );
                    if first_startup_publication && hls_access_manifest_uses_startup_view(access_lease_state) {
                        app_state.hls_proxy.spawn_access_lease_repair_prewarm(
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

pub(super) async fn wait_for_hls_startup_evidence(started_at: tokio::time::Instant, wait_timeout: Duration) -> bool {
    let elapsed = started_at.elapsed();
    if wait_timeout.is_zero() || elapsed >= wait_timeout {
        return false;
    }
    tokio::time::sleep(wait_timeout.saturating_sub(elapsed).min(Duration::from_millis(25))).await;
    true
}

pub(super) async fn mark_successful_canonical_manifest_activity(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    mark_hls_authorized_media_access(app_state, session, now_ms).await;
}

pub(super) async fn mark_hls_authorized_manifest_access(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    session.write().await.mark_authorized_manifest_access(now_ms);
    app_state.hls_proxy.schedule_session_idle_for_handle(session).await;
}

pub(super) async fn mark_hls_authorized_media_access(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    session.write().await.mark_authorized_media_access(now_ms);
    app_state.hls_proxy.schedule_session_idle_for_handle(session).await;
}

pub(super) fn hls_cache_configured(app_state: &Arc<AppState>) -> bool {
    let config = app_state.app_config.config.load();
    config.reverse_proxy.as_ref().is_some_and(|reverse_proxy| reverse_proxy.hls_cache.is_some())
}

pub(super) fn hls_cache_enabled_for_target(app_state: &Arc<AppState>, target: &ConfigTarget) -> bool {
    hls_cache_configured(app_state) && is_hls_stream_share_enabled(target)
}

pub(super) struct HlsAccessManifestRequestContext {
    pub(super) input: Arc<ConfigInput>,
    pub(super) hls_url: String,
    pub(super) session_entry_url: HlsOriginEntryUrl,
    pub(super) original_hls_entry_path: String,
    pub(super) origin_source: HlsOriginSource,
    pub(super) headers: HeaderMap,
    pub(super) server_path: Option<String>,
}

pub(super) async fn resolve_hls_playback_manifest_request_context(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    req_headers: &HeaderMap,
) -> Result<HlsAccessManifestRequestContext, StatusCode> {
    let Some((user, target)) = app_state.app_config.get_target_for_username(&access_context.username) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !hls_cache_enabled_for_target(app_state, &target) {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(input) = app_state.app_config.get_input_by_id(access_context.input_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if app_state
        .active_users
        .is_user_blocked_for_stream(&user.username, VirtualId::new(access_context.virtual_id))
        .await
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(channel) = get_stream_channel(app_state, &target, access_context.virtual_id).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    let origin_playlist_url = if let Some(archive_url) = access_context.archive_origin_url.as_ref() {
        archive_url.clone()
    } else {
        resolve_hls_origin_playlist_url(app_state, &target, &input, access_context.virtual_id, channel.url.as_ref())
            .await?
    };
    let Some(hls_cache_origin) = build_hls_origin_resolution(&input, &origin_playlist_url) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let origin_source = build_hls_origin_source_for_playback(
        &input,
        access_context.stream_ref.clone(),
        access_context.epg_reference_ts,
        Some(&origin_playlist_url),
    );
    let Some(server_info) = app_state.app_config.get_user_server_info(&user) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        channel.upstream_user_agent.as_deref(),
    );

    let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, access_context.virtual_id);

    Ok(HlsAccessManifestRequestContext {
        input,
        hls_url: hls_cache_origin.hls_url,
        session_entry_url: hls_cache_origin.session_entry_url,
        original_hls_entry_path,
        origin_source,
        headers,
        server_path: server_info.path.clone(),
    })
}

pub(super) async fn hls_manifest_preflight_refresh_ordering(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    lease: &HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Result<HlsManifestRefreshOrdering, Box<axum::response::Response>> {
    match hls_manifest_terminal_preflight(session, lease, now_ms).await {
        HlsManifestTerminalPreflight::ServeCommittedPlayback => {
            Err(Box::new(hls_terminal_playback_response(lease, proxy_session_id, access_lease_id).unwrap_or_else(
                || hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::RuntimeUnavailable),
            )))
        }
        HlsManifestTerminalPreflight::BootstrapPendingLease => Ok(HlsManifestRefreshOrdering::Background),
        HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation => {
            Ok(HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation)
        }
        HlsManifestTerminalPreflight::EvaluateTerminal => resolve_hls_terminal_manifest_state(
            app_state,
            session,
            proxy_session_id,
            access_lease_id,
            lease.clone(),
            now_ms,
        )
        .await
        .map(|_| HlsManifestRefreshOrdering::Background),
        HlsManifestTerminalPreflight::FailClosed { reason } => {
            Err(Box::new(hls_terminal_failed_closed_response(reason)))
        }
    }
}

pub(super) async fn hls_proxy_manifest(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyManifestPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let access_lease_id = HlsAccessLeaseId(params.hls_access_lease_id);
    let now_ms = current_time_millis();
    let access_lease_snapshot =
        app_state.hls_proxy.access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms).await;
    if let Some(lease) = access_lease_snapshot.as_ref() {
        let standalone_policy_response_required = lease.playback_mode == HlsLeasePlaybackMode::Ended
            && lease
                .runtime_policy_denial_reason()
                .is_some_and(HlsRuntimeCustomTailReason::permits_unpublished_lease_standalone_tail);
        if let (false, Some(response)) = (
            standalone_policy_response_required,
            hls_terminal_playback_response(lease, &proxy_session_id, &access_lease_id),
        ) {
            return response;
        }
    }
    let session = app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await;
    if let Some(session) = session.as_ref() {
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                session,
                &proxy_session_id,
                now_ms,
            )
            .await;
    }
    let (access_context, access_lease_state) = match hls_manifest_access_context_and_state(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &access_lease_id,
        access_lease_snapshot.as_ref(),
        now_ms,
    )
    .await
    {
        Ok(context_and_state) => context_and_state,
        Err(response) => return *response,
    };
    let refresh_ordering = if let (Some(session), Some(lease)) = (session.as_ref(), access_lease_snapshot.as_ref()) {
        match hls_manifest_preflight_refresh_ordering(
            &app_state,
            session,
            lease,
            &proxy_session_id,
            &access_lease_id,
            now_ms,
        )
        .await
        {
            Ok(ordering) => ordering,
            Err(response) => return *response,
        }
    } else {
        HlsManifestRefreshOrdering::Background
    };
    let request_context =
        match resolve_hls_playback_manifest_request_context(&app_state, &access_context, &headers).await {
            Ok(context) => context,
            Err(status) => return hls_canonical_status_response(status),
        };

    try_hls_cache_canonical_manifest_response(
        &app_state,
        &fingerprint,
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        access_lease_state,
        HlsCacheManifestOrigin {
            raw_request_url: &request_context.hls_url,
            session_entry_url: request_context.session_entry_url.clone(),
            input: &request_context.input,
            origin_source: request_context.origin_source,
        },
        request_context.headers,
        request_context.server_path.as_deref(),
        &request_context.original_hls_entry_path,
        refresh_ordering,
    )
    .await
    .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}
