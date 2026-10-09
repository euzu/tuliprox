use super::{hls_canonical_manifest_path, HlsEntryMasterPlaylistResponse};
use crate::{
    api::{
        api_utils::{record_connect_failed_attempt, try_unwrap_body, ConnectFailedAttempt},
        model::{
            hls_cache::initial_strip::{
                materialize_initial_hls_strip_view, HlsInitialStripOutcome, HlsInitialStripSkipReason,
            },
            hls_custom_video_manifest_response_with_virtual_id, AppState, CustomVideoStreamType,
        },
    },
    auth::Fingerprint,
    model::ProxyUserCredentials,
    processing::parser::hls::origin_manifest::HlsManifestWindowPolicy,
};
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use shared::model::{ConnectFailureReason, FailureStage, StreamChannel};
use std::{borrow::Cow, sync::Arc};
use tuliprox_hls::api::{
    HlsAccessLeaseId, HlsAccessLeaseState, HlsMasterBandwidth, HlsSingleVariantMasterPlaylist, ProxySessionId,
    HLS_ACCESS_LEASE_ID_PLACEHOLDER,
};

#[derive(Debug, Deserialize)]
pub(in crate::api::endpoints::hls_api) struct HlsProxyTerminalSegmentPathParams {
    pub(in crate::api::endpoints::hls_api) proxy_session_id: String,
    pub(in crate::api::endpoints::hls_api) hls_access_lease_id: String,
    pub(in crate::api::endpoints::hls_api) generation: String,
    pub(in crate::api::endpoints::hls_api) terminal_file: String,
}

pub(in crate::api::endpoints::hls_api) fn hls_custom_video_type_for_failure_reason(
    reason: ConnectFailureReason,
) -> CustomVideoStreamType {
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

pub(in crate::api::endpoints::hls_api) fn apply_hls_proxy_public_path_prefix(
    hls_content: String,
    server_path: Option<&str>,
) -> String {
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

pub(in crate::api::endpoints::hls_api) fn normalize_hls_proxy_public_path_prefix(
    server_path: Option<&str>,
) -> Option<String> {
    let path = server_path?.trim().trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(format!("/{path}"))
}

pub(in crate::api::endpoints::hls_api) fn split_hls_line_ending(part: &str) -> (&str, &str) {
    if let Some(line) = part.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = part.strip_suffix('\n') {
        (line, "\n")
    } else {
        (part, "")
    }
}

pub(in crate::api::endpoints::hls_api) fn materialize_hls_access_manifest(
    hls_content: &str,
    lease_id: &HlsAccessLeaseId,
    server_path: Option<&str>,
) -> String {
    let hls_content = hls_content.replace(HLS_ACCESS_LEASE_ID_PLACEHOLDER, &lease_id.0);
    apply_hls_proxy_public_path_prefix(hls_content, server_path)
}

pub(in crate::api::endpoints::hls_api) fn hls_access_manifest_uses_startup_view(
    lease_state: HlsAccessLeaseState,
) -> bool {
    matches!(lease_state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Idle)
}

pub(in crate::api::endpoints::hls_api) fn materialize_shared_hls_access_manifest(
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

pub(in crate::api::endpoints::hls_api) struct HlsMaterializedSharedManifest {
    pub(in crate::api::endpoints::hls_api) body: String,
    pub(in crate::api::endpoints::hls_api) mode: &'static str,
    pub(in crate::api::endpoints::hls_api) initial_strip_outcome: Option<HlsInitialStripOutcome>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsInitialStripLeaseSkipReason {
    LeaseActivated,
    LeaseNotStartupView,
}

impl HlsInitialStripLeaseSkipReason {
    pub(in crate::api::endpoints::hls_api) const fn as_log_reason(self) -> &'static str {
        match self {
            Self::LeaseActivated => "lease-activated",
            Self::LeaseNotStartupView => "lease-not-startup-view",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsInitialStripPublicationDiagnostic {
    Applied { mode: &'static str, strip_mode: &'static str, configured: u64, effective: usize, visible_segments: usize },
    Skipped { mode: &'static str, reason: HlsInitialStripSkipReason, visible_segments: usize },
    SkippedForLeaseState { mode: &'static str, reason: HlsInitialStripLeaseSkipReason },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsInitialStripPublicationStatus {
    NotCommitted,
    Committed,
}

pub(in crate::api::endpoints::hls_api) fn hls_initial_strip_publication_diagnostic(
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

pub(in crate::api::endpoints::hls_api) fn hls_entry_master_playlist_response(
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
