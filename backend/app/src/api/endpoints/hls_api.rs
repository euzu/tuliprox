#![allow(clippy::large_futures)]

// Cross-import of xtream URL helpers is routed through `xtream_url` (a one-way
// re-export module) so this endpoint file no longer depends on a sibling
// endpoint file directly. See `xtream_url`'s module docs for the ARCH-1
// roadmap that motivates this re-export.
use super::{
    hls_terminal_response::{
        hls_manifest_terminal_preflight, hls_response, hls_temporary_resource_unavailable_response,
        hls_terminal_failed_closed_response, hls_terminal_playback_response, resolve_hls_terminal_manifest_state,
        terminal_segment_get_response, terminal_segment_head_response, terminal_segment_immutable_replay_response,
        terminal_tail_plan_for_current_route, HlsManifestTerminalPreflight,
    },
    xtream_url::{get_query_path, get_xtream_player_api_stream_url, ApiStreamContext},
};
use crate::{
    api::{
        api_utils::{
            connection_priority_for_kind, create_api_proxy_user, create_m3u_catchup_session_key,
            create_playback_session_fingerprint, create_session_fingerprint, force_provider_stream_response,
            get_headers_from_request, get_hls_session_ttl_secs, get_stream_alternative_url,
            is_hls_stream_share_enabled, is_seekable_media_request, local_stream_response,
            record_connect_failed_attempt, resolve_playback_request_admission, try_option_bad_request, try_unwrap_body,
            ConnectFailedAttempt, EvictionReentryGuard, HeaderFilter,
        },
        model::{
            hls_cache::initial_strip::{
                materialize_initial_hls_strip_view, HlsInitialStripOutcome, HlsInitialStripSkipReason,
            },
            hls_custom_video_manifest_response_for_access_lease, hls_custom_video_manifest_response_with_virtual_id,
            hls_provisioning_discontinuity_sequence, hls_virtual_entry_redirect_response,
            is_custom_video_stream_enabled, start_hls_panel_provisioning_once,
            try_hls_panel_provisioning_manifest_response, AppState, ConnectionHistoryMode, CustomVideoStreamType,
            GraceMode, HlsPanelProvisioningRedirectPaths, HlsProvisioningStatus, ProviderAllocation,
            ProviderConfig as RuntimeProviderConfig, ProviderHandle, StreamMeterHandle, TransportStreamBuffer,
            UserSession,
        },
        panel_api::can_provision_on_exhausted,
    },
    auth::{check_network_access_only, Fingerprint},
    model::{
        ConfigInput, ConfigInputFlags, ConfigProvider, ConfigTarget, InputSource, ProxyUserCredentials,
        ReverseProxyDisabledHeaderConfig,
    },
    processing::parser::hls::{get_hls_session_token_and_url_from_token, rewrite_hls, RewriteHlsProps},
    repository::{
        load_input_live_bitrate_bps, m3u_get_item_for_stream_id, persist_input_live_bitrate_bps, storage_const,
        xtream_get_item_for_stream_id, LiveBitratePersistenceOutcome,
    },
    utils::{content_coding::OutboundContentCodingPolicy, debug_if_enabled, request, request::is_file_url},
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::IntoResponse,
};
use futures::FutureExt;
use log::{debug, error, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shared::{
    defaults::HLS_EXT,
    model::{
        ConnectFailureReason, FailureStage, InputType, PlaylistEntry, PlaylistItemType, StreamChannel, StreamInfo,
        StreamProperties, TargetType, UserConnectionPermission, XtreamCluster,
    },
    utils::{
        extract_extension_from_url, generate_random_string, is_hls_url, is_m3u_catchup_session_token,
        replace_url_extension, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX,
    },
};
use std::{borrow::Cow, collections::HashMap, sync::Arc, time::Duration};
use tuliprox_hls::{
    api::{
        begin_hls_origin_account_io_bounded, build_hls_origin_session_owner, build_proxy_session_id,
        cold_start_retry_after_seconds, commit_hls_runtime_custom_tail, derive_hls_lease_manifest_snapshot,
        extract_hls_provider_session_headers, fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
        fetch_hls_transient_origin_response_with_attempt_prepare, finite_hls_terminal_key_response,
        force_identity_without_range, hls_cached_manifest_options_for_requirement,
        hls_committed_manifest_body_for_request, hls_manifest_acceptance_directive_for_session,
        hls_manifest_commit_requirement, hls_object_body_deadline, hls_origin_account_status,
        hls_should_wait_for_initial_manifest_commit, hls_startup_admission_allows_snapshot,
        hls_transient_object_fetch_failure, hls_transient_origin_response, is_hls_provisioning_gap_segment,
        is_hls_provisioning_segment, maybe_trigger_origin_refresh_with_outcome, new_hls_access_lease_id,
        origin_account_binding_from_allocation, record_successful_transient_segment_fetch,
        record_temporary_transient_segment_fetch_failure, register_hls_availability_reevaluation,
        resolve_hls_transient_object_cache_action, safe_hls_access_lease_id, safe_proxy_session_id, safe_session_key,
        safe_user_session_token, scrub_hls_origin_headers, serve_hls_map_cache_outcome,
        serve_hls_segment_cache_outcome, serve_hls_transient_object_cache_outcome,
        serve_hls_transient_object_cache_response, should_remove_hls_origin_header, trigger_origin_refresh_sync,
        validate_hls_access_lease, CacheAccessState, HlsAccessAdmissionMode, HlsAccessContext, HlsAccessLease,
        HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseState,
        HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsAccessLeaseValidationError, HlsAccountBindingProtection,
        HlsAccountOverlapTiming, HlsAvailabilityReevaluationObservation, HlsAvailabilityReevaluationRegistration,
        HlsBandwidthPersistenceOutcome, HlsBoundAccountAcquireErrorKind, HlsCacheResponseContext,
        HlsCachedManifestOptions, HlsCommittedManifestBody, HlsEffectiveOriginAcquirePolicy,
        HlsLeaseManifestSnapshotInput, HlsLeasePlaybackMode, HlsLeaseStartupAdmissionState, HlsLogIdentity,
        HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome, HlsManifestCommitRequirement,
        HlsMapFile, HlsMasterBandwidth, HlsMasterBandwidthSelection, HlsMediaActivityCommitOutcome,
        HlsMediaActivityMarker, HlsMediaLeaseIdentity, HlsOriginAccountBinding, HlsOriginAccountBindingMode,
        HlsOriginAccountDetachedReason, HlsOriginAccountStatus, HlsOriginIoContext, HlsOriginRefreshTriggerOutcome,
        HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginSource, HlsOriginSourceKind,
        HlsOriginWorkClass, HlsPlaybackFamilyKey, HlsPostRefreshRuntime, HlsQosMeterInit, HlsQosRuntimeConfig,
        HlsResourceFetchAttempt, HlsResourceServeFailure, HlsResourceServeOutcome, HlsRuntimeCustomTailOutcome,
        HlsRuntimeCustomTailReason, HlsRuntimeCustomTailRequest, HlsSegmentFile, HlsSession, HlsSessionHandle,
        HlsSessionKey, HlsSessionMode, HlsSessionStoreOutcome, HlsSingleVariantMasterPlaylist,
        HlsTerminalFailedClosedReason, HlsTerminalSegmentPath, HlsTransientCacheCommitContext,
        HlsTransientDecodedOriginResponse, HlsTransientDirectResponseContext, HlsTransientObjectCacheAction,
        HlsTransientObjectFetchFailure, HlsTransientObjectFetchFinalizer, HlsTransientOriginCacheFetchRequest,
        HlsTransientOriginFetchRequest, HlsTransientOriginIoGuard, LiveHlsOriginEntry, OriginRefreshRequest,
        OriginSegmentKey, ProxySessionId, RetryPolicy, SegmentCacheKey, SegmentCacheStatus, SegmentDemandFetchOutcome,
        SegmentEntry, SegmentFetchContext, SegmentFetchPolicy, TransientObjectFetchToken,
        TransientObjectUnavailableState, TransientPassthroughState, TransientResourceFile, TransientResourceId,
        TransientResourceRef, HLS_ACCESS_LEASE_ID_PLACEHOLDER, HLS_PROVISIONING_GAP_ORIGIN_EPOCH,
        HLS_PROVISIONING_ORIGIN_EPOCH, HLS_PROVISIONING_SEGMENT_DURATION_MS, HLS_PROVISIONING_TARGET_DURATION_SECS,
        MAX_HLS_MANIFEST_BYTES,
    },
    HlsCtx, MAX_MANUAL_REDIRECTS,
};
use url::Url;

const HLS_TEMPORARY_RESOURCE_RETRY_AFTER_SECS: u64 = 1;
const HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS: u64 = HLS_TEMPORARY_RESOURCE_RETRY_AFTER_SECS * 1_000;
/// Poll interval while waiting for a canonical manifest commit. Lower values
/// reduce time-to-first-manifest at the cost of more wakeups per waiting client.
const HLS_MANIFEST_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Recover archive EPG reference from `m3u-catchup|...|archive|{start}|{duration}` session keys.
///
/// `BitTV` archive media URLs look like `2026/07/24/14/13/38-06800.ts` and lose Flussonic
/// path markers after HLS rewrite, so the panel would otherwise keep showing Live + live EPG.
pub(in crate::api) fn m3u_catchup_epg_reference_from_session_token(session_token: &str) -> Option<i64> {
    let rest = session_token.strip_prefix("m3u-catchup|")?;
    for marker in ["|archive|", "|timeshift_abs|"] {
        if let Some(idx) = rest.rfind(marker) {
            let after = &rest[idx + marker.len()..];
            let start = after.split('|').next()?.trim();
            if let Ok(ts) = start.parse::<i64>() {
                return Some(ts);
            }
        }
    }
    None
}

fn resolve_m3u_archive_reference(stream_url: &str, session_token: Option<&str>) -> Option<i64> {
    m3u_archive_epg_reference_ts(stream_url)
        .or_else(|| epg_reference_ts_from_date_tree_path(stream_url))
        .or_else(|| session_token.and_then(m3u_catchup_epg_reference_from_session_token))
}

fn looks_like_archive_media_path(path: &str) -> bool {
    let rel = path.trim_start_matches('/');
    if rel.is_empty() {
        return false;
    }
    rel.starts_with("dvr-") || rel.contains("/dvr-") || epg_reference_ts_from_date_tree_path(rel).is_some()
}

/// `BitTV` / Flussonic date-tree segments: `YYYY/MM/DD/HH/MM/SS-*.ts` or `dvr-YYYY/...`.
pub(in crate::api) fn epg_reference_ts_from_date_tree_path(path: &str) -> Option<i64> {
    let owned_path;
    let mut rel = path.trim_start_matches('/');
    if let Some(idx) = rel.find('?') {
        rel = &rel[..idx];
    }
    if rel.contains("://") {
        let parsed = Url::parse(rel).ok()?;
        owned_path = parsed.path().trim_start_matches('/').to_string();
        rel = owned_path.as_str();
    }
    if let Some(rest) = rel.strip_prefix("dvr-") {
        rel = rest;
    }
    let mut parts = rel.split('/');
    let year: i32 = parts.next()?.parse().ok()?;
    if !(2000..=2100).contains(&year) {
        return None;
    }
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let sec_token = parts.next()?.split('-').next()?.trim_end_matches(".ts").trim_end_matches(".m3u8");
    let second: u32 = sec_token.parse().ok()?;
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, second)?;
    Some(naive.and_utc().timestamp())
}

/// Join a client-leaked relative DVR/media path against the session's origin URL.
///
/// When an origin `.m3u8` is force-piped without `rewrite_hls`, players resolve
/// `dvr-2026/...ts?token=` against the proxy playlist URL (`/hls/.../{token}.m3u8`).
fn resolve_leaked_hls_relative_origin(
    session_stream_url: &str,
    relative_path: &str,
    request_query: Option<&str>,
) -> Option<String> {
    let rel = relative_path.trim_start_matches('/');
    if rel.is_empty() || rel.contains("://") {
        return None;
    }
    // Only recover archive-style relative paths (BitTV/Flussonic DVR or date trees).
    if !looks_like_archive_media_path(rel) {
        return None;
    }
    let parsed = url::Url::parse(session_stream_url).ok()?;
    let session_path = parsed.path();

    // If the session URL is already inside a DVR/date tree, strip back to the stream root
    // so sibling relative segments do not nest under the previous segment directory.
    let joined = if rel.starts_with("dvr-") {
        if let Some(idx) = session_path.find("/dvr-") {
            let mut joined = parsed.clone();
            joined.set_path(&format!("{}{}", &session_path[..=idx], rel));
            joined.set_query(None);
            joined.into()
        } else {
            parsed.join(rel).ok()?.into()
        }
    } else if let Some(idx) = session_path.find("/202") {
        let mut joined = parsed.clone();
        joined.set_path(&format!("{}/{}", &session_path[..idx], rel));
        joined.set_query(None);
        joined.into()
    } else {
        parsed.join(rel).ok()?.into()
    };

    if let Some(query) = request_query.filter(|q| !q.is_empty()) {
        Some(format!("{joined}?{query}"))
    } else {
        Some(joined)
    }
}

fn legacy_hls_route_allowed_with_cache(
    cache_enabled: bool,
    decoded_session_token: Option<&str>,
    existing_session_token: Option<&str>,
) -> bool {
    !cache_enabled
        || decoded_session_token.is_some_and(|decoded| {
            existing_session_token.is_some_and(|existing| decoded == existing && is_m3u_catchup_session_token(existing))
        })
}

fn query_flag_is_archive(key: &str) -> bool { key.eq_ignore_ascii_case("utc") || key.eq_ignore_ascii_case("utcstart") }

fn query_flag_marks_start_context(key: &str) -> bool {
    key.eq_ignore_ascii_case("end")
        || key.eq_ignore_ascii_case("duration")
        || key.eq_ignore_ascii_case("lutc")
        || key.eq_ignore_ascii_case("offset")
}

pub(in crate::api) fn m3u_archive_epg_reference_ts(stream_url: &str) -> Option<i64> {
    use crate::iptv::m3u::parse_flussonic_archive_file;

    let parsed = Url::parse(stream_url).ok()?;
    // Flussonic / TiviMate path forms: archive|index|video|mono-{utc}-{duration}.m3u8
    // and timeshift_abs / timeshift_rel. Without this, HLS sessions stay LiveHls in the panel.
    if let Some(file) = parsed.path_segments().and_then(|mut segments| segments.next_back()) {
        if let Some(archive) = parse_flussonic_archive_file(file) {
            if let Some(ts) = archive.epg_reference_ts() {
                return Some(ts);
            }
        }
    }
    // BitTV date-tree: /YYYY/MM/DD/HH/MM/SS-*.ts
    if let Some(ts) = epg_reference_ts_from_date_tree_path(parsed.path()) {
        return Some(ts);
    }
    let mut start_ts = None;
    let mut has_start_context = false;
    for (key, value) in parsed.query_pairs() {
        if query_flag_is_archive(&key) {
            if let Ok(ts) = value.parse::<i64>() {
                return Some(ts);
            }
        } else if key.eq_ignore_ascii_case("start") || key.eq_ignore_ascii_case("timestamp") {
            start_ts = value.parse::<i64>().ok();
        } else if query_flag_marks_start_context(&key) {
            has_start_context = true;
        }
    }

    has_start_context.then_some(start_ts).flatten()
}

#[derive(Debug, Deserialize)]
struct HlsApiPathParams {
    username: String,
    password: String,
    target_id: u16,
    input_id: u16,
    stream_id: u32,
    /// Single obfuscated token, or a leaked relative origin path (`dvr-YYYY/...ts`).
    token: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxySegmentPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    segment_file: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyManifestPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyMapPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    map_file: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyResourcePathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    resource_file: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyTerminalSegmentPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    generation: String,
    terminal_file: String,
}

fn hls_custom_video_type_for_failure_reason(reason: ConnectFailureReason) -> CustomVideoStreamType {
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

fn apply_hls_proxy_public_path_prefix(hls_content: String, server_path: Option<&str>) -> String {
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

fn normalize_hls_proxy_public_path_prefix(server_path: Option<&str>) -> Option<String> {
    let path = server_path?.trim().trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(format!("/{path}"))
}

fn split_hls_line_ending(part: &str) -> (&str, &str) {
    if let Some(line) = part.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = part.strip_suffix('\n') {
        (line, "\n")
    } else {
        (part, "")
    }
}

fn materialize_hls_access_manifest(
    hls_content: &str,
    lease_id: &HlsAccessLeaseId,
    server_path: Option<&str>,
) -> String {
    let hls_content = hls_content.replace(HLS_ACCESS_LEASE_ID_PLACEHOLDER, &lease_id.0);
    apply_hls_proxy_public_path_prefix(hls_content, server_path)
}

fn hls_access_manifest_uses_startup_view(lease_state: HlsAccessLeaseState) -> bool {
    matches!(lease_state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Idle)
}

fn materialize_shared_hls_access_manifest(
    hls_content: &str,
    lease_id: &HlsAccessLeaseId,
    lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    mode: &'static str,
    server_path: Option<&str>,
) -> HlsMaterializedSharedManifest {
    let (response_body, initial_strip_outcome) = if hls_access_manifest_uses_startup_view(lease_state) {
        let view = materialize_initial_hls_strip_view(hls_content, strip);
        (view.body, Some(view.outcome))
    } else {
        (hls_content.to_string(), None)
    };
    HlsMaterializedSharedManifest {
        body: materialize_hls_access_manifest(&response_body, lease_id, server_path),
        mode,
        initial_strip_outcome,
    }
}

struct HlsMaterializedSharedManifest {
    body: String,
    mode: &'static str,
    initial_strip_outcome: Option<HlsInitialStripOutcome>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsInitialStripLeaseSkipReason {
    LeaseActivated,
    LeaseNotStartupView,
}

impl HlsInitialStripLeaseSkipReason {
    const fn as_log_reason(self) -> &'static str {
        match self {
            Self::LeaseActivated => "lease-activated",
            Self::LeaseNotStartupView => "lease-not-startup-view",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsInitialStripPublicationDiagnostic {
    Applied { mode: &'static str, strip_mode: &'static str, configured: u64, effective: usize, visible_segments: usize },
    Skipped { mode: &'static str, reason: HlsInitialStripSkipReason, visible_segments: usize },
    SkippedForLeaseState { mode: &'static str, reason: HlsInitialStripLeaseSkipReason },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsInitialStripPublicationStatus {
    NotCommitted,
    Committed,
}

fn hls_initial_strip_publication_diagnostic(
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

fn log_hls_initial_strip_publication(
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    diagnostic: HlsInitialStripPublicationDiagnostic,
) {
    match diagnostic {
        HlsInitialStripPublicationDiagnostic::Applied { mode, strip_mode, configured, effective, visible_segments } => {
            debug!(
                "HLS initial strip applied: mode={} lease={} proxy_session={} strip_mode={} configured={} effective={} visible_segments={}",
                mode,
                safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id),
                strip_mode,
                configured,
                effective,
                visible_segments
            );
        }
        HlsInitialStripPublicationDiagnostic::Skipped { mode, reason, visible_segments } => {
            debug!(
                "HLS initial strip skipped: mode={} lease={} proxy_session={} reason={} visible_segments={}",
                mode,
                safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id),
                reason.as_log_reason(),
                visible_segments
            );
        }
        HlsInitialStripPublicationDiagnostic::SkippedForLeaseState { mode, reason } => {
            debug!(
                "HLS initial strip skipped: mode={} lease={} proxy_session={} reason={}",
                mode,
                safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id),
                reason.as_log_reason()
            );
        }
    }
}

fn stripped_tail_segments(materialized: &HlsMaterializedSharedManifest) -> usize {
    materialized.initial_strip_outcome.as_ref().map_or(0, |outcome| match outcome {
        HlsInitialStripOutcome::Applied { effective, .. } => *effective,
        HlsInitialStripOutcome::Skipped { .. } => 0,
    })
}

fn hls_access_lease_ttl_ms(app_state: &Arc<AppState>) -> u64 { app_state.hls_proxy.session_idle_timeout_ms() }

fn duration_to_millis_saturating(duration: Duration) -> u64 { u64::try_from(duration.as_millis()).unwrap_or(u64::MAX) }

fn hls_pending_bootstrap_window_ms(app_state: &Arc<AppState>) -> u64 {
    duration_to_millis_saturating(hls_initial_manifest_decision_wait_timeout(app_state))
}

async fn hls_access_lease_timing_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
) -> HlsAccessLeaseTiming {
    let timing = session.read().await.account_overlap_timing();
    let active_window_ms = timing.hard_active_window_ms.saturating_mul(2);
    HlsAccessLeaseTiming { active_window_ms, valid_window_ms: hls_access_lease_ttl_ms(app_state) }
}

async fn touch_pending_manifest_follow_up_window(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
) {
    if access_lease_state != HlsAccessLeaseState::Pending {
        return;
    }
    let (proxy_session_id, target_duration) = {
        let session = session.read().await;
        (session.proxy_session_id.clone(), session.target_duration)
    };
    let now_ms = current_time_millis();
    if !app_state
        .hls_proxy
        .mark_pending_manifest_follow_up_for_lease(access_lease_id, &proxy_session_id, now_ms, target_duration)
        .await
    {
        debug!(
            "HLS pending manifest follow-up skipped: lease={} proxy_session={} reason=expired-or-generation-race",
            safe_hls_access_lease_id(access_lease_id),
            safe_proxy_session_id(&proxy_session_id)
        );
    }
}

struct HlsResourceAccess {
    session: HlsSessionHandle,
    access_context: HlsAccessContext,
    lease: HlsAccessLease,
}

async fn prepare_hls_resource_access(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    request_kind: &'static str,
) -> Result<HlsResourceAccess, Box<axum::response::Response>> {
    let Some(session) = app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    app_state
        .hls_proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            proxy_session_id,
            now_ms,
        )
        .await;
    let access_context = match validate_hls_proxy_access_request(
        app_state,
        fingerprint,
        proxy_session_id,
        hls_access_lease_id,
        now_ms,
        hls_access_lease_timing_for_session(app_state, &session).await,
        request_kind,
    )
    .await
    {
        Ok(context) => context,
        Err(err) => {
            return Err(Box::new(hls_resource_access_lease_validation_response(&err)));
        }
    };
    app_state
        .hls_proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            proxy_session_id,
            now_ms,
        )
        .await;
    reclaim_hls_account_overlap_if_needed(app_state, &session, now_ms).await;
    let Some(lease) =
        app_state.hls_proxy.access_lease_response_snapshot(&access_context.lease_id, proxy_session_id, now_ms).await
    else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    Ok(HlsResourceAccess { session, access_context, lease })
}

fn hls_lease_allows_live_origin_work(lease: &HlsAccessLease) -> bool {
    matches!(lease.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
        && lease.playback_mode == HlsLeasePlaybackMode::Live
}

fn hls_lease_allows_cached_segment(lease: &HlsAccessLease, proxy_seq: u64) -> bool {
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => true,
        HlsLeasePlaybackMode::TerminalTail(plan) => plan.protected_base_proxy_seqs.contains(&proxy_seq),
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => false,
    }
}

async fn current_hls_resource_lease(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
) -> Option<HlsAccessLease> {
    app_state
        .hls_proxy
        .access_lease_response_snapshot(
            &access_context.lease_id,
            &access_context.proxy_session_id,
            current_time_millis(),
        )
        .await
}

async fn hls_live_lease_identity_is_current(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    expected_identity: HlsMediaLeaseIdentity,
) -> bool {
    current_hls_resource_lease(app_state, access_context).await.is_some_and(|lease| {
        lease.playback_mode == HlsLeasePlaybackMode::Live && lease.media_identity() == Some(expected_identity)
    })
}

fn create_hls_cache_user_session_token(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    existing_session_token: Option<&str>,
    archive_reference: Option<i64>,
) -> String {
    let base =
        hls_entry_user_session_token(fingerprint, username, virtual_id, existing_session_token, archive_reference);
    format!("{base}|hls-cache|{}", generate_random_string(16))
}

fn is_hls_media_activity_status(status: StatusCode) -> bool {
    matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
}

async fn hls_cache_response_context(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    now_ms: u64,
) -> HlsCacheResponseContext {
    let qos_meter = app_state.hls_proxy.qos().meter_for_access_lease(&access_context.lease_id).await;
    let log_identity = {
        let session = session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    HlsCacheResponseContext::new(
        access_context.lease_id.clone(),
        log_identity,
        app_state.hls_proxy.cache_duration_seconds(),
        Arc::clone(app_state.hls_proxy.metrics()),
        Arc::clone(app_state.hls_proxy.segment_repair()),
        qos_meter,
        Some(HlsMediaActivityMarker::new(
            Arc::clone(&app_state.hls_proxy),
            Arc::clone(session),
            access_context.proxy_session_id.clone(),
            access_context.lease_id.clone(),
            lease_identity,
        )),
        now_ms,
    )
}

fn hls_qos_meter_init(app_state: &Arc<AppState>, qos_config: HlsQosRuntimeConfig) -> Option<HlsQosMeterInit> {
    if !qos_config.live_metering_enabled {
        return None;
    }
    let meter_uid = app_state.connection_manager.next_stream_uid();
    let meter = Arc::new(StreamMeterHandle::new(meter_uid, Arc::downgrade(&app_state.event_manager)));
    Some(HlsQosMeterInit { meter_uid, meter })
}

async fn register_hls_cache_stream_for_successful_media_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    access_context: &HlsAccessContext,
    session: &HlsSessionHandle,
    response_context: &HlsCacheResponseContext,
) {
    if ensure_hls_cache_stream_registered(app_state, fingerprint, headers, access_context, session).await.is_none() {
        debug!(
            "HLS media registration skipped: lease={} reason=session-or-connection-unavailable",
            safe_hls_access_lease_id(&access_context.lease_id)
        );
    }
    response_context.set_qos_meter(app_state.hls_proxy.qos().meter_for_access_lease(&access_context.lease_id).await);
}

async fn hls_proxy_segment(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxySegmentPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context, lease } = match prepare_hls_resource_access(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &params.hls_access_lease_id,
        now_ms,
        "segment",
    )
    .await
    {
        Ok(access) => access,
        Err(response) => return *response,
    };
    let Some(segment_file) = HlsSegmentFile::parse(&params.segment_file) else {
        return hls_resource_channel_unavailable_response(&app_state, &access_context);
    };
    if !hls_lease_allows_cached_segment(&lease, segment_file.proxy_seq) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let allows_origin_work = hls_lease_allows_live_origin_work(&lease);
    if let Err(response) =
        validate_hls_segment_entry(&app_state, &session, &access_context, &segment_file, allows_origin_work).await
    {
        return *response;
    }
    let demand_result = if allows_origin_work {
        demand_fetch_hls_live_segment(
            &app_state,
            &session,
            &segment_file,
            &access_context,
            &fingerprint,
            &headers,
            now_ms,
        )
        .await
    } else {
        Ok(())
    };
    if let Err(response) = demand_result {
        return *response;
    }

    serve_hls_segment_for_current_lease(
        &app_state,
        &session,
        &access_context,
        &fingerprint,
        &headers,
        segment_file,
        now_ms,
    )
    .await
}

async fn validate_hls_segment_entry(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    segment_file: &HlsSegmentFile,
    allows_origin_work: bool,
) -> Result<(), Box<axum::response::Response>> {
    let session = session.read().await;
    if session.is_gc_marked_for_removal() {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return Err(Box::new(hls_resource_channel_unavailable_response(app_state, access_context)));
    };
    if entry.proxy_file_ext != segment_file.extension {
        return Err(Box::new(hls_resource_channel_unavailable_response(app_state, access_context)));
    }
    if !allows_origin_work && !matches!(&entry.status, SegmentCacheStatus::Ready { .. }) {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    Ok(())
}

async fn demand_fetch_hls_live_segment(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    now_ms: u64,
) -> Result<(), Box<axum::response::Response>> {
    let preacquired_provider_handle = if hls_segment_request_requires_origin_work(session, segment_file).await {
        match prepare_hls_origin_binding_for_authorized_resource_work(
            app_state,
            session,
            access_context,
            fingerprint,
            headers,
            HlsOriginWorkKind::Segment,
            now_ms,
        )
        .await
        {
            Ok(handle) => handle,
            Err(err) => {
                return Err(Box::new(hls_origin_runtime_resource_failure_response(app_state, access_context, err)))
            }
        }
    } else {
        None
    };
    match demand_fetch_hls_segment_if_needed(
        app_state,
        session,
        segment_file,
        access_context,
        fingerprint,
        preacquired_provider_handle,
        now_ms,
    )
    .await
    {
        SegmentDemandFetchOutcome::NotFound => {
            Err(Box::new(hls_resource_channel_unavailable_response(app_state, access_context)))
        }
        SegmentDemandFetchOutcome::Ready
        | SegmentDemandFetchOutcome::QueuedOrFetching
        | SegmentDemandFetchOutcome::Unavailable
        | SegmentDemandFetchOutcome::TimedOut => Ok(()),
    }
}

async fn serve_hls_segment_for_current_lease(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    segment_file: HlsSegmentFile,
    now_ms: u64,
) -> axum::response::Response {
    let Some(current_lease) = current_hls_resource_lease(app_state, access_context).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !hls_lease_allows_cached_segment(&current_lease, segment_file.proxy_seq) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let current_allows_origin_work = hls_lease_allows_live_origin_work(&current_lease);
    let Some(lease_identity) = current_lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response_context = hls_cache_response_context(app_state, session, access_context, lease_identity, now_ms).await;
    let response = hls_resource_serve_outcome_response(
        app_state,
        access_context,
        serve_hls_segment_cache_outcome(
            Arc::clone(app_state.hls_proxy.segment_cache()),
            Arc::clone(session),
            segment_file,
            headers.get(header::RANGE).cloned(),
            &response_context,
        )
        .await,
    );
    if current_allows_origin_work && is_hls_media_activity_status(response.status()) {
        register_hls_cache_stream_for_successful_media_response(
            app_state,
            fingerprint,
            headers,
            access_context,
            session,
            &response_context,
        )
        .await;
    }
    response
}

async fn hls_proxy_terminal_segment(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyTerminalSegmentPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let Some(path) = HlsTerminalSegmentPath::parse(&params.generation, &params.terminal_file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now_ms = current_time_millis();
    let access_lease_id = HlsAccessLeaseId(params.hls_access_lease_id.clone());
    let immutable_replay_plan =
        app_state.hls_proxy.access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms).await.and_then(
            |lease| match lease.playback_mode {
                HlsLeasePlaybackMode::TerminalTail(plan) if plan.matches_route(&proxy_session_id, &access_lease_id) => {
                    Some(plan)
                }
                HlsLeasePlaybackMode::Live
                | HlsLeasePlaybackMode::TerminalTail(_)
                | HlsLeasePlaybackMode::TerminalUnavailable { .. }
                | HlsLeasePlaybackMode::Ended => None,
            },
        );
    let access = prepare_hls_resource_access(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &params.hls_access_lease_id,
        now_ms,
        "terminal-segment",
    )
    .await;
    let Ok(HlsResourceAccess { session, access_context, lease }) = access else {
        return immutable_replay_plan
            .and_then(|plan| {
                terminal_segment_immutable_replay_response(
                    &plan,
                    path,
                    headers.get(header::RANGE),
                    method == Method::HEAD,
                )
            })
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    };
    let Some(current_lease) = current_hls_resource_lease(&app_state, &access_context).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(plan) =
        terminal_tail_plan_for_current_route(&lease, &current_lease, &proxy_session_id, &access_context.lease_id)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if method == Method::HEAD {
        return terminal_segment_head_response(&plan, path, headers.get(header::RANGE))
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    }
    let Some(lease_identity) = current_lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response_context =
        hls_cache_response_context(&app_state, &session, &access_context, lease_identity, now_ms).await;
    let Some(response) =
        terminal_segment_get_response(&plan, path, headers.get(header::RANGE), &response_context, &proxy_session_id)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if is_hls_media_activity_status(response.status()) {
        response_context.mark_media_activity().await;
        register_hls_cache_stream_for_successful_media_response(
            &app_state,
            &fingerprint,
            &headers,
            &access_context,
            &session,
            &response_context,
        )
        .await;
    }
    response
}

async fn demand_fetch_hls_segment_if_needed(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    preacquired_provider_handle: Option<ProviderHandle>,
    now_ms: u64,
) -> SegmentDemandFetchOutcome {
    let context = build_hls_segment_fetch_context(
        app_state,
        session,
        Some(access_context.lease_id.clone()),
        fingerprint,
        preacquired_provider_handle,
    )
    .await;
    app_state.hls_proxy.segment_worker_pool().demand_fetch_and_wait(context, segment_file, now_ms).await
}

async fn build_hls_segment_fetch_context(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    repair_access_lease_id: Option<HlsAccessLeaseId>,
    fingerprint: &Fingerprint,
    preacquired_provider_handle: Option<ProviderHandle>,
) -> SegmentFetchContext {
    let (headers, origin_provider_session_headers, origin_policy, reservation_ttl_secs) = {
        let session = session.read().await;
        (
            session.origin_request_headers.clone(),
            session.origin_provider_session_headers.clone(),
            session.effective_origin_acquire_policy_or_default(),
            session.account_overlap_timing().reservation_ttl_secs(),
        )
    };
    let mut origin_io = HlsOriginIoContext {
        ctx: app_state.hls_ctx(),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    };
    if let Some(provider_handle) = preacquired_provider_handle {
        origin_io = origin_io.with_preacquired_provider_handle(provider_handle);
    }
    SegmentFetchContext {
        session: Arc::clone(session),
        segment_cache: Arc::clone(app_state.hls_proxy.segment_cache()),
        segment_repair: Arc::clone(app_state.hls_proxy.segment_repair()),
        repair_access_lease_id,
        headers,
        origin_provider_session_headers,
        client: app_state.http_client.load().as_ref().clone(),
        no_redirect_client: app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        origin_io: Some(origin_io),
    }
}

async fn hls_effective_origin_acquire_policy(session: &HlsSessionHandle) -> HlsEffectiveOriginAcquirePolicy {
    session.read().await.effective_origin_acquire_policy_or_default()
}

async fn hls_origin_account_reservation_ttl_secs_for_session(session: &HlsSessionHandle) -> u64 {
    session.read().await.account_overlap_timing().reservation_ttl_secs()
}

fn hls_origin_account_reservation_ttl_secs_fallback() -> u64 {
    HlsAccountOverlapTiming::from_target_duration_secs(None).reservation_ttl_secs()
}

async fn hls_proxy_map(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyMapPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context, lease } = match prepare_hls_resource_access(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &params.hls_access_lease_id,
        now_ms,
        "map",
    )
    .await
    {
        Ok(access) => access,
        Err(response) => return *response,
    };
    if !hls_lease_allows_live_origin_work(&lease) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(map_file) = HlsMapFile::parse(&params.map_file) else {
        return hls_resource_channel_unavailable_response(&app_state, &access_context);
    };
    {
        let session_guard = session.read().await;
        if session_guard.is_gc_marked_for_removal() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(entry) = session_guard.maps.get(&map_file.proxy_map_id.into()) else {
            return hls_resource_channel_unavailable_response(&app_state, &access_context);
        };
        if entry.proxy_file_ext != map_file.extension {
            return hls_resource_channel_unavailable_response(&app_state, &access_context);
        }
    }

    let Some(current_lease) = current_hls_resource_lease(&app_state, &access_context).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if current_lease.playback_mode != HlsLeasePlaybackMode::Live {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(lease_identity) = current_lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response_context =
        hls_cache_response_context(&app_state, &session, &access_context, lease_identity, now_ms).await;
    let response = hls_resource_serve_outcome_response(
        &app_state,
        &access_context,
        serve_hls_map_cache_outcome(
            Arc::clone(app_state.hls_proxy.segment_cache()),
            Arc::clone(&session),
            map_file,
            headers.get(header::RANGE).cloned(),
            &response_context,
        )
        .await,
    );
    if is_hls_media_activity_status(response.status()) {
        register_hls_cache_stream_for_successful_media_response(
            &app_state,
            &fingerprint,
            &headers,
            &access_context,
            &session,
            &response_context,
        )
        .await;
    }
    response
}

async fn hls_proxy_resource(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyResourcePathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context, lease } = match prepare_hls_resource_access(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &params.hls_access_lease_id,
        now_ms,
        "resource",
    )
    .await
    {
        Ok(access) => access,
        Err(response) => return *response,
    };
    let Some(resource_file) = TransientResourceFile::parse(&params.resource_file) else {
        return hls_resource_channel_unavailable_response(&app_state, &access_context);
    };
    let Some(lease_identity) = lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let context = HlsResourceEndpointContext {
        app_state: &app_state,
        session: &session,
        fingerprint: &fingerprint,
        headers: &headers,
        access_context: &access_context,
        lease_identity,
        resource_file,
        range_header: headers.get(header::RANGE).cloned(),
        now_ms,
    };
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => serve_hls_live_transient_resource(context).await,
        HlsLeasePlaybackMode::TerminalTail(_) => serve_hls_terminal_key_resource(context, &lease.playback_mode).await,
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

struct HlsResourceEndpointContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    now_ms: u64,
}

async fn serve_hls_terminal_key_resource(
    context: HlsResourceEndpointContext<'_>,
    playback_mode: &HlsLeasePlaybackMode,
) -> axum::response::Response {
    let HlsLeasePlaybackMode::TerminalTail(plan) = playback_mode else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let proxy_session_id = &context.access_context.proxy_session_id;
    let Some(binding) =
        plan.terminal_key_binding(proxy_session_id, &context.access_context.lease_id, &context.resource_file)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !context.session.read().await.terminal_key_binding_is_current(
        &context.access_context.lease_id,
        plan.generation,
        &binding,
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response_context = hls_cache_response_context(
        context.app_state,
        context.session,
        context.access_context,
        context.lease_identity,
        context.now_ms,
    )
    .await;
    let response = finite_hls_terminal_key_response(
        binding.bytes(),
        context.range_header.as_ref(),
        binding.content_type(),
        "private, max-age=300, immutable",
        &response_context,
        proxy_session_id,
        context.resource_file.resource_id.0,
    );
    if is_hls_media_activity_status(response.status()) {
        response_context.mark_media_activity().await;
        register_hls_cache_stream_for_successful_media_response(
            context.app_state,
            context.fingerprint,
            context.headers,
            context.access_context,
            context.session,
            &response_context,
        )
        .await;
    }
    response
}

async fn serve_hls_live_transient_resource(context: HlsResourceEndpointContext<'_>) -> axum::response::Response {
    let cache_duration_ms = context.app_state.hls_proxy.cache_duration_seconds().saturating_mul(1_000);
    let Ok(cache_resolution) = resolve_hls_transient_object_cache_action(
        context.session,
        &context.access_context.proxy_session_id,
        &context.resource_file,
        context.range_header.as_ref(),
        context.now_ms,
        cache_duration_ms,
    )
    .await
    else {
        return hls_resource_channel_unavailable_response(context.app_state, context.access_context);
    };
    let resource = cache_resolution.resource;
    let origin_headers = cache_resolution.origin_headers;
    let origin_provider_session_headers = cache_resolution.origin_provider_session_headers;
    let cache_action = cache_resolution.action;

    match cache_action {
        HlsTransientObjectCacheAction::ServeReady => {
            return serve_transient_object_cache_response_and_mark_or_unavailable(TransientObjectCacheServeContext {
                app_state: context.app_state,
                session: context.session,
                fingerprint: context.fingerprint,
                headers: context.headers,
                access_context: context.access_context,
                lease_identity: context.lease_identity,
                resource_file: context.resource_file,
                range_header: context.range_header,
                now_ms: context.now_ms,
            })
            .await;
        }
        HlsTransientObjectCacheAction::WaitForFetch(notifier) => {
            return wait_for_transient_object_cache_fetch(TransientObjectWaitContext {
                app_state: context.app_state,
                session: context.session,
                fingerprint: context.fingerprint,
                headers: context.headers,
                access_context: context.access_context,
                lease_identity: context.lease_identity,
                resource_file: context.resource_file,
                range_header: context.range_header,
                notifier,
            })
            .await;
        }
        HlsTransientObjectCacheAction::FetchAndCache(_) | HlsTransientObjectCacheAction::PassthroughNoCache => {}
    }

    fetch_or_passthrough_transient_resource(HlsTransientPassthroughContext {
        endpoint: context,
        resource,
        cache_action,
        origin_headers,
        origin_provider_session_headers,
        cache_duration_ms,
    })
    .await
}

struct HlsTransientPassthroughContext<'a> {
    endpoint: HlsResourceEndpointContext<'a>,
    resource: TransientResourceRef,
    cache_action: HlsTransientObjectCacheAction,
    origin_headers: HeaderMap,
    origin_provider_session_headers: HeaderMap,
    cache_duration_ms: u64,
}

async fn fetch_or_passthrough_transient_resource(
    context: HlsTransientPassthroughContext<'_>,
) -> axum::response::Response {
    let HlsTransientPassthroughContext {
        endpoint,
        resource,
        cache_action,
        origin_headers,
        origin_provider_session_headers,
        cache_duration_ms,
    } = context;
    if let HlsTransientObjectCacheAction::FetchAndCache(fetch_token) = cache_action {
        return fetch_and_cache_transient_origin_response(HlsTransientEndpointCacheFetchContext {
            app_state: endpoint.app_state,
            session: endpoint.session,
            fingerprint: endpoint.fingerprint,
            headers: endpoint.headers,
            access_context: endpoint.access_context,
            lease_identity: endpoint.lease_identity,
            resource: &resource,
            resource_file: endpoint.resource_file,
            fetch_token: *fetch_token,
            origin_headers,
            origin_provider_session_headers,
            range_header: endpoint.range_header,
            cache_duration_ms,
        })
        .await;
    }

    let policy = endpoint.app_state.hls_proxy.segment_fetch_policy();
    let fetch_result = fetch_transient_origin_response_with_provider_io(HlsTransientEndpointOriginFetchRequest {
        app_state: endpoint.app_state,
        session: endpoint.session,
        access_context: endpoint.access_context,
        fingerprint: endpoint.fingerprint,
        headers: endpoint.headers,
        resource: &resource,
        resource_file: &endpoint.resource_file,
        origin_headers,
        origin_provider_session_headers,
        range_header: endpoint.range_header.clone(),
        policy: policy.clone(),
    })
    .await;
    serve_hls_transient_passthrough_result(endpoint, resource, policy, fetch_result).await
}

async fn serve_hls_transient_passthrough_result(
    endpoint: HlsResourceEndpointContext<'_>,
    resource: TransientResourceRef,
    policy: SegmentFetchPolicy,
    fetch_result: HlsTransientOriginFetchResult,
) -> axum::response::Response {
    match fetch_result.result {
        Ok(response) => {
            if response.decoded.status.is_success() {
                let activity_outcome = endpoint
                    .app_state
                    .hls_proxy
                    .mark_authorized_media_access_for_lease_if_identity_matches(
                        endpoint.session,
                        &endpoint.access_context.lease_id,
                        &endpoint.access_context.proxy_session_id,
                        endpoint.lease_identity,
                        endpoint.now_ms,
                    )
                    .await;
                match activity_outcome {
                    HlsMediaActivityCommitOutcome::Committed => {}
                    HlsMediaActivityCommitOutcome::StaleLeaseIdentity => {
                        debug!(
                            "HLS transient media response discarded: lease={} proxy_session={} reason=playback-generation-race",
                            safe_hls_access_lease_id(&endpoint.access_context.lease_id),
                            safe_proxy_session_id(&endpoint.access_context.proxy_session_id)
                        );
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    HlsMediaActivityCommitOutcome::DeferredLockContention => {
                        debug!(
                            "HLS transient media response deferred: lease={} proxy_session={} reason=lock-contention",
                            safe_hls_access_lease_id(&endpoint.access_context.lease_id),
                            safe_proxy_session_id(&endpoint.access_context.proxy_session_id)
                        );
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                }
                if ensure_hls_cache_stream_registered(
                    endpoint.app_state,
                    endpoint.fingerprint,
                    endpoint.headers,
                    endpoint.access_context,
                    endpoint.session,
                )
                .await
                .is_none()
                {
                    debug!(
                        "HLS transient media registration skipped: lease={} reason=session-or-connection-unavailable",
                        safe_hls_access_lease_id(&endpoint.access_context.lease_id)
                    );
                }
            }
            hls_transient_origin_response(
                response,
                HlsTransientDirectResponseContext {
                    session: Arc::clone(endpoint.session),
                    resource,
                    policy: policy.clone(),
                    now_ms: endpoint.now_ms,
                    log_identity: {
                        let session = endpoint.session.read().await;
                        HlsLogIdentity::from_session(&session)
                    },
                },
            )
        }
        Err(err) => {
            if matches!(err, HlsOriginResourceFetchError::ProviderUnavailable(_)) {
                if let Some(runtime_err) = fetch_result.runtime_prepare_error {
                    return hls_origin_runtime_resource_failure_response(
                        endpoint.app_state,
                        endpoint.access_context,
                        runtime_err,
                    );
                }
            }
            match hls_transient_object_fetch_failure(&err) {
                HlsTransientObjectFetchFailure::Retryable => {
                    let failed_at_ms = current_time_millis();
                    if record_temporary_transient_segment_fetch_failure(
                        endpoint.session,
                        &resource,
                        &policy,
                        failed_at_ms,
                    )
                    .await
                    {
                        hls_resource_channel_unavailable_response(endpoint.app_state, endpoint.access_context)
                    } else {
                        hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
                    }
                }
                HlsTransientObjectFetchFailure::Permanent { status: _ } => {
                    hls_resource_channel_unavailable_response(endpoint.app_state, endpoint.access_context)
                }
            }
        }
    }
}

struct HlsTransientEndpointOriginFetchRequest<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    access_context: &'a HlsAccessContext,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    resource: &'a TransientResourceRef,
    resource_file: &'a TransientResourceFile,
    origin_headers: HeaderMap,
    origin_provider_session_headers: HeaderMap,
    range_header: Option<HeaderValue>,
    policy: SegmentFetchPolicy,
}

struct HlsTransientOriginFetchResult {
    result: Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError>,
    runtime_prepare_error: Option<HlsOriginRuntimeAcquireError>,
}

async fn fetch_transient_origin_response_with_provider_io(
    request: HlsTransientEndpointOriginFetchRequest<'_>,
) -> HlsTransientOriginFetchResult {
    let clients = HlsOriginResourceClients {
        client: request.app_state.http_client.load().as_ref().clone(),
        no_redirect_client: request.app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: request.app_state.should_use_manual_redirects(),
    };
    let log_identity = {
        let session = request.session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    let fetch_request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: request.resource.resolved_origin_uri.clone(),
        origin_headers: request.origin_headers,
        origin_provider_session_headers: request.origin_provider_session_headers,
        range_header: request.range_header,
        resource_file: request.resource_file.clone(),
        resource_kind: request.resource.kind,
        clients,
        policy: request.policy,
        log_identity,
    };
    let runtime_prepare_error = Arc::new(tokio::sync::Mutex::new(None));
    let prepare_attempt = hls_transient_origin_prepare_closure(
        request.app_state,
        request.session,
        request.access_context,
        request.fingerprint,
        request.headers,
        &runtime_prepare_error,
    );
    let result = fetch_hls_transient_origin_response_with_attempt_prepare(fetch_request, prepare_attempt).await;
    let runtime_prepare_error = *runtime_prepare_error.lock().await;
    HlsTransientOriginFetchResult { result, runtime_prepare_error }
}

/// Builds the shared per-attempt prepare closure for transient origin fetches.
/// Runtime acquire failures are captured in `runtime_prepare_error` and mapped
/// to a provider-unavailable fetch error so the retry loop can proceed uniformly.
fn hls_transient_origin_prepare_closure(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    runtime_prepare_error: &Arc<tokio::sync::Mutex<Option<HlsOriginRuntimeAcquireError>>>,
) -> impl FnMut(
    HlsResourceFetchAttempt,
) -> futures::future::BoxFuture<
    'static,
    Result<Option<HlsTransientOriginIoGuard>, HlsOriginResourceFetchError>,
> {
    let app_state = Arc::clone(app_state);
    let session = Arc::clone(session);
    let access_context = access_context.clone();
    let fingerprint = fingerprint.clone();
    let headers = headers.clone();
    let runtime_prepare_error = Arc::clone(runtime_prepare_error);
    move |_attempt| {
        let app_state = Arc::clone(&app_state);
        let session = Arc::clone(&session);
        let access_context = access_context.clone();
        let fingerprint = fingerprint.clone();
        let headers = headers.clone();
        let runtime_prepare_error = Arc::clone(&runtime_prepare_error);
        async move {
            match prepare_hls_transient_origin_io_for_authorized_resource_work(
                &app_state,
                &session,
                &access_context,
                &fingerprint,
                &headers,
                current_time_millis(),
            )
            .await
            {
                Ok(guard) => Ok(guard),
                Err(err) => {
                    *runtime_prepare_error.lock().await = Some(err);
                    Err(HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Unavailable))
                }
            }
        }
        .boxed()
    }
}

#[allow(clippy::too_many_arguments)]
struct HlsTransientEndpointCacheFetchContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    resource: &'a TransientResourceRef,
    resource_file: TransientResourceFile,
    fetch_token: TransientObjectFetchToken,
    origin_headers: HeaderMap,
    origin_provider_session_headers: HeaderMap,
    range_header: Option<HeaderValue>,
    cache_duration_ms: u64,
}

struct TransientObjectWaitContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    notifier: Arc<tokio::sync::Notify>,
}

struct TransientObjectCacheServeContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    now_ms: u64,
}

async fn serve_transient_object_cache_response_and_mark(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    if !hls_live_lease_identity_is_current(context.app_state, context.access_context, context.lease_identity).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response_context = hls_cache_response_context(
        context.app_state,
        context.session,
        context.access_context,
        context.lease_identity,
        context.now_ms,
    )
    .await;
    let response = hls_resource_serve_outcome_response(
        context.app_state,
        context.access_context,
        serve_hls_transient_object_cache_outcome(
            Arc::clone(context.app_state.hls_proxy.segment_cache()),
            Arc::clone(context.session),
            context.resource_file,
            context.range_header,
            &response_context,
        )
        .await,
    );
    if is_hls_media_activity_status(response.status()) {
        register_hls_cache_stream_for_successful_media_response(
            context.app_state,
            context.fingerprint,
            context.headers,
            context.access_context,
            context.session,
            &response_context,
        )
        .await;
    }
    response
}

async fn serve_transient_object_cache_response_and_mark_or_unavailable(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    serve_transient_object_cache_response_and_mark(context).await
}

async fn wait_for_transient_object_cache_fetch(context: TransientObjectWaitContext<'_>) -> axum::response::Response {
    let wait_timeout = context.app_state.hls_proxy.segment_fetch_policy().origin_object_wait_timeout();
    let safe_resource_id = safe_transient_resource_id(&context.resource_file.resource_id);
    debug!(
        "HLS transient object wait started: resource_id={} lease={} state=inflight",
        safe_resource_id,
        safe_hls_access_lease_id(&context.access_context.lease_id)
    );
    let wait_result = tokio::time::timeout(wait_timeout, context.notifier.notified()).await;
    if wait_result.is_err() {
        debug!(
            "HLS transient object wait timed out: resource_id={} lease={} state=inflight",
            safe_resource_id,
            safe_hls_access_lease_id(&context.access_context.lease_id)
        );
        return hls_transient_object_unavailable_response(
            context.app_state,
            context.session,
            &context.resource_file,
            current_time_millis(),
            context.access_context,
        )
        .await;
    }
    let response = serve_transient_object_cache_response_and_mark_or_unavailable(TransientObjectCacheServeContext {
        app_state: context.app_state,
        session: context.session,
        fingerprint: context.fingerprint,
        headers: context.headers,
        access_context: context.access_context,
        lease_identity: context.lease_identity,
        resource_file: context.resource_file,
        range_header: context.range_header,
        now_ms: current_time_millis(),
    })
    .await;
    debug!(
        "HLS transient object wait completed: resource_id={} lease={} status={}",
        safe_resource_id,
        safe_hls_access_lease_id(&context.access_context.lease_id),
        response.status()
    );
    response
}

fn safe_transient_resource_id(resource_id: &TransientResourceId) -> String {
    // Truncate at the first char boundary at or before byte 8 to avoid allocating
    // a temporary `String` of 8 chars (and a second UTF-8 walk via `len()`).
    let full = resource_id.0.as_str();
    let truncate_at = full.char_indices().nth(8).map_or(full.len(), |(byte_idx, _)| byte_idx);
    if truncate_at == full.len() {
        return full.to_owned();
    }
    let mut out = String::with_capacity(truncate_at + 3);
    out.push_str(&full[..truncate_at]);
    out.push_str("...");
    out
}

async fn validate_hls_proxy_access_request(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    timing: HlsAccessLeaseTiming,
    request_kind: &'static str,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    let context = validate_hls_proxy_access_context(
        app_state,
        fingerprint,
        proxy_session_id,
        hls_access_lease_id,
        now_ms,
        HlsAccessAdmissionMode::ResourceAccess,
    )
    .await?;
    let startup_admission_pending = app_state
        .hls_proxy
        .access_lease_response_snapshot(&context.lease_id, proxy_session_id, now_ms)
        .await
        .is_some_and(|lease| {
            lease.state == HlsAccessLeaseState::Pending
                && lease.startup_admission == HlsLeaseStartupAdmissionState::Pending
        });
    if startup_admission_pending {
        return Err(HlsAccessLeaseValidationError::AvailabilityPending);
    }
    match app_state.hls_proxy.activate_access_lease(&context.lease_id, proxy_session_id, now_ms, timing).await {
        HlsAccessLeaseActivation::Activated { .. } => {
            debug!(
                "HLS access lease accepted: lease={} proxy_session={} user_session={} request={request_kind}",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Ok(context)
        }
        HlsAccessLeaseActivation::Denied => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} user_session={} request={request_kind} reason=denied",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            let (runtime_tail, reason) = app_state
                .hls_proxy
                .access_lease_response_snapshot(&context.lease_id, proxy_session_id, now_ms)
                .await
                .map_or((None, None), |lease| {
                    (lease.runtime_policy_revocation_outcome(), lease.runtime_policy_denial_reason())
                });
            Err(HlsAccessLeaseValidationError::AdmissionDenied { runtime_tail, reason })
        }
        HlsAccessLeaseActivation::Expired
        | HlsAccessLeaseActivation::UnknownLease
        | HlsAccessLeaseActivation::SessionMismatch => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} user_session={} request={request_kind} reason=expired",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Err(HlsAccessLeaseValidationError::Expired)
        }
    }
}

async fn ensure_hls_cache_stream_registered(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    access: &HlsAccessContext,
    session: &HlsSessionHandle,
) -> Option<StreamInfo> {
    let (proxy_session_id, origin_source, origin_account_binding) = {
        let session = session.read().await;
        if session.is_gc_marked_for_removal() {
            return None;
        }
        (session.proxy_session_id.clone(), session.origin_source.clone(), session.origin_account_binding.clone())
    };
    let user = app_state.app_config.get_user_credentials(&access.username)?;
    let user_session =
        app_state.active_users.get_and_update_user_session(&access.username, &access.user_session_token).await?;
    let connection_kind = user_session.connection_kind?;
    let priority = connection_priority_for_kind(&user, connection_kind);
    let mut stream_channel = build_hls_cache_stream_channel(app_state, access, &origin_source, &proxy_session_id).await;
    let provider = hls_cache_stats_provider(&origin_source, origin_account_binding.as_ref(), &user_session);
    let user_agent = req_headers
        .get(header::USER_AGENT)
        .map_or_else(|| Cow::Borrowed(""), |value| String::from_utf8_lossy(value.as_bytes()));

    stream_channel.url = Arc::from(hls_cache_stream_stats_url(&proxy_session_id));
    // Panel Streams/History read this item_type. Shared HLS transport is still HLS, but
    // archive/catchup leases must never be published as Live/LiveHls.
    let panel_archive_reference = origin_source
        .archive_reference
        .or(access.epg_reference_ts)
        .or_else(|| access.archive_origin_url.as_deref().and_then(m3u_archive_epg_reference_ts))
        .or_else(|| m3u_catchup_epg_reference_from_session_token(&access.user_session_token))
        .or(stream_channel.epg_reference_ts);
    let is_archive_playback = panel_archive_reference.is_some()
        || access.archive_origin_url.is_some()
        || origin_source.archive_reference.is_some()
        || is_m3u_catchup_session_token(&access.user_session_token)
        || stream_channel.item_type == PlaylistItemType::Catchup;
    if is_archive_playback {
        stream_channel.item_type = PlaylistItemType::Catchup;
        stream_channel.cluster = XtreamCluster::Video;
        stream_channel.epg_reference_ts = panel_archive_reference;
    } else {
        stream_channel.item_type = PlaylistItemType::LiveHls;
        stream_channel.cluster = XtreamCluster::try_from(PlaylistItemType::LiveHls).unwrap_or(stream_channel.cluster);
    }
    let shared_stream_id = hls_cache_shared_stream_id(&proxy_session_id);
    stream_channel.shared = true;
    stream_channel.shared_stream_id = Some(shared_stream_id);
    stream_channel.shared_joined_existing = Some(
        hls_cache_shared_joined_existing(app_state, shared_stream_id, &access.username, &access.user_session_token)
            .await,
    );
    let qos_config = HlsQosRuntimeConfig::from_app_config(&app_state.app_config);
    let qos_registration = app_state
        .hls_proxy
        .qos()
        .ensure_access_lease(
            &access.lease_id,
            &proxy_session_id,
            current_time_millis(),
            hls_qos_meter_init(app_state, qos_config),
        )
        .await;
    if let Some(meter) = qos_registration.register_meter.as_ref() {
        app_state.event_manager.register_meter(Arc::clone(meter)).await;
    }
    let history_mode = if qos_registration.emit_connect_record {
        ConnectionHistoryMode::EmitConnect
    } else {
        ConnectionHistoryMode::RefreshOnly
    };

    app_state
        .connection_manager
        .update_connection_with_history_mode(
            crate::api::model::ConnectionParams {
                meter_uid: qos_registration.meter_uid,
                username: &access.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind,
                priority,
                soft_priority: user.soft_priority,
                fingerprint,
                provider,
                stream_channel: &stream_channel,
                user_agent,
                session_token: Some(&access.user_session_token),
            },
            history_mode,
        )
        .await
}

fn hls_cache_stats_provider(
    origin_source: &HlsOriginSource,
    origin_account_binding: Option<&HlsOriginAccountBinding>,
    user_session: &UserSession,
) -> Arc<str> {
    origin_account_binding.filter(|binding| binding.is_active()).map_or_else(
        || {
            if user_session.provider.is_empty() {
                Arc::clone(&origin_source.input_name)
            } else {
                Arc::clone(&user_session.provider)
            }
        },
        |binding| Arc::clone(&binding.account_name),
    )
}

async fn build_hls_cache_stream_channel(
    app_state: &Arc<AppState>,
    access: &HlsAccessContext,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    let mut channel = if let Some((_, target)) = app_state.app_config.get_target_for_username(&access.username) {
        if let Some(mut channel) = get_stream_channel(app_state, &target, access.virtual_id).await {
            channel.url = Arc::from(hls_cache_stream_stats_url(proxy_session_id));
            channel
        } else {
            fallback_hls_cache_stream_channel(target.id, access.virtual_id, origin_source, proxy_session_id)
        }
    } else {
        fallback_hls_cache_stream_channel(0, access.virtual_id, origin_source, proxy_session_id)
    };

    let archive_reference = access
        .epg_reference_ts
        .or_else(|| access.archive_origin_url.as_deref().and_then(m3u_archive_epg_reference_ts))
        .or_else(|| m3u_catchup_epg_reference_from_session_token(&access.user_session_token));

    if archive_reference.is_some()
        || access.archive_origin_url.is_some()
        || is_m3u_catchup_session_token(&access.user_session_token)
    {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.cluster = XtreamCluster::try_from(PlaylistItemType::LiveHls).unwrap_or(channel.cluster);
        channel.epg_reference_ts = None;
    }
    channel
}

fn fallback_hls_cache_stream_channel(
    target_id: u16,
    virtual_id: u32,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    StreamChannel {
        target_id,
        virtual_id,
        provider_id: 0,
        input_name: Arc::clone(&origin_source.input_name),
        item_type: PlaylistItemType::LiveHls,
        cluster: XtreamCluster::Live,
        group: unknown.clone(),
        title: unknown,
        url: Arc::from(hls_cache_stream_stats_url(proxy_session_id)),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

fn hls_cache_stream_stats_url(proxy_session_id: &ProxySessionId) -> String {
    format!("/hls/shared/live/{}/manifest.m3u8", proxy_session_id.0)
}

fn hls_cache_shared_stream_id(proxy_session_id: &ProxySessionId) -> u64 {
    let digest = Sha256::digest(proxy_session_id.0.as_bytes());
    digest.iter().take(8).fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
}

async fn hls_cache_shared_joined_existing(
    app_state: &Arc<AppState>,
    shared_stream_id: u64,
    username: &str,
    session_token: &str,
) -> bool {
    let streams = app_state.active_users.active_streams().await;
    if let Some(existing) = streams.iter().find(|stream| {
        stream.username == username
            && stream.session_token.as_deref() == Some(session_token)
            && stream.channel.shared
            && stream.channel.shared_stream_id == Some(shared_stream_id)
    }) {
        return existing.channel.shared_joined_existing.unwrap_or(false);
    }

    streams.iter().any(|stream| {
        stream.channel.shared
            && stream.channel.shared_stream_id == Some(shared_stream_id)
            && (stream.username != username || stream.session_token.as_deref() != Some(session_token))
    })
}

async fn validate_hls_proxy_access_context(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    admission_mode: HlsAccessAdmissionMode,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    validate_hls_access_lease(
        &app_state.hls_ctx(),
        fingerprint,
        proxy_session_id,
        &HlsAccessLeaseId(hls_access_lease_id.to_string()),
        now_ms,
        admission_mode,
    )
    .await
}

async fn hls_custom_video_manifest_response_for_username(
    app_state: &Arc<AppState>,
    username: &str,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    if let Some(user) = app_state.app_config.get_user_credentials(username) {
        return hls_custom_video_manifest_response(app_state, &user, video_type, fallback_status).await;
    }
    fallback_status.into_response()
}

async fn hls_custom_video_manifest_response_for_lease(
    app_state: &Arc<AppState>,
    lease: &HlsAccessLease,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let Some(user) = app_state.app_config.get_user_credentials(&lease.username) else {
        return fallback_status.into_response();
    };
    hls_custom_video_manifest_response_for_access_lease(app_state, &user, video_type, fallback_status, lease).await
}

async fn hls_runtime_custom_tail_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let outcome = commit_hls_runtime_custom_tail(
        app_state.hls_ctx(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(session),
            proxy_session_id: proxy_session_id.clone(),
            lease_id: access_lease_id.clone(),
            reason,
            now_ms: current_time_millis(),
        },
    )
    .await;
    if matches!(outcome, HlsRuntimeCustomTailOutcome::Committed | HlsRuntimeCustomTailOutcome::AlreadyCommitted) {
        let now_ms = current_time_millis();
        if let Some(lease) =
            app_state.hls_proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
        {
            if let Some(response) = hls_terminal_playback_response(&lease, proxy_session_id, access_lease_id) {
                return response;
            }
        }
    }
    if outcome == HlsRuntimeCustomTailOutcome::PendingOwnerRegistered {
        return hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS);
    }
    fallback_status.into_response()
}

async fn hls_runtime_or_standalone_custom_tail_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let now_ms = current_time_millis();
    let Some(lease) =
        app_state.hls_proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
    else {
        return fallback_status.into_response();
    };
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. } => {
            if let Some(response) = hls_terminal_playback_response(&lease, proxy_session_id, access_lease_id) {
                return response;
            }
        }
        HlsLeasePlaybackMode::Live
            if lease.last_manifest_snapshot.is_some()
                && matches!(lease.state, HlsAccessLeaseState::Activated | HlsAccessLeaseState::PolicyRevoking) =>
        {
            return hls_runtime_custom_tail_response(
                app_state,
                session,
                proxy_session_id,
                access_lease_id,
                reason,
                fallback_status,
            )
            .await;
        }
        HlsLeasePlaybackMode::Ended if !reason.permits_unpublished_lease_standalone_tail() => {
            return fallback_status.into_response();
        }
        HlsLeasePlaybackMode::Live | HlsLeasePlaybackMode::Ended => {}
    }
    hls_custom_video_manifest_response_for_lease(app_state, &lease, reason.video_type(), fallback_status).await
}

async fn hls_manifest_channel_unavailable_response_for_username(
    app_state: &Arc<AppState>,
    username: &str,
) -> axum::response::Response {
    hls_custom_video_manifest_response_for_username(
        app_state,
        username,
        CustomVideoStreamType::ChannelUnavailable,
        StatusCode::NOT_FOUND,
    )
    .await
}

/// Resolves the final canonical-manifest fallback after refresh and cached-live
/// publication have both produced no response.
async fn hls_unpublished_lease_channel_unavailable_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> axum::response::Response {
    let reason = HlsRuntimeCustomTailReason::ChannelUnavailable;
    let lease = app_state
        .hls_proxy
        .access_lease_response_snapshot(access_lease_id, proxy_session_id, current_time_millis())
        .await;
    if let Some(lease) = lease.filter(|lease| lease.permits_unpublished_standalone_tail(reason)) {
        return hls_custom_video_manifest_response_for_lease(
            app_state,
            &lease,
            CustomVideoStreamType::ChannelUnavailable,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

async fn hls_manifest_access_denial_runtime_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_snapshot: Option<&HlsAccessLease>,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let Some(lease) = lease_snapshot else {
        return fallback_status.into_response();
    };
    let Some(session) = app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
        return fallback_status.into_response();
    };
    hls_runtime_or_standalone_custom_tail_response(
        app_state,
        &session,
        proxy_session_id,
        &lease.lease_id,
        reason,
        fallback_status,
    )
    .await
}

fn hls_resource_channel_unavailable_response(
    _app_state: &Arc<AppState>,
    _access_context: &HlsAccessContext,
) -> axum::response::Response {
    StatusCode::NOT_FOUND.into_response()
}

fn hls_origin_runtime_resource_failure_response(
    _app_state: &Arc<AppState>,
    _access_context: &HlsAccessContext,
    err: HlsOriginRuntimeAcquireError,
) -> axum::response::Response {
    match err {
        HlsOriginRuntimeAcquireError::NoAccountAvailable { .. } => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        HlsOriginRuntimeAcquireError::Fatal(status) => hls_canonical_status_response(status),
    }
}

fn hls_resource_serve_outcome_response(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    outcome: HlsResourceServeOutcome,
) -> axum::response::Response {
    match outcome {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms }) => {
            hls_temporary_resource_unavailable_response(retry_after_ms)
        }
        HlsResourceServeOutcome::Failure(
            HlsResourceServeFailure::Missing
            | HlsResourceServeFailure::Expired
            | HlsResourceServeFailure::PermanentFailed { .. },
        ) => hls_resource_channel_unavailable_response(app_state, access_context),
    }
}

async fn hls_manifest_access_lease_validation_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_snapshot: Option<&HlsAccessLease>,
    err: HlsAccessLeaseValidationError,
) -> axum::response::Response {
    match err {
        HlsAccessLeaseValidationError::AdmissionDenied { reason, .. } => {
            hls_manifest_access_denial_runtime_response(
                app_state,
                proxy_session_id,
                lease_snapshot,
                reason.unwrap_or(HlsRuntimeCustomTailReason::UserConnectionsExhausted),
                StatusCode::FORBIDDEN,
            )
            .await
        }
        HlsAccessLeaseValidationError::AvailabilityPending => {
            hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
        }
        HlsAccessLeaseValidationError::UserSessionMissing { .. } => {
            hls_manifest_access_denial_runtime_response(
                app_state,
                proxy_session_id,
                lease_snapshot,
                HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
                StatusCode::NOT_FOUND,
            )
            .await
        }
        HlsAccessLeaseValidationError::UserAccountExpired { .. } => {
            hls_manifest_access_denial_runtime_response(
                app_state,
                proxy_session_id,
                lease_snapshot,
                HlsRuntimeCustomTailReason::UserAccountExpired,
                StatusCode::FORBIDDEN,
            )
            .await
        }
        HlsAccessLeaseValidationError::Expired => StatusCode::NOT_FOUND.into_response(),
    }
}

fn hls_resource_access_lease_validation_response(err: &HlsAccessLeaseValidationError) -> axum::response::Response {
    match err {
        HlsAccessLeaseValidationError::AdmissionDenied { .. }
        | HlsAccessLeaseValidationError::UserAccountExpired { .. } => StatusCode::FORBIDDEN.into_response(),
        HlsAccessLeaseValidationError::AvailabilityPending => {
            hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
        }
        HlsAccessLeaseValidationError::UserSessionMissing { .. } | HlsAccessLeaseValidationError::Expired => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn hls_manifest_access_context_and_state(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_snapshot: Option<&HlsAccessLease>,
    now_ms: u64,
) -> Result<(HlsAccessContext, HlsAccessLeaseState), Box<axum::response::Response>> {
    app_state.hls_proxy.startup_observability().record_media_manifest_request(access_lease_id, now_ms);
    let access_context = match validate_hls_proxy_access_context(
        app_state,
        fingerprint,
        proxy_session_id,
        &access_lease_id.0,
        now_ms,
        HlsAccessAdmissionMode::ManifestPrepare,
    )
    .await
    {
        Ok(context) => context,
        Err(err) => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} user_session=none reason={err:?}",
                safe_hls_access_lease_id(access_lease_id),
                safe_proxy_session_id(proxy_session_id)
            );
            return Err(Box::new(
                hls_manifest_access_lease_validation_response(app_state, proxy_session_id, access_lease_snapshot, err)
                    .await,
            ));
        }
    };
    if access_lease_snapshot.is_none()
        && app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.is_none()
        && app_state.hls_proxy.expired_session_marker(proxy_session_id, now_ms).await.is_some()
    {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    debug!(
        "HLS access lease accepted: lease={} proxy_session={} user_session={} request=manifest",
        safe_hls_access_lease_id(&access_context.lease_id),
        safe_proxy_session_id(proxy_session_id),
        safe_user_session_token(&access_context.user_session_token)
    );

    let access_lease_state = match app_state
        .hls_proxy
        .touch_manifest_access_lease(
            &access_context.lease_id,
            proxy_session_id,
            now_ms,
            None,
            Some(HlsAccessLeasePendingDeadline::Bootstrap {
                deadline_ms: now_ms.saturating_add(hls_pending_bootstrap_window_ms(app_state)),
            }),
            hls_access_lease_ttl_ms(app_state),
        )
        .await
    {
        HlsAccessLeaseTouch::Touched { lease } => lease.state,
        HlsAccessLeaseTouch::Denied => {
            return Err(Box::new(
                hls_manifest_access_denial_runtime_response(
                    app_state,
                    proxy_session_id,
                    access_lease_snapshot,
                    HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                    StatusCode::FORBIDDEN,
                )
                .await,
            ));
        }
        HlsAccessLeaseTouch::Expired | HlsAccessLeaseTouch::UnknownLease | HlsAccessLeaseTouch::SessionMismatch => {
            return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
        }
    };

    Ok((access_context, access_lease_state))
}

async fn hls_transient_object_unavailable_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    resource_file: &TransientResourceFile,
    now_ms: u64,
    access_context: &HlsAccessContext,
) -> axum::response::Response {
    let state = {
        let session = session.read().await;
        let key = TransientPassthroughState::transient_object_key(
            &session.proxy_session_id,
            &resource_file.resource_id,
            resource_file.extension.clone(),
        );
        session.transient.object_unavailable_state(&key, now_ms)
    };
    match state {
        TransientObjectUnavailableState::Fetching => {
            hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
        }
        TransientObjectUnavailableState::FailedRetryable { retry_after_ms } => {
            hls_temporary_resource_unavailable_response(retry_after_ms)
        }
        TransientObjectUnavailableState::FailedPermanent | TransientObjectUnavailableState::Missing => {
            hls_resource_channel_unavailable_response(app_state, access_context)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn fetch_and_cache_transient_origin_response(
    context: HlsTransientEndpointCacheFetchContext<'_>,
) -> axum::response::Response {
    let policy = context.app_state.hls_proxy.segment_fetch_policy();
    let mut fetch_finalizer = HlsTransientObjectFetchFinalizer::new(
        Arc::clone(context.session),
        Arc::clone(context.app_state.hls_proxy.segment_cache()),
        context.fetch_token.clone(),
        HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
    );
    let clients = HlsOriginResourceClients {
        client: context.app_state.http_client.load().as_ref().clone(),
        no_redirect_client: context.app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: context.app_state.should_use_manual_redirects(),
    };
    let log_identity = {
        let session = context.session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    let fetch_request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: context.resource.resolved_origin_uri.clone(),
        origin_headers: context.origin_headers.clone(),
        origin_provider_session_headers: context.origin_provider_session_headers.clone(),
        range_header: None,
        resource_file: context.resource_file.clone(),
        resource_kind: context.resource.kind,
        clients,
        policy: policy.clone(),
        log_identity,
    };
    let cache_fetch_request = HlsTransientOriginCacheFetchRequest {
        fetch: fetch_request,
        commit: HlsTransientCacheCommitContext {
            segment_cache: Arc::clone(context.app_state.hls_proxy.segment_cache()),
            segment_repair: Arc::clone(context.app_state.hls_proxy.segment_repair()),
            session: Arc::clone(context.session),
            access_lease_id: context.access_context.lease_id.clone(),
            resource: context.resource.clone(),
            resource_file: context.resource_file.clone(),
            fetch_token: context.fetch_token.clone(),
            cache_duration_ms: context.cache_duration_ms,
        },
    };
    let runtime_prepare_error = Arc::new(tokio::sync::Mutex::new(None));
    let prepare_attempt = hls_transient_origin_prepare_closure(
        context.app_state,
        context.session,
        context.access_context,
        context.fingerprint,
        context.headers,
        &runtime_prepare_error,
    );
    let final_failure =
        match fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(cache_fetch_request, prepare_attempt)
            .await
        {
            Ok(()) => {
                let ready_at_ms = current_time_millis();
                let response_context = hls_cache_response_context(
                    context.app_state,
                    context.session,
                    context.access_context,
                    context.lease_identity,
                    ready_at_ms,
                )
                .await;
                let response = serve_hls_transient_object_cache_response(
                    Arc::clone(context.app_state.hls_proxy.segment_cache()),
                    Arc::clone(context.session),
                    context.resource_file.clone(),
                    context.range_header.clone(),
                    &response_context,
                )
                .await;
                if is_hls_media_activity_status(response.status()) {
                    register_hls_cache_stream_for_successful_media_response(
                        context.app_state,
                        context.fingerprint,
                        context.headers,
                        context.access_context,
                        context.session,
                        &response_context,
                    )
                    .await;
                }
                record_successful_transient_segment_fetch(context.session, context.resource).await;
                fetch_finalizer.complete();
                return response;
            }
            Err(err) => {
                if matches!(err, HlsOriginResourceFetchError::ProviderUnavailable(_)) {
                    let runtime_prepare_error = *runtime_prepare_error.lock().await;
                    if let Some(runtime_err) = runtime_prepare_error {
                        context.session.write().await.fail_transient_object_retryable_if_current(
                            &context.fetch_token,
                            current_time_millis(),
                            HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
                        );
                        return hls_origin_runtime_resource_failure_response(
                            context.app_state,
                            context.access_context,
                            runtime_err,
                        );
                    }
                }
                hls_transient_object_fetch_failure(&err)
            }
        };

    let failed_at_ms = current_time_millis();
    match final_failure {
        HlsTransientObjectFetchFailure::Retryable => {
            if record_temporary_transient_segment_fetch_failure(
                context.session,
                context.resource,
                &policy,
                failed_at_ms,
            )
            .await
            {
                context.session.write().await.fail_transient_object_permanent_if_current(
                    &context.fetch_token,
                    failed_at_ms,
                    None,
                );
            } else {
                context.session.write().await.fail_transient_object_retryable_if_current(
                    &context.fetch_token,
                    failed_at_ms,
                    HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
                );
            }
        }
        HlsTransientObjectFetchFailure::Permanent { status } => {
            context.session.write().await.fail_transient_object_permanent_if_current(
                &context.fetch_token,
                failed_at_ms,
                status,
            );
        }
    }
    hls_transient_object_unavailable_response(
        context.app_state,
        context.session,
        &context.resource_file,
        failed_at_ms,
        context.access_context,
    )
    .await
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

async fn release_prepared_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    addr: &std::net::SocketAddr,
) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.release_unbound_session_reservation(username, session_token, None, false).await;
    app_state.active_users.clear_unbound_session_addr(username, session_token, addr).await;
}

async fn terminate_failed_hls_manifest_session(app_state: &Arc<AppState>, username: &str, session_token: &str) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.terminate_session(username, session_token).await;
    app_state.active_provider.clear_provider_reservation(session_token).await;
}

fn normalize_xtream_live_hls_url(hls_url: &str, input: &ConfigInput) -> String {
    if !input.input_type.is_xtream() || !input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix) {
        return hls_url.to_string();
    }

    let (Some(username), Some(password)) = (input.username.as_deref(), input.password.as_deref()) else {
        return hls_url.to_string();
    };

    let Ok(mut parsed) = Url::parse(hls_url) else {
        return hls_url.to_string();
    };
    let Some(segments) = parsed.path_segments() else {
        return hls_url.to_string();
    };

    let parts: Vec<&str> = segments.collect();
    if parts.len() >= 3 && parts[0] == username && parts[1] == password {
        parsed.set_path(&format!("/live/{}", parts.join("/")));
        return parsed.to_string();
    }

    hls_url.to_string()
}

fn ensure_hls_manifest_extension(url: &str) -> String {
    let with_extension = replace_url_extension(url, HLS_EXT);
    let (base_url, suffix) = match with_extension.find(['?', '#'].as_ref()) {
        Some(pos) => (&with_extension[..pos], &with_extension[pos..]),
        None => (with_extension.as_str(), ""),
    };
    let Some(path_without_ext) = base_url.strip_suffix(HLS_EXT) else {
        return with_extension;
    };
    format!("{}{}{}", path_without_ext.trim_end_matches('.'), HLS_EXT, suffix)
}

fn build_hls_manifest_request_headers(
    input_headers: &HashMap<String, String>,
    req_headers: &HeaderMap,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
    upstream_user_agent: Option<&str>,
) -> HeaderMap {
    let input_headers = input_headers
        .iter()
        .filter(|(key, _)| !should_remove_hls_origin_header(key, disabled_headers))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    let disabled_headers_for_filter = disabled_headers.cloned();
    let filter_header: HeaderFilter = Some(Box::new(move |name: &str| {
        !name.eq_ignore_ascii_case("range")
            && !should_remove_hls_origin_header(name, disabled_headers_for_filter.as_ref())
    }));
    let forwarded = get_headers_from_request(req_headers, &filter_header);
    let mut headers =
        request::get_request_headers(Some(&input_headers), Some(&forwarded), disabled_headers, default_user_agent);
    request::overlay_upstream_user_agent(&mut headers, upstream_user_agent, disabled_headers);
    scrub_hls_origin_headers(&mut headers, disabled_headers);
    force_identity_without_range(&mut headers);
    headers
}

async fn download_legacy_hls_manifest(
    app_state: &Arc<AppState>,
    input: &InputSource,
    headers: &HeaderMap,
) -> Result<(String, String, HeaderMap), std::io::Error> {
    let deadline = Duration::from_millis(app_state.hls_proxy.origin_manifest_timeout_ms().max(1));
    let fetch_options = request::RequestFetchOptions::with_attempt_idle_timeout(deadline)
        .with_content_coding(OutboundContentCodingPolicy::Identity);
    let body_options = request::TextContentBodyOptions::hls_manifest(MAX_HLS_MANIFEST_BYTES, deadline);
    let options = request::TextContentFetchOptions::new(fetch_options, body_options);

    if app_state.should_use_manual_redirects() {
        request::download_text_content_with_manual_redirects_and_headers_and_options(
            &app_state.app_config,
            &app_state.http_client_no_redirect.load(),
            input,
            Some(headers),
            false,
            MAX_MANUAL_REDIRECTS,
            options,
        )
        .await
    } else {
        request::download_text_content_with_headers_and_options(
            &app_state.app_config,
            &app_state.http_client.load(),
            input,
            Some(headers),
            false,
            options,
        )
        .await
    }
}

struct HlsCacheManifestOrigin<'a> {
    raw_request_url: &'a str,
    session_entry_url: HlsOriginEntryUrl,
    input: &'a ConfigInput,
    origin_source: HlsOriginSource,
}

struct HlsCacheOriginResolution {
    hls_url: String,
    session_entry_url: HlsOriginEntryUrl,
}

#[derive(Clone, Debug)]
enum HlsOriginEntryUrl {
    DirectHttp { url: String },
    ProviderFailover { url: String, provider: Arc<ConfigProvider> },
}

impl HlsOriginEntryUrl {
    fn direct_http(url: impl Into<String>) -> Self { Self::DirectHttp { url: url.into() } }

    fn provider_failover(url: impl Into<String>, provider: Arc<ConfigProvider>) -> Self {
        Self::ProviderFailover { url: url.into(), provider }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::DirectHttp { url } | Self::ProviderFailover { url, .. } => url,
        }
    }

    fn url_failover_provider(&self) -> Option<Arc<ConfigProvider>> {
        match self {
            Self::DirectHttp { .. } => None,
            Self::ProviderFailover { provider, .. } => Some(Arc::clone(provider)),
        }
    }
}

fn resolve_hls_cache_origin_entry_url(input: &ConfigInput, url: &str) -> Option<HlsCacheOriginResolution> {
    if let Some(provider) = hls_url_failover_provider_for_origin_url(input, url) {
        return Some(HlsCacheOriginResolution {
            hls_url: url.to_string(),
            session_entry_url: HlsOriginEntryUrl::provider_failover(url, provider),
        });
    }

    let parsed = Url::parse(url).ok()?;
    if matches!(parsed.scheme(), "http" | "https") {
        return Some(HlsCacheOriginResolution {
            hls_url: url.to_string(),
            session_entry_url: HlsOriginEntryUrl::direct_http(url),
        });
    }

    warn!("HLS origin entry URL is not supported: url={}", sanitize_sensitive_info(url));
    None
}

fn hls_url_failover_provider_for_origin_url(input: &ConfigInput, url: &str) -> Option<Arc<ConfigProvider>> {
    if !url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return None;
    }
    input.get_resolve_provider(url).map(|provider| Arc::clone(&provider))
}

fn is_http_hls_origin_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

fn is_supported_hls_origin_url(input: &ConfigInput, url: &str) -> bool {
    input.get_resolve_provider(url).is_some() || is_http_hls_origin_url(url)
}

fn build_hls_origin_source(input: &ConfigInput, stream_ref: impl Into<String>) -> HlsOriginSource {
    HlsOriginSource::new(input.id, Arc::clone(&input.name), stream_ref, hls_origin_source_kind(input.input_type))
}

fn build_hls_origin_source_for_playback(
    input: &ConfigInput,
    stream_ref: impl Into<String>,
    archive_reference: Option<i64>,
    archive_url: Option<&str>,
) -> HlsOriginSource {
    let source = build_hls_origin_source(input, stream_ref);
    match (archive_reference, archive_url) {
        (Some(timestamp), Some(url)) => source.with_archive_request(timestamp, url),
        (Some(timestamp), None) => source.with_archive_reference(timestamp),
        (None, _) => source,
    }
}

/// Keeps target routing identity separate from the immutable input content identity.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(in crate::api) struct HlsEntryStreamIdentity {
    virtual_id: u32,
    input_stream_id: Arc<str>,
    upstream_user_agent: Option<Arc<str>>,
}

impl HlsEntryStreamIdentity {
    pub(in crate::api) fn new(virtual_id: u32, input_stream_id: impl Into<Arc<str>>) -> Option<Self> {
        let input_stream_id = input_stream_id.into();
        if input_stream_id.trim().is_empty() {
            return None;
        }
        Some(Self { virtual_id, input_stream_id, upstream_user_agent: None })
    }

    pub(in crate::api) fn from_playlist_item(item: &impl PlaylistEntry) -> Option<Self> {
        let mut identity = Self::new(item.get_virtual_id(), item.get_input_stream_id()?)?;
        identity.upstream_user_agent = item.get_upstream_user_agent().map(Internable::intern);
        Some(identity)
    }

    pub(in crate::api) const fn virtual_id(&self) -> u32 { self.virtual_id }

    fn stream_ref(&self) -> &str { self.input_stream_id.as_ref() }

    fn upstream_user_agent(&self) -> Option<&str> { self.upstream_user_agent.as_deref() }
}

/// Immutable input identity plus bitrate metadata available at the virtual HLS entry.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(in crate::api) struct HlsEntryStreamContext {
    identity: HlsEntryStreamIdentity,
    known_bitrate_bps: Option<u32>,
}

impl HlsEntryStreamContext {
    pub(in crate::api) fn from_playlist_item(item: &impl PlaylistEntry) -> Option<Self> {
        let identity = HlsEntryStreamIdentity::from_playlist_item(item)?;
        let known_bitrate_bps = match item.get_additional_properties() {
            Some(StreamProperties::Live(properties)) if properties.bitrate > 0 => Some(properties.bitrate),
            Some(
                StreamProperties::Live(_)
                | StreamProperties::Video(_)
                | StreamProperties::Series(_)
                | StreamProperties::Episode(_),
            )
            | None => None,
        };
        Some(Self { identity, known_bitrate_bps })
    }

    pub(in crate::api) const fn virtual_id(&self) -> u32 { self.identity.virtual_id() }

    pub(in crate::api) fn stream_ref(&self) -> &str { self.identity.stream_ref() }

    pub(in crate::api) const fn known_bitrate_bps(&self) -> Option<u32> { self.known_bitrate_bps }

    pub(in crate::api) fn identity(&self) -> &HlsEntryStreamIdentity { &self.identity }
}

/// Resolves the configured input together with both identities of one target entry.
#[derive(Debug, Clone)]
pub(in crate::api) struct HlsResolvedVirtualSource {
    pub(in crate::api) input: Arc<ConfigInput>,
    pub(in crate::api) stream_context: HlsEntryStreamContext,
}

fn hls_origin_source_kind(input_type: InputType) -> HlsOriginSourceKind {
    if input_type.is_xtream() {
        HlsOriginSourceKind::XtreamLive
    } else if input_type.is_m3u() {
        HlsOriginSourceKind::M3uMediaPlaylist
    } else {
        HlsOriginSourceKind::DirectMediaPlaylist
    }
}

fn build_hls_origin_resolution(input: &ConfigInput, media_playlist_url: &str) -> Option<HlsCacheOriginResolution> {
    let candidate = match hls_origin_source_kind(input.input_type) {
        HlsOriginSourceKind::XtreamLive => {
            ensure_hls_manifest_extension(&normalize_xtream_live_hls_url(media_playlist_url, input))
        }
        HlsOriginSourceKind::M3uMediaPlaylist | HlsOriginSourceKind::DirectMediaPlaylist => {
            ensure_hls_manifest_extension(media_playlist_url)
        }
    };
    resolve_hls_cache_origin_entry_url(input, &candidate)
}

#[derive(Clone, Copy)]
enum HlsOriginWorkKind {
    Manifest,
    Segment,
    Resource,
}

impl HlsOriginWorkKind {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Segment => "segment",
            Self::Resource => "resource",
        }
    }
}

fn build_hls_origin_fetch_url(
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    provider_config: Option<&Arc<RuntimeProviderConfig>>,
) -> Option<String> {
    let provider_scheme_url = [session_entry_url, raw_request_url]
        .into_iter()
        .find(|url| hls_url_failover_provider_for_origin_url(input, url).is_some());
    let url = if let (Some(provider_config), Some(provider_scheme_url)) = (provider_config, provider_scheme_url) {
        rewrite_hls_provider_scheme_origin_account(provider_scheme_url, input, provider_config)?
    } else if let Some(provider_config) = provider_config {
        get_stream_alternative_url(raw_request_url, input, provider_config)
            .or_else(|| get_stream_alternative_url(session_entry_url, input, provider_config))
            .unwrap_or_else(|| session_entry_url.to_string())
    } else {
        session_entry_url.to_string()
    };

    if is_supported_hls_origin_url(input, &url) {
        Some(url)
    } else {
        None
    }
}

fn rewrite_hls_provider_scheme_origin_account(
    provider_scheme_url: &str,
    input: &ConfigInput,
    provider_config: &Arc<RuntimeProviderConfig>,
) -> Option<String> {
    if !provider_scheme_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return None;
    }
    let alt_input_user_info = provider_config.get_user_info()?;
    let Some((_source_base_url, source_username, source_password)) =
        input.get_matched_config_by_url(provider_scheme_url)
    else {
        return Some(provider_scheme_url.to_string());
    };
    let (Some(old_username), Some(old_password)) = (source_username, source_password) else {
        return Some(provider_scheme_url.to_string());
    };

    let mut url = Url::parse(provider_scheme_url).ok()?;
    if rewrite_hls_url_auth_fields(
        &mut url,
        old_username,
        old_password,
        &alt_input_user_info.username,
        &alt_input_user_info.password,
    ) {
        Some(url.to_string())
    } else {
        None
    }
}

fn rewrite_hls_url_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    if rewrite_hls_query_auth_fields(url, new_username, new_password) {
        return true;
    }

    if url.username() == old_username && url.password() == Some(old_password) {
        return url.set_username(new_username).is_ok() && url.set_password(Some(new_password)).is_ok();
    }

    rewrite_hls_path_auth_fields(url, old_username, old_password, new_username, new_password)
}

fn rewrite_hls_query_auth_fields(url: &mut Url, new_username: &str, new_password: &str) -> bool {
    let mut has_username = false;
    let mut has_password = false;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case("username") {
                has_username = true;
                (key.into_owned(), new_username.to_string())
            } else if key.eq_ignore_ascii_case("password") {
                has_password = true;
                (key.into_owned(), new_password.to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect();

    if !(has_username && has_password) {
        return false;
    }

    url.query_pairs_mut().clear().extend_pairs(pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    true
}

fn rewrite_hls_path_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    let Some(mut segments) = url.path_segments().map(|segments| segments.map(ToOwned::to_owned).collect::<Vec<_>>())
    else {
        return false;
    };

    let credential_index = if segments.len() >= 3
        && matches!(segments.first().map(String::as_str), Some("live" | "movie" | "series"))
        && segments.get(1).is_some_and(|segment| segment == old_username)
        && segments.get(2).is_some_and(|segment| segment == old_password)
    {
        Some(1)
    } else if segments.len() >= 2
        && segments.first().is_some_and(|segment| segment == old_username)
        && segments.get(1).is_some_and(|segment| segment == old_password)
    {
        Some(0)
    } else {
        None
    };

    let Some(credential_index) = credential_index else {
        return false;
    };

    segments[credential_index] = new_username.to_string();
    segments[credential_index + 1] = new_password.to_string();

    let Ok(mut path_segments) = url.path_segments_mut() else {
        return false;
    };
    path_segments.clear().extend(segments.iter().map(String::as_str));
    true
}

fn hls_url_failover_provider_for_origin_context(
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    fetch_url: &str,
) -> Option<Arc<ConfigProvider>> {
    hls_url_failover_provider_for_origin_url(input, session_entry_url)
        .or_else(|| hls_url_failover_provider_for_origin_url(input, raw_request_url))
        .or_else(|| hls_url_failover_provider_for_origin_url(input, fetch_url))
}

struct PreparedHlsOriginRuntime {
    fetch_url: String,
    // URL failover comes from source.yml provider:// resolution. Origin-account
    // binding/handles are runtime account reservations and must stay separate.
    url_failover_provider: Option<Arc<ConfigProvider>>,
    origin_account_binding_to_store: Option<HlsOriginAccountBinding>,
    preacquired_origin_account_handle: Option<ProviderHandle>,
}

fn effective_hls_url_failover_provider_for_fetch_url(
    fetch_url: &str,
    prepared_url_failover_provider: Option<Arc<ConfigProvider>>,
    origin_url_failover_provider: Option<Arc<ConfigProvider>>,
) -> Option<Arc<ConfigProvider>> {
    if !fetch_url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return None;
    }
    prepared_url_failover_provider.or(origin_url_failover_provider)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsOriginRuntimeAcquireError {
    NoAccountAvailable { reason: HlsOriginRuntimeNoAccountReason },
    Fatal(StatusCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsOriginRuntimeNoAccountReason {
    ProviderConnectionsExhausted,
    OriginBindingPreempted,
}

fn hls_no_account_reason_for_binding(binding: Option<&HlsOriginAccountBinding>) -> HlsOriginRuntimeNoAccountReason {
    let Some(binding) = binding else {
        return HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted;
    };
    match &binding.binding_mode {
        HlsOriginAccountBindingMode::Detached {
            reason: HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner,
            ..
        }
        | HlsOriginAccountBindingMode::Detached {
            reason: HlsOriginAccountDetachedReason::PreemptedByHigherPriority,
            ..
        } => HlsOriginRuntimeNoAccountReason::OriginBindingPreempted,
        HlsOriginAccountBindingMode::Detached { .. }
        | HlsOriginAccountBindingMode::Active
        | HlsOriginAccountBindingMode::Speculative { .. } => {
            HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted
        }
    }
}

#[derive(Clone)]
struct HlsAccountOverlapCandidate {
    proxy_session_id: ProxySessionId,
    input_name: Arc<str>,
    account_name: Arc<str>,
    session_owner: String,
    reclaim_until_ms: u64,
    last_media_at_ms: u64,
    soft_overlap_eligible_at_ms: u64,
    soft_overlap_delay_ms: u64,
    tuliprox_target_user_connection_capacity: u32,
    origin_input_account_connection_capacity: u32,
}

#[derive(Clone)]
struct HlsOriginPolicyPreemptCandidate {
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    account_name: Arc<str>,
    session_owner: String,
    reservation_ttl_secs: u64,
    victim_policy: HlsEffectiveOriginAcquirePolicy,
    last_media_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct HlsSoftOverlapCapacity {
    tuliprox_target_user_connection_capacity: u32,
    origin_input_account_connection_capacity: u32,
    delay_ms: u64,
}

fn hls_soft_overlap_capacity_for_target_duration(
    tuliprox_target_user_connection_capacity: u32,
    origin_input_account_connection_capacity: u32,
    target_duration_ms: u64,
) -> HlsSoftOverlapCapacity {
    let delay_ms = hls_soft_overlap_delay_ms(
        target_duration_ms,
        tuliprox_target_user_connection_capacity,
        origin_input_account_connection_capacity,
    );
    HlsSoftOverlapCapacity {
        tuliprox_target_user_connection_capacity,
        origin_input_account_connection_capacity,
        delay_ms,
    }
}

async fn hls_origin_input_account_connection_capacity(app_state: &Arc<AppState>, input: &ConfigInput) -> u32 {
    let capacities = app_state.active_provider.provider_capacities_for_input(&input.name).await;
    if capacities.is_empty() {
        return hls_configured_origin_input_account_connection_capacity(input);
    }
    capacities
        .into_iter()
        .map(|(_, _, max)| if max == 0 { u32::MAX } else { u32::try_from(max).unwrap_or(u32::MAX) })
        .fold(0u32, u32::saturating_add)
        .max(1)
}

fn hls_configured_origin_input_account_connection_capacity(input: &ConfigInput) -> u32 {
    let input_capacity = if input.max_connections == 0 { 1 } else { u32::from(input.max_connections) };
    input
        .aliases
        .as_ref()
        .map_or(0, |aliases| {
            aliases
                .iter()
                .filter(|alias| alias.enabled)
                .map(|alias| if alias.max_connections == 0 { 1 } else { u32::from(alias.max_connections) })
                .fold(0u32, u32::saturating_add)
        })
        .saturating_add(input_capacity)
        .max(1)
}

async fn hls_tuliprox_target_user_connection_capacity(app_state: &Arc<AppState>, input: &ConfigInput) -> u32 {
    hls_configured_tuliprox_target_user_connection_capacity(app_state, input)
        .max(hls_active_tuliprox_target_user_connections_for_input(app_state, input).await)
        .max(1)
}

fn hls_configured_tuliprox_target_user_connection_capacity(app_state: &Arc<AppState>, input: &ConfigInput) -> u32 {
    let Some(api_proxy) = app_state.app_config.api_proxy.load().as_ref().cloned() else {
        return 1;
    };
    api_proxy
        .user
        .iter()
        .filter(|target_user| {
            app_state
                .app_config
                .get_inputs_for_target(&target_user.target)
                .is_some_and(|inputs| inputs.iter().any(|candidate| candidate.name == input.name))
        })
        .map(|target_user| {
            target_user
                .credentials
                .iter()
                .map(|user| {
                    if user.max_connections == 0 {
                        u32::MAX
                    } else {
                        user.max_connections.saturating_add(u32::from(user.soft_connections))
                    }
                })
                .fold(0u32, u32::saturating_add)
        })
        .max()
        .unwrap_or(1)
        .max(1)
}

async fn hls_active_tuliprox_target_user_connections_for_input(app_state: &Arc<AppState>, input: &ConfigInput) -> u32 {
    u32::try_from(
        app_state
            .active_users
            .active_streams()
            .await
            .iter()
            .filter(|stream| stream.channel.input_name == input.name)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

fn hls_soft_overlap_delay_ms(
    target_duration_ms: u64,
    tuliprox_target_user_connection_capacity: u32,
    origin_input_account_connection_capacity: u32,
) -> u64 {
    let target_duration_ms = target_duration_ms.max(1);
    let users = u64::from(tuliprox_target_user_connection_capacity.max(1));
    let origin = u64::from(origin_input_account_connection_capacity.max(1));
    if users >= origin.saturating_mul(2) {
        return target_duration_ms;
    }
    if users <= origin {
        return target_duration_ms.saturating_mul(2);
    }
    let numerator = origin.saturating_mul(3).saturating_sub(users);
    target_duration_ms.saturating_mul(numerator).saturating_add(origin.saturating_sub(1)) / origin
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn prepare_hls_origin_runtime(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    promote_elapsed_hls_account_overlaps(app_state, now_ms).await;
    detach_unprotected_hls_origin_account_bindings(app_state, now_ms).await;
    reclaim_hls_account_overlap_if_needed(app_state, session, now_ms).await;
    let existing_binding = session.read().await.origin_account_binding.clone();
    let reacquire_detached_binding = existing_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached);
    let final_no_account_reason = hls_no_account_reason_for_binding(existing_binding.as_ref());
    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_started(session, work_kind).await;
    }
    if let Some(binding) = existing_binding {
        if binding.is_active() {
            match hls_origin_account_status(&app_state.hls_ctx(), &binding) {
                stale_status @ (HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired) => {
                    return rebind_hls_origin_account(
                        app_state,
                        session,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                        stale_status,
                        fingerprint,
                        connection_kind,
                        priority,
                        now_ms,
                    )
                    .await;
                }
                HlsOriginAccountStatus::Known => {
                    return Ok(prepared_hls_origin_runtime_for_known_binding(
                        app_state,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                    ));
                }
            }
        }
    }

    match prepare_hls_origin_runtime_with_new_account(
        app_state,
        input,
        raw_request_url,
        session_entry_url,
        proxy_session_id,
        fingerprint,
        connection_kind,
        priority,
        false,
        work_kind,
        work_class,
        now_ms,
    )
    .await
    {
        Ok(prepared) => {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
        Err(HlsOriginRuntimeAcquireError::Fatal(status)) => return Err(HlsOriginRuntimeAcquireError::Fatal(status)),
        Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { .. }) => {}
    }

    if work_class.allows_speculative_overlap() {
        if let Ok(prepared) = prepare_hls_origin_policy_preempt_runtime(
            app_state,
            session,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }

        if let Ok(prepared) = prepare_hls_speculative_origin_runtime(
            app_state,
            session,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
    } else {
        debug!("HLS account overlap skipped: work_class={} reason=background-origin-work", work_class.as_log_value());
    }

    if work_class.allows_grace() {
        match prepare_hls_origin_runtime_with_new_account(
            app_state,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            true,
            work_kind,
            work_class,
            now_ms,
        )
        .await
        {
            Ok(prepared) => {
                if reacquire_detached_binding {
                    if let Some(binding) = prepared.origin_account_binding_to_store.as_ref() {
                        log_hls_origin_binding_reacquired(session, binding).await;
                    }
                }
                return Ok(prepared);
            }
            Err(HlsOriginRuntimeAcquireError::Fatal(status)) => {
                return Err(HlsOriginRuntimeAcquireError::Fatal(status))
            }
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { .. }) => {}
        }
    } else {
        debug!(
            "HLS origin account grace skipped: work_class={} reason=background-origin-work",
            work_class.as_log_value()
        );
    }

    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_failed(session, "no-account-available").await;
    }
    Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { reason: final_no_account_reason })
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hls_origin_runtime_with_new_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    allow_grace: bool,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            allow_grace,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        debug!(
            "HLS origin account acquire unavailable: work={} work_class={} grace={}",
            work_kind.as_log_value(),
            work_class.as_log_value(),
            if allow_grace { "attempted" } else { "disabled" }
        );
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted,
        });
    };

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(binding) = origin_account_binding_from_allocation(
        Arc::clone(&input.name),
        proxy_session_id,
        &provider_handle.allocation,
        now_ms,
    ) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let grace_state = if matches!(provider_handle.allocation, ProviderAllocation::GracePeriod(_)) {
        "granted"
    } else if allow_grace {
        "not-needed"
    } else {
        "disabled"
    };
    debug!(
        "HLS origin account binding created: account={} owner={} work={} work_class={} grace={}",
        sanitize_sensitive_info(binding.account_name.as_ref()),
        sanitize_sensitive_info(&binding.session_owner),
        work_kind.as_log_value(),
        work_class.as_log_value(),
        grace_state
    );

    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: Some(binding),
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

fn prepared_hls_origin_runtime_for_known_binding(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    binding: &HlsOriginAccountBinding,
) -> PreparedHlsOriginRuntime {
    let fetch_url = app_state
        .active_provider
        .find_provider_config(&binding.account_name)
        .as_ref()
        .and_then(|provider_config| {
            build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(provider_config))
        })
        .unwrap_or_else(|| session_entry_url.to_string());

    PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: None,
        preacquired_origin_account_handle: None,
    }
}

async fn log_hls_origin_binding_reacquire_started(session: &HlsSessionHandle, work_kind: HlsOriginWorkKind) {
    let session_guard = session.read().await;
    let mode = match session_guard.mode {
        HlsSessionMode::NormalCacheTimeline => "normal",
        HlsSessionMode::TransientPassthrough { .. } => "transient",
    };
    debug!(
        "HLS origin binding reacquire started: proxy_session={} mode={} work={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        mode,
        work_kind.as_log_value()
    );
}

async fn log_hls_origin_binding_reacquired(session: &HlsSessionHandle, binding: &HlsOriginAccountBinding) {
    let session_guard = session.read().await;
    debug!(
        "HLS origin binding reacquired: proxy_session={} account={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(binding.account_name.as_ref())
    );
}

async fn log_hls_origin_binding_reacquire_failed(session: &HlsSessionHandle, reason: &str) {
    let session_guard = session.read().await;
    debug!(
        "HLS origin binding reacquire failed: proxy_session={} reason={} retry_after_ms={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        reason,
        cold_start_retry_after_seconds().saturating_mul(1_000)
    );
}

async fn detach_unprotected_hls_origin_account_bindings(app_state: &Arc<AppState>, now_ms: u64) {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let binding = {
            let mut session_guard = session.write().await;
            let Some(binding) = session_guard.origin_account_binding.clone() else {
                continue;
            };
            if !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
                continue;
            }
            let timing = session_guard.account_overlap_timing();
            let protection = session_guard.account_binding_protection(now_ms);
            debug!(
                "HLS account protection classified: proxy_session={} state={} target_duration_ms={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                protection.as_log_state(),
                timing.target_duration_ms
            );
            if !matches!(protection, HlsAccountBindingProtection::Expired)
                || session_guard.activity.active_origin_work_count > 0
            {
                continue;
            }
            if !matches!(hls_origin_account_status(&app_state.hls_ctx(), &binding), HlsOriginAccountStatus::Known) {
                continue;
            }
            if let Some(binding) = session_guard.origin_account_binding.as_mut() {
                binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, now_ms);
            }
            session_guard.invalidate_queued_origin_work();
            debug!(
                "HLS origin binding detached: proxy_session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::SoftWindowElapsed.as_log_reason()
            );
            binding
        };
        app_state.active_provider.clear_provider_reservation(&binding.session_owner).await;
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn prepare_hls_origin_policy_preempt_runtime(
    app_state: &Arc<AppState>,
    new_session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let request_policy = HlsEffectiveOriginAcquirePolicy::new(connection_kind, priority, now_ms);
    let Some(candidate) =
        find_hls_origin_policy_preempt_candidate(app_state, input, proxy_session_id, request_policy, now_ms).await
    else {
        debug!("HLS origin policy preemption denied: reason=no-lower-origin-policy-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    app_state.active_provider.clear_provider_reservation(&candidate.session_owner).await;
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_exact_connection_with_grace_for_session(
            &candidate.account_name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=exact-acquire-failed");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=missing-provider-config");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=invalid-origin-url");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(binding) = origin_account_binding_from_allocation(
        Arc::clone(&input.name),
        proxy_session_id,
        &provider_handle.allocation,
        now_ms,
    ) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=invalid-allocation");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let mut detached_victim = false;
    {
        let mut victim = candidate.session.write().await;
        if let Some(victim_binding) = victim.origin_account_binding.as_mut() {
            if victim_binding.account_name == candidate.account_name
                && victim_binding.session_owner == candidate.session_owner
                && matches!(victim_binding.binding_mode, HlsOriginAccountBindingMode::Active)
            {
                victim_binding.detach(HlsOriginAccountDetachedReason::PreemptedByHigherPriority, now_ms);
                detached_victim = true;
            }
        }
        if detached_victim {
            victim.invalidate_queued_origin_work();
        }
    }
    if !detached_victim {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        restore_hls_origin_policy_preempt_candidate_reservation(app_state, &candidate).await;
        debug!("HLS origin policy preemption denied: reason=stale-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }

    {
        let mut session_guard = new_session.write().await;
        session_guard.replace_origin_account_binding(Some(binding.clone()));
    }
    debug!(
        "HLS origin policy preempted: account={} victim_proxy_session={} winner_proxy_session={} victim_kind={:?} victim_priority={} request_kind={:?} request_priority={}",
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        safe_proxy_session_id(&candidate.proxy_session_id),
        safe_proxy_session_id(proxy_session_id),
        candidate.victim_policy.connection_kind,
        candidate.victim_policy.priority,
        request_policy.connection_kind,
        request_policy.priority
    );
    debug!(
        "HLS origin binding detached: proxy_session={} account={} reason={}",
        safe_proxy_session_id(&candidate.proxy_session_id),
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        HlsOriginAccountDetachedReason::PreemptedByHigherPriority.as_log_reason()
    );

    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: Some(binding),
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

async fn restore_hls_origin_policy_preempt_candidate_reservation(
    app_state: &Arc<AppState>,
    candidate: &HlsOriginPolicyPreemptCandidate,
) {
    app_state
        .active_provider
        .refresh_provider_reservation(&candidate.account_name, &candidate.session_owner, candidate.reservation_ttl_secs)
        .await;
}

async fn find_hls_origin_policy_preempt_candidate(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    new_proxy_session_id: &ProxySessionId,
    request_policy: HlsEffectiveOriginAcquirePolicy,
    _now_ms: u64,
) -> Option<HlsOriginPolicyPreemptCandidate> {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    let mut best_candidate = None;
    for session in sessions {
        let session_guard = session.read().await;
        if session_guard.proxy_session_id == *new_proxy_session_id {
            continue;
        }
        let Some(binding) = session_guard.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != input.name || !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
            continue;
        }
        if session_guard.activity.active_origin_work_count > 0 {
            continue;
        }
        if !matches!(hls_origin_account_status(&app_state.hls_ctx(), binding), HlsOriginAccountStatus::Known) {
            continue;
        }
        let victim_policy = session_guard.effective_origin_acquire_policy_or_default();
        if !request_policy.is_better_than(victim_policy) {
            continue;
        }
        let candidate = HlsOriginPolicyPreemptCandidate {
            session: Arc::clone(&session),
            proxy_session_id: session_guard.proxy_session_id.clone(),
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            reservation_ttl_secs: session_guard.account_overlap_timing().reservation_ttl_secs(),
            victim_policy,
            last_media_at_ms: session_guard.activity.last_authorized_media_at_ms.unwrap_or_default(),
        };
        if hls_origin_policy_preempt_candidate_is_better(best_candidate.as_ref(), &candidate) {
            best_candidate = Some(candidate);
        }
    }
    best_candidate
}

fn hls_origin_policy_preempt_candidate_is_better(
    current: Option<&HlsOriginPolicyPreemptCandidate>,
    candidate: &HlsOriginPolicyPreemptCandidate,
) -> bool {
    let Some(current) = current else {
        return true;
    };
    match (candidate.victim_policy.connection_kind, current.victim_policy.connection_kind) {
        (crate::api::model::ConnectionKind::Soft, crate::api::model::ConnectionKind::Normal) => return true,
        (crate::api::model::ConnectionKind::Normal, crate::api::model::ConnectionKind::Soft) => return false,
        _ => {}
    }
    candidate.victim_policy.priority > current.victim_policy.priority
        || (candidate.victim_policy.priority == current.victim_policy.priority
            && candidate.last_media_at_ms < current.last_media_at_ms)
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hls_speculative_origin_runtime(
    app_state: &Arc<AppState>,
    new_session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let Some(candidate) = find_hls_account_overlap_candidate(app_state, input, proxy_session_id, now_ms).await else {
        debug!("HLS account overlap denied: reason=no-soft-active-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    app_state.active_provider.clear_provider_reservation(&candidate.session_owner).await;
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_exact_connection_with_grace_for_session(
            &candidate.account_name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        debug!("HLS account overlap denied: reason=speculative-acquire-failed");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        debug!("HLS account overlap denied: reason=missing-provider-config");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        debug!("HLS account overlap denied: reason=invalid-origin-url");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let binding = HlsOriginAccountBinding::speculative_from(
        Arc::clone(&input.name),
        Arc::clone(&candidate.account_name),
        proxy_session_id,
        candidate.proxy_session_id.clone(),
        candidate.reclaim_until_ms,
        now_ms,
    );
    {
        let mut session_guard = new_session.write().await;
        session_guard.replace_origin_account_binding(Some(binding.clone()));
    }
    debug!(
        "HLS account overlap granted: account={} victim_proxy_session={} winner_proxy_session={} reclaim_until_ms={} eligible_after_ms={} delay_ms={} tuliprox_target_user_connections={} origin_input_account_connections={}",
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        safe_proxy_session_id(&candidate.proxy_session_id),
        safe_proxy_session_id(proxy_session_id),
        candidate.reclaim_until_ms,
        candidate.soft_overlap_eligible_at_ms,
        candidate.soft_overlap_delay_ms,
        candidate.tuliprox_target_user_connection_capacity,
        candidate.origin_input_account_connection_capacity
    );
    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: Some(binding),
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

async fn find_hls_account_overlap_candidate(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    new_proxy_session_id: &ProxySessionId,
    now_ms: u64,
) -> Option<HlsAccountOverlapCandidate> {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    let tuliprox_target_user_connection_capacity = hls_tuliprox_target_user_connection_capacity(app_state, input).await;
    let origin_input_account_connection_capacity = hls_origin_input_account_connection_capacity(app_state, input).await;
    let mut speculative_accounts = Vec::new();
    for session in &sessions {
        let session = session.read().await;
        let Some(binding) = session.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != input.name {
            continue;
        }
        if matches!(
            binding.binding_mode,
            HlsOriginAccountBindingMode::Speculative { reclaim_until_ms, .. } if now_ms <= reclaim_until_ms
        ) {
            speculative_accounts.push(Arc::clone(&binding.account_name));
        }
    }

    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if session_guard.proxy_session_id == *new_proxy_session_id {
            continue;
        }
        let Some(binding) = session_guard.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != input.name
            || speculative_accounts.iter().any(|account| account == &binding.account_name)
        {
            continue;
        }
        if !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
            continue;
        }
        if session_guard.activity.active_origin_work_count > 0 {
            continue;
        }
        let timing = session_guard.account_overlap_timing();
        let protection = session_guard.account_binding_protection(now_ms);
        debug!(
            "HLS account protection classified: proxy_session={} state={} target_duration_ms={}",
            safe_proxy_session_id(&session_guard.proxy_session_id),
            protection.as_log_state(),
            timing.target_duration_ms
        );
        let HlsAccountBindingProtection::SoftActive { reclaim_until_ms } = protection else {
            continue;
        };
        let last_media_at_ms = session_guard.activity.last_authorized_media_at_ms.unwrap_or_default();
        let capacity = hls_soft_overlap_capacity_for_target_duration(
            tuliprox_target_user_connection_capacity,
            origin_input_account_connection_capacity,
            timing.target_duration_ms,
        );
        let eligible_at_ms = last_media_at_ms.saturating_add(capacity.delay_ms);
        if now_ms < eligible_at_ms {
            debug!(
                "HLS account overlap waiting: proxy_session={} account={} eligible_at_ms={} now_ms={} delay_ms={} tuliprox_target_user_connections={} origin_input_account_connections={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(binding.account_name.as_ref()),
                eligible_at_ms,
                now_ms,
                capacity.delay_ms,
                capacity.tuliprox_target_user_connection_capacity,
                capacity.origin_input_account_connection_capacity
            );
            continue;
        }
        candidates.push(HlsAccountOverlapCandidate {
            proxy_session_id: session_guard.proxy_session_id.clone(),
            input_name: Arc::clone(&binding.input_name),
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            reclaim_until_ms,
            last_media_at_ms,
            soft_overlap_eligible_at_ms: eligible_at_ms,
            soft_overlap_delay_ms: capacity.delay_ms,
            tuliprox_target_user_connection_capacity: capacity.tuliprox_target_user_connection_capacity,
            origin_input_account_connection_capacity: capacity.origin_input_account_connection_capacity,
        });
    }
    let mut eligible = filter_hls_account_overlap_cooldowns(app_state, candidates, now_ms).await;
    eligible.sort_by_key(|candidate| (candidate.last_media_at_ms, candidate.soft_overlap_eligible_at_ms));
    eligible.into_iter().next()
}

async fn filter_hls_account_overlap_cooldowns(
    app_state: &Arc<AppState>,
    candidates: Vec<HlsAccountOverlapCandidate>,
    now_ms: u64,
) -> Vec<HlsAccountOverlapCandidate> {
    let mut eligible = Vec::new();
    for candidate in candidates {
        if app_state
            .hls_proxy
            .is_account_overlap_cooling_down(&candidate.input_name, &candidate.account_name, now_ms)
            .await
        {
            debug!(
                "HLS account overlap skipped: proxy_session={} account={} reason=cooldown-active",
                safe_proxy_session_id(&candidate.proxy_session_id),
                sanitize_sensitive_info(candidate.account_name.as_ref())
            );
            continue;
        }
        eligible.push(candidate);
    }
    eligible
}

async fn reclaim_hls_account_overlap_if_needed(
    app_state: &Arc<AppState>,
    winner_session: &HlsSessionHandle,
    now_ms: u64,
) {
    let winner_proxy_session_id = winner_session.read().await.proxy_session_id.clone();
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let (loser_proxy_session_id, loser_binding) = {
            let session_guard = session.read().await;
            let Some(binding) = session_guard.origin_account_binding.clone() else {
                continue;
            };
            let HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms } =
                &binding.binding_mode
            else {
                continue;
            };
            if displaced_proxy_session_id != &winner_proxy_session_id || now_ms > *reclaim_until_ms {
                continue;
            }
            (session_guard.proxy_session_id.clone(), binding)
        };
        app_state.active_provider.clear_provider_reservation(&loser_binding.session_owner).await;
        {
            let mut loser = session.write().await;
            if let Some(binding) = loser.origin_account_binding.as_mut() {
                binding.detach(HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner, now_ms);
            }
            loser.invalidate_queued_origin_work();
        }
        {
            let mut winner = winner_session.write().await;
            if let Some(binding) = winner.origin_account_binding.as_mut() {
                binding.promote_to_active();
            }
        }
        let hard_active_window_ms = winner_session.read().await.account_overlap_timing().hard_active_window_ms;
        app_state
            .hls_proxy
            .mark_account_overlap_reclaimed_cooldown(
                Arc::clone(&loser_binding.input_name),
                Arc::clone(&loser_binding.account_name),
                now_ms,
                hard_active_window_ms,
            )
            .await;
        debug!(
            "HLS account overlap reclaimed: account={} winner={} loser={}",
            sanitize_sensitive_info(loser_binding.account_name.as_ref()),
            safe_proxy_session_id(&winner_proxy_session_id),
            safe_proxy_session_id(&loser_proxy_session_id)
        );
        debug!(
            "HLS origin binding detached: proxy_session={} account={} reason={}",
            safe_proxy_session_id(&loser_proxy_session_id),
            sanitize_sensitive_info(loser_binding.account_name.as_ref()),
            HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner.as_log_reason()
        );
    }
}

async fn promote_elapsed_hls_account_overlaps(app_state: &Arc<AppState>, now_ms: u64) {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let (input_name, account_name, promoted_session_id, displaced_session_id, hard_active_window_ms) = {
            let mut session_guard = session.write().await;
            let hard_active_window_ms = session_guard.account_overlap_timing().hard_active_window_ms;
            let Some(binding) = session_guard.origin_account_binding.as_mut() else {
                continue;
            };
            let HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms } =
                &binding.binding_mode
            else {
                continue;
            };
            if now_ms <= *reclaim_until_ms {
                continue;
            }
            let displaced_session_id = displaced_proxy_session_id.clone();
            let input_name = Arc::clone(&binding.input_name);
            let account_name = Arc::clone(&binding.account_name);
            binding.promote_to_active();
            (
                input_name,
                account_name,
                session_guard.proxy_session_id.clone(),
                displaced_session_id,
                hard_active_window_ms,
            )
        };
        app_state
            .hls_proxy
            .mark_account_overlap_promoted_cooldown(
                Arc::clone(&input_name),
                Arc::clone(&account_name),
                now_ms,
                hard_active_window_ms,
            )
            .await;
        if let Some(displaced) = app_state.hls_proxy.sessions().get_by_proxy_session_id(&displaced_session_id).await {
            let mut detached = false;
            let mut displaced = displaced.write().await;
            if displaced.origin_account_binding.as_ref().is_some_and(|binding| binding.account_name == account_name) {
                if let Some(binding) = displaced.origin_account_binding.as_mut() {
                    binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, now_ms);
                    detached = true;
                }
                displaced.invalidate_queued_origin_work();
            }
            if detached {
                debug!(
                    "HLS origin binding detached: proxy_session={} account={} reason={}",
                    safe_proxy_session_id(&displaced_session_id),
                    sanitize_sensitive_info(account_name.as_ref()),
                    HlsOriginAccountDetachedReason::SoftWindowElapsed.as_log_reason()
                );
            }
        }
        debug!(
            "HLS account overlap promoted: account={} proxy_session={}",
            sanitize_sensitive_info(account_name.as_ref()),
            safe_proxy_session_id(&promoted_session_id)
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn rebind_hls_origin_account(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    stale_binding: &HlsOriginAccountBinding,
    stale_status: HlsOriginAccountStatus,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    {
        let mut session_guard = session.write().await;
        if !session_guard.origin_account_rebind.is_allowed_now(now_ms) {
            debug!(
                "HLS origin account rebind skipped by backoff: proxy_session={} old_account={} retry_after_ms=2000",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref())
            );
            return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
        }
        session_guard.origin_account_rebind.mark_attempt_started(Arc::clone(&stale_binding.account_name), now_ms);
    }

    let safe_proxy_session = {
        let session_guard = session.read().await;
        safe_proxy_session_id(&session_guard.proxy_session_id)
    };
    debug!(
        "HLS origin account rebind started: proxy_session={} old_account={} reason={stale_status:?}",
        safe_proxy_session,
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
    app_state.active_provider.clear_provider_reservation(&stale_binding.session_owner).await;
    {
        let mut session_guard = session.write().await;
        if let Some(binding) = session_guard.origin_account_binding.as_mut().filter(|binding| {
            binding.account_name == stale_binding.account_name && binding.session_owner == stale_binding.session_owner
        }) {
            binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, now_ms);
            debug!(
                "HLS origin binding detached: proxy_session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::AccountMissingOrExpired.as_log_reason()
            );
        }
        session_guard.invalidate_queued_origin_work();
    }

    let Some(provider_handle) = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&stale_binding.session_owner),
        )
        .await
    else {
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_account_available").await;
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: hls_no_account_reason_for_binding(Some(stale_binding)),
        });
    };

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_provider_config").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "invalid_origin_url").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(new_account_name) = provider_handle.allocation.get_provider_name() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "missing_account_name").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let new_binding = HlsOriginAccountBinding::rebound(
        Arc::clone(&input.name),
        new_account_name,
        stale_binding.session_owner.clone(),
        stale_binding.generation.saturating_add(1),
        now_ms,
    );
    {
        let mut session_guard = session.write().await;
        session_guard.replace_origin_account_binding(Some(new_binding.clone()));
        session_guard.origin_account_rebind.mark_success();
    }
    debug!(
        "HLS origin account rebound: old_account={} new_account={}",
        sanitize_sensitive_info(stale_binding.account_name.as_ref()),
        sanitize_sensitive_info(new_binding.account_name.as_ref())
    );

    Ok(PreparedHlsOriginRuntime {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        fetch_url,
        origin_account_binding_to_store: None,
        preacquired_origin_account_handle: Some(provider_handle),
    })
}

async fn mark_hls_origin_rebind_failed(
    session: &HlsSessionHandle,
    stale_binding: &HlsOriginAccountBinding,
    now_ms: u64,
    reason: &str,
) {
    let mut session_guard = session.write().await;
    session_guard.origin_account_rebind.mark_failed(now_ms);
    debug!(
        "HLS origin account rebind failed: proxy_session={} old_account={} reason={reason} retry_after_ms=2000",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hls_cache_user_session(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    session_token: &str,
    virtual_id: u32,
    request_url: &str,
    input: &ConfigInput,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
) -> String {
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id,
            provider: input.name.as_ref(),
            stream_url: request_url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind,
            socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
        })
        .await
}

fn hls_entry_origin_connection_kind(
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
) -> Option<crate::api::model::ConnectionKind> {
    match connection_permission {
        UserConnectionPermission::Allowed | UserConnectionPermission::GracePeriod => connection_kind,
        UserConnectionPermission::Exhausted => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_hls_cache_entry_master_playlist_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    origin_source: HlsOriginSource,
    virtual_id: u32,
    existing_user_session: Option<&UserSession>,
    known_bitrate_bps: Option<u32>,
    session_token_hint: Option<&str>,
    request_url: &str,
    input: &ConfigInput,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
    server_path: Option<&str>,
) -> axum::response::Response {
    let item_bandwidth = HlsMasterBandwidth::new(known_bitrate_bps);
    let database_bitrate_bps = if item_bandwidth.is_unknown() {
        match load_input_live_bitrate_bps(&app_state.app_config, input, &origin_source.stream_ref).await {
            Ok(known_bitrate_bps) => known_bitrate_bps,
            Err(err) => {
                warn!("HLS entry live bitrate lookup failed; using fallback: input_id={} error={err}", input.id);
                None
            }
        }
    } else {
        None
    };
    let bandwidth = HlsMasterBandwidthSelection::resolve(known_bitrate_bps, database_bitrate_bps);
    let known_bitrate_bps = bandwidth.known_bitrate_bps();

    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let family_key = HlsPlaybackFamilyKey::new(user.username.clone(), fingerprint.key.clone());
    let now_ms = current_time_millis();
    let origin_connection_kind = hls_entry_origin_connection_kind(connection_permission, connection_kind);
    let access_lease_id = new_hls_access_lease_id();
    let existing_token = existing_user_session.map(|session| session.token.as_str()).or(session_token_hint);
    let session_token = create_hls_cache_user_session_token(
        fingerprint,
        &user.username,
        virtual_id,
        existing_token,
        origin_source.archive_reference,
    );
    let session_token = prepare_hls_cache_user_session(
        app_state,
        fingerprint,
        user,
        &session_token,
        virtual_id,
        request_url,
        input,
        connection_permission,
        origin_connection_kind,
    )
    .await;
    let mut lease = HlsAccessLease::pending(
        access_lease_id.clone(),
        family_key,
        proxy_session_id.clone(),
        user.username.clone(),
        session_token.clone(),
        origin_source.input_id,
        origin_source.stream_ref.clone(),
        virtual_id,
        now_ms,
        hls_pending_bootstrap_window_ms(app_state),
    )
    .with_known_bitrate_bps(known_bitrate_bps)
    .with_archive_playback(
        origin_source.archive_reference,
        origin_source.archive_reference.map(|_| request_url.to_string()),
    );
    if let Some(connection_kind) = origin_connection_kind {
        lease = lease.with_origin_acquire_policy(connection_kind, connection_priority_for_kind(user, connection_kind));
    } else {
        lease.state = HlsAccessLeaseState::Denied;
    }
    app_state.hls_proxy.prepare_access_lease(lease).await;
    debug!(
        "HLS access lease prepared: lease={} session={} proxy_session={} user_session={} action=created reason=new-playback",
        safe_hls_access_lease_id(&access_lease_id),
        safe_session_key(&session_key),
        safe_proxy_session_id(&proxy_session_id),
        safe_user_session_token(&session_token)
    );
    let response =
        hls_entry_master_playlist_response(&proxy_session_id, &access_lease_id, bandwidth.bandwidth(), server_path);
    app_state.hls_proxy.startup_observability().record_entry_master_response(
        access_lease_id.clone(),
        HlsLogIdentity::new(&session_key, &proxy_session_id),
        current_time_millis(),
    );
    debug!(
        "HLS master playlist response: {}",
        HlsMasterPlaylistResponseDiagnostic {
            lease: safe_hls_access_lease_id(&access_lease_id),
            session: safe_session_key(&session_key),
            proxy_session: safe_proxy_session_id(&proxy_session_id),
            user_session: safe_user_session_token(&session_token),
            virtual_id,
            bandwidth_bps: bandwidth.bandwidth().advertised_bps(),
            bandwidth_source: bandwidth.source().as_log_value(),
            content_length: response.content_length,
        }
    );
    response.response
}

struct HlsEntryMasterPlaylistResponse {
    response: axum::response::Response,
    content_length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsMasterPlaylistResponseDiagnostic {
    lease: String,
    session: String,
    proxy_session: String,
    user_session: String,
    virtual_id: u32,
    bandwidth_bps: u32,
    bandwidth_source: &'static str,
    content_length: usize,
}

impl std::fmt::Display for HlsMasterPlaylistResponseDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "lease={} session={} proxy_session={} user_session={} virtual_id={} bandwidth_bps={} bandwidth_source={} status=200 content_length={}",
            self.lease,
            self.session,
            self.proxy_session,
            self.user_session,
            self.virtual_id,
            self.bandwidth_bps,
            self.bandwidth_source,
            self.content_length
        )
    }
}

fn hls_entry_master_playlist_response(
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

fn hls_canonical_manifest_path(proxy_session_id: &ProxySessionId, access_lease_id: &HlsAccessLeaseId) -> String {
    format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, access_lease_id.0)
}

fn hls_canonical_retry_after_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        .header(axum::http::header::RETRY_AFTER, cold_start_retry_after_seconds().to_string())
        .body(axum::body::Body::empty()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsCanonicalOwnerRegistration {
    Join(HlsCanonicalOwnerRegistrationKind),
    FailClosed(HlsCanonicalOwnerRegistrationFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsCanonicalOwnerRegistrationKind {
    Scheduled,
    AlreadyOwned,
}

impl HlsCanonicalOwnerRegistrationKind {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::AlreadyOwned => "already_owned",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsCanonicalOwnerRegistrationFailure {
    CapacityExceeded,
    RuntimeUnavailable,
}

impl HlsCanonicalOwnerRegistrationFailure {
    const fn as_label(self) -> &'static str {
        match self {
            Self::CapacityExceeded => "capacity_exceeded",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

const fn hls_canonical_owner_registration(
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

fn hls_availability_reevaluation_registration_failure_response(
    failure: HlsCanonicalOwnerRegistrationFailure,
) -> axum::response::Response {
    warn!("HLS availability reevaluation not registered: reason={}", failure.as_label());
    hls_canonical_retry_after_response()
}

enum HlsCanonicalOwnerResolution {
    Live(axum::response::Response),
    Terminal(axum::response::Response),
    Standalone(axum::response::Response),
    FailedClosed { reason: HlsCanonicalOwnerFailureReason, response: axum::response::Response },
}

impl HlsCanonicalOwnerResolution {
    const fn outcome_label(&self) -> &'static str {
        match self {
            Self::Live(_) => "live",
            Self::Terminal(_) => "terminal",
            Self::Standalone(_) => "standalone",
            Self::FailedClosed { reason, .. } => reason.as_label(),
        }
    }

    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Live(response)
            | Self::Terminal(response)
            | Self::Standalone(response)
            | Self::FailedClosed { response, .. } => response,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsCanonicalOwnerFailureReason {
    Superseded,
    DeadlineElapsed,
    LeaseUnavailable,
}

impl HlsCanonicalOwnerFailureReason {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::DeadlineElapsed => "deadline_elapsed",
            Self::LeaseUnavailable => "lease_unavailable",
        }
    }

    fn response(self) -> axum::response::Response {
        let reason = match self {
            Self::DeadlineElapsed => HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed,
            Self::Superseded | Self::LeaseUnavailable => HlsTerminalFailedClosedReason::LeaseStateUnavailable,
        };
        hls_terminal_failed_closed_response(reason)
    }
}

struct HlsCanonicalOwnerPending {
    deadline_ms: u64,
    current_session_available: bool,
}

enum HlsCanonicalOwnerEvaluation {
    Resolved(HlsCanonicalOwnerResolution),
    Pending(HlsCanonicalOwnerPending),
}

fn hls_canonical_owner_failed(reason: HlsCanonicalOwnerFailureReason) -> HlsCanonicalOwnerEvaluation {
    HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::FailedClosed {
        reason,
        response: reason.response(),
    })
}

struct HlsCanonicalOwnerHandoffContext<'a> {
    app_state: &'a Arc<AppState>,
    proxy_session_id: &'a ProxySessionId,
    access_lease_id: &'a HlsAccessLeaseId,
    expected_lease_issued_at_ms: Option<u64>,
    strip: &'a crate::model::StripConfig,
    server_path: Option<&'a str>,
    manifest_commit_requirement: HlsManifestCommitRequirement,
    manifest_boundary_rendered_at_ms: u64,
    bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
    request_deadline_ms: u64,
    safe_session: String,
}

fn hls_canonical_owner_lease_deadline_ms(lease: &HlsAccessLease) -> u64 {
    if lease.state == HlsAccessLeaseState::Pending {
        lease.pending_deadline_ms().unwrap_or(lease.valid_until_ms)
    } else {
        lease.valid_until_ms
    }
}

fn hls_canonical_owner_request_deadline_ms(lease: &HlsAccessLease, wait_timeout: Duration, now_ms: u64) -> u64 {
    let lease_deadline_ms = hls_canonical_owner_lease_deadline_ms(lease);
    if wait_timeout.is_zero() && lease.state == HlsAccessLeaseState::Pending {
        lease_deadline_ms
    } else {
        lease_deadline_ms.min(now_ms.saturating_add(duration_to_millis_saturating(wait_timeout)))
    }
}

async fn evaluate_hls_canonical_owner_handoff(
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

async fn finalize_hls_canonical_owner_handoff(
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

async fn join_hls_canonical_manifest_owner(
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

async fn hls_direct_refresh_follow_up(
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
enum HlsManifestRefreshOrdering {
    Background,
    AwaitBeforeTerminalEvaluation,
}

async fn trigger_hls_canonical_manifest_refresh(
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

fn hls_canonical_status_response(status: StatusCode) -> axum::response::Response {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        hls_canonical_retry_after_response()
    } else {
        status.into_response()
    }
}

struct HlsEntryOriginAccountReservation {
    request_url: String,
    session_token: String,
    provider_handle: Option<ProviderHandle>,
    selected_provider_config: Option<Arc<RuntimeProviderConfig>>,
}

#[allow(clippy::too_many_arguments)]
async fn try_reserve_hls_entry_origin_account_for_redirect(
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
async fn try_reserve_hls_virtual_entry_origin_account_for_redirect(
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
        PlaylistItemType::LiveHls,
        None,
        &session_token,
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id },
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

async fn mark_hls_provisioning_handoff_discontinuity(
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

async fn mark_hls_provisioning_handoff_discontinuity_once_for_session(
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

async fn mark_hls_provisioning_handoff_discontinuity_for_session(session: &HlsSessionHandle, now_ms: u64) {
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

fn clear_hls_provisioning_handoff_consumer(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    virtual_id: u32,
    now_ms: u64,
) {
    if !app_state.hls_provisioning.take_ready_slot_for_consumer(&input.name, virtual_id, now_ms) {
        app_state.hls_provisioning.clear_consumer(&input.name, virtual_id);
    }
}

async fn maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
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

async fn latest_shared_hls_manifest_rendered_at_ms(session: &HlsSessionHandle) -> u64 {
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

enum HlsProvisioningPollResponseKind {
    Legacy,
}

impl HlsProvisioningPollResponseKind {
    fn access_lease_id(&self) -> Option<&HlsAccessLeaseId> {
        match self {
            Self::Legacy => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn hls_panel_provisioning_poll_response(
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

async fn hls_panel_provisioning_or_status_response(
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
struct SharedHlsProvisioningSegmentPlan {
    proxy_seq: u64,
    physical_index: usize,
    cache_key: SegmentCacheKey,
    segment_kind: SharedHlsProvisioningLocalSegmentKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SharedHlsProvisioningLocalSegmentKind {
    Provisioning,
    Gap,
}

fn shared_hls_provisioning_segment_plans(
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

fn shared_hls_provisioning_segment_entry(
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

async fn commit_shared_hls_provisioning_segments(
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

async fn ensure_shared_hls_provisioning_handoff_gap(
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

async fn hls_shared_provisioning_timeline_manifest_response(
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
async fn hls_shared_provisioning_or_provider_exhausted_response(
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

enum HlsProviderExhaustedResolution {
    RetryAcquire,
    Response(axum::response::Response),
}

#[allow(clippy::too_many_arguments)]
async fn hls_provider_connections_exhausted_manifest_resolution(
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
async fn prepare_hls_canonical_manifest_origin_runtime(
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
async fn try_hls_cache_canonical_manifest_response(
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

fn hls_initial_manifest_decision_wait_timeout(app_state: &Arc<AppState>) -> Duration {
    Duration::from_secs(app_state.hls_proxy.initial_manifest_wait_timeout_secs())
}

async fn hls_manifest_wait_timeout_for_requirement(
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

async fn touch_initial_manifest_access_lease_window(
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

async fn hls_initial_manifest_wait_timeout(app_state: &Arc<AppState>, session: &HlsSessionHandle) -> Duration {
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

async fn hls_segment_request_requires_origin_work(session: &HlsSessionHandle, segment_file: &HlsSegmentFile) -> bool {
    let session = session.read().await;
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return false;
    };
    if entry.proxy_file_ext != segment_file.extension {
        return false;
    }
    matches!(entry.status, SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. })
        && entry.origin_fetch_ref.is_some()
}

async fn hls_origin_binding_needs_reacquire(session: &HlsSessionHandle) -> bool {
    let session = session.read().await;
    session.origin_account_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached)
}

fn hls_transient_origin_binding_requires_runtime_prepare(hls_ctx: &HlsCtx, binding: &HlsOriginAccountBinding) -> bool {
    binding.is_detached()
        || (binding.is_active()
            && matches!(
                hls_origin_account_status(hls_ctx, binding),
                HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired
            ))
}

async fn prepare_hls_origin_binding_for_authorized_resource_work(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    work_kind: HlsOriginWorkKind,
    now_ms: u64,
) -> Result<Option<ProviderHandle>, HlsOriginRuntimeAcquireError> {
    if !hls_origin_binding_needs_reacquire(session).await {
        return Ok(None);
    }
    if session.read().await.activity.active_origin_work_count > 0 {
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }
    let request_context = resolve_hls_playback_manifest_request_context(app_state, access_context, req_headers)
        .await
        .map_err(HlsOriginRuntimeAcquireError::Fatal)?;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let origin_policy = hls_effective_origin_acquire_policy(session).await;
    let prepared_origin = prepare_hls_origin_runtime(
        app_state,
        session,
        &request_context.input,
        &request_context.hls_url,
        request_context.session_entry_url.as_str(),
        &proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        work_kind,
        HlsOriginWorkClass::Demand,
        now_ms,
    )
    .await?;
    if let Some(binding) = prepared_origin.origin_account_binding_to_store {
        session.write().await.replace_origin_account_binding(Some(binding));
    }
    Ok(prepared_origin.preacquired_origin_account_handle)
}

#[allow(clippy::too_many_lines)]
async fn prepare_hls_transient_origin_io_for_authorized_resource_work(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    now_ms: u64,
) -> Result<Option<HlsTransientOriginIoGuard>, HlsOriginRuntimeAcquireError> {
    let hls_ctx = app_state.hls_ctx();
    let existing_binding = session.read().await.origin_account_binding.clone();
    let origin_policy = hls_effective_origin_acquire_policy(session).await;
    let reservation_ttl_secs = hls_origin_account_reservation_ttl_secs_for_session(session).await;
    if let Some(binding) = existing_binding.as_ref().filter(|binding| binding.is_active()) {
        match hls_origin_account_status(&hls_ctx, binding) {
            HlsOriginAccountStatus::Known => {
                let origin_io = HlsOriginIoContext {
                    ctx: hls_ctx.clone(),
                    client_addr: fingerprint.addr,
                    allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
                    priority: origin_policy.priority,
                    connection_kind: origin_policy.connection_kind,
                    reservation_ttl_secs,
                    preacquired_provider_handle: None,
                    started_generation: None,
                };
                let started_generation = session.write().await.start_origin_work();
                if let Ok(lease_guard) = begin_hls_origin_account_io_bounded(
                    &origin_io,
                    session,
                    binding,
                    hls_object_body_deadline(app_state.hls_proxy.segment_fetch_policy().origin_segment_timeout_ms),
                )
                .await
                {
                    return Ok(Some(HlsTransientOriginIoGuard::new(
                        Arc::clone(session),
                        origin_io,
                        lease_guard,
                        started_generation,
                    )));
                }
                session.write().await.finish_origin_work(started_generation);
                return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
            }
            HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired => {}
        }
    }

    if !existing_binding
        .as_ref()
        .is_some_and(|binding| hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, binding))
    {
        return Ok(None);
    }
    if session.read().await.activity.active_origin_work_count > 0 {
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }
    let request_context = resolve_hls_playback_manifest_request_context(app_state, access_context, req_headers)
        .await
        .map_err(HlsOriginRuntimeAcquireError::Fatal)?;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let prepared_origin = prepare_hls_origin_runtime(
        app_state,
        session,
        &request_context.input,
        &request_context.hls_url,
        request_context.session_entry_url.as_str(),
        &proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        HlsOriginWorkKind::Resource,
        HlsOriginWorkClass::Demand,
        now_ms,
    )
    .await?;
    if let Some(binding) = prepared_origin.origin_account_binding_to_store {
        session.write().await.replace_origin_account_binding(Some(binding));
    }
    let Some(provider_handle) = prepared_origin.preacquired_origin_account_handle else {
        return Ok(None);
    };
    let Some(binding) = session.read().await.origin_account_binding.clone().filter(HlsOriginAccountBinding::is_active)
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let origin_io = HlsOriginIoContext {
        ctx: hls_ctx,
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    }
    .with_preacquired_provider_handle(provider_handle);
    let started_generation = session.write().await.start_origin_work();
    let Ok(lease_guard) = begin_hls_origin_account_io_bounded(
        &origin_io,
        session,
        &binding,
        hls_object_body_deadline(app_state.hls_proxy.segment_fetch_policy().origin_segment_timeout_ms),
    )
    .await
    else {
        session.write().await.finish_origin_work(started_generation);
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    Ok(Some(HlsTransientOriginIoGuard::new(Arc::clone(session), origin_io, lease_guard, started_generation)))
}

struct HlsCachedManifestRead {
    transient_body: Option<(String, u64)>,
    rendered_body: Option<String>,
    should_wait: bool,
    wait_for_initial_commit: bool,
}

async fn read_hls_cached_manifest(
    session: &HlsSessionHandle,
    options: HlsCachedManifestOptions,
    started_at_ms: u64,
) -> HlsCachedManifestRead {
    let session = session.read().await;
    let now_ms = current_time_millis();
    let should_wait = session.initial_manifest_commit_work_pending();
    let committed_body = hls_committed_manifest_body_for_request(&session, options, started_at_ms, now_ms);
    let (transient_body, rendered_body) = match committed_body {
        Some(HlsCommittedManifestBody::Transient(body)) => {
            (Some((body, session.transient.last_manifest_rendered_at_ms.unwrap_or(started_at_ms))), None)
        }
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

struct HlsCachedManifestViewContext<'a> {
    proxy_session_id: &'a ProxySessionId,
    access_lease_id: &'a HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &'a crate::model::StripConfig,
    server_path: Option<&'a str>,
    bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
}

#[derive(Clone, Copy)]
enum HlsRuntimeBandwidthLearningContext<'a> {
    Disabled,
    Eligible(&'a ConfigInput),
}

impl HlsCachedManifestViewContext<'_> {
    fn new<'a>(
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

    fn materialize(&self, body: &str, mode: &'static str) -> HlsMaterializedSharedManifest {
        materialize_shared_hls_access_manifest(
            body,
            self.access_lease_id,
            self.access_lease_state,
            self.strip,
            mode,
            self.server_path,
        )
    }

    async fn finish(
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

fn spawn_hls_runtime_bandwidth_persistence(
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

fn hls_bandwidth_persistence_outcome(
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

fn hls_cached_manifest_temporarily_unavailable() -> axum::response::Response {
    hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn try_hls_cached_manifest_response(
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
            let prepared = if let Some((body, source_rendered_at_ms)) = cached.transient_body {
                let materialized = view.materialize(&body, "transient");
                let delivered_at_ms = current_time_millis();
                let snapshot = derive_hls_lease_manifest_snapshot(
                    &HlsLeaseManifestSnapshotInput::TransientPassthrough {
                        materialized_body: &materialized.body,
                        source_rendered_at_ms,
                    },
                    delivered_at_ms,
                );
                let Some(snapshot) = snapshot else {
                    return Some(hls_cached_manifest_temporarily_unavailable());
                };
                Some((materialized, snapshot, delivered_at_ms))
            } else if let Some(body) = cached.rendered_body {
                let materialized = view.materialize(&body, "normal");
                let delivered_at_ms = current_time_millis();
                let snapshot = {
                    let session = session.read().await;
                    derive_hls_lease_manifest_snapshot(
                        &HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
                            session: &session,
                            committed_body: &body,
                            materialized_body: &materialized.body,
                            stripped_tail_segments: stripped_tail_segments(&materialized),
                        },
                        delivered_at_ms,
                    )
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
                Some((materialized, snapshot, delivered_at_ms))
            } else {
                None
            };
            if let Some((materialized, snapshot, delivered_at_ms)) = prepared {
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
                    .commit_access_lease_manifest_publication(
                        access_lease_id,
                        &proxy_session_id,
                        publication_guard,
                        snapshot,
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

async fn wait_for_hls_startup_evidence(started_at: tokio::time::Instant, wait_timeout: Duration) -> bool {
    let elapsed = started_at.elapsed();
    if wait_timeout.is_zero() || elapsed >= wait_timeout {
        return false;
    }
    tokio::time::sleep(wait_timeout.saturating_sub(elapsed).min(Duration::from_millis(25))).await;
    true
}

async fn mark_successful_canonical_manifest_activity(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    mark_hls_authorized_media_access(app_state, session, now_ms).await;
}

async fn mark_hls_authorized_manifest_access(app_state: &Arc<AppState>, session: &HlsSessionHandle, now_ms: u64) {
    session.write().await.mark_authorized_manifest_access(now_ms);
    app_state.hls_proxy.schedule_session_idle_for_handle(session).await;
}

async fn mark_hls_authorized_media_access(app_state: &Arc<AppState>, session: &HlsSessionHandle, now_ms: u64) {
    session.write().await.mark_authorized_media_access(now_ms);
    app_state.hls_proxy.schedule_session_idle_for_handle(session).await;
}

fn hls_cache_configured(app_state: &Arc<AppState>) -> bool {
    let config = app_state.app_config.config.load();
    config.reverse_proxy.as_ref().is_some_and(|reverse_proxy| reverse_proxy.hls_cache.is_some())
}

fn hls_cache_enabled_for_target(app_state: &Arc<AppState>, target: &ConfigTarget) -> bool {
    hls_cache_configured(app_state) && is_hls_stream_share_enabled(target)
}

pub(in crate::api) fn build_virtual_hls_entry_path(
    target: &ConfigTarget,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    virtual_id: u32,
) -> String {
    if input.input_type.is_m3u() && !target.has_output(TargetType::Xtream) {
        format!("/{}/live/{}/{}/{}{HLS_EXT}", storage_const::M3U_STREAM_PATH, user.username, user.password, virtual_id)
    } else {
        format!("/live/{}/{}/{}{HLS_EXT}", user.username, user.password, virtual_id)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn handle_hls_stream_request(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    user_session: Option<&UserSession>,
    session_token_hint: Option<&str>,
    hls_url: &str,
    archive_reference: Option<i64>,
    stream_context: HlsEntryStreamContext,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
    original_hls_entry_path: &str,
) -> impl IntoResponse + Send {
    let virtual_id = stream_context.virtual_id();
    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let stream_ref = stream_context.stream_ref().to_string();
    let normalized_hls_url = normalize_xtream_live_hls_url(hls_url, input);
    if normalized_hls_url != hls_url {
        debug_if_enabled!(
            "Normalized xtream hls url from {} to {}",
            sanitize_sensitive_info(hls_url),
            sanitize_sensitive_info(&normalized_hls_url)
        );
    }
    let url = ensure_hls_manifest_extension(&normalized_hls_url);
    // Recover archive context when callers (esp. Xtream timeshift) pass None but the
    // resolved provider URL / catchup session still carries Flussonic archive markers.
    let archive_reference = archive_reference.or_else(|| m3u_archive_epg_reference_ts(&url)).or_else(|| {
        user_session
            .map(|session| session.token.as_str())
            .or(session_token_hint)
            .and_then(m3u_catchup_epg_reference_from_session_token)
    });
    let hls_cache_origin = build_hls_origin_resolution(input, &url);
    let hls_origin_source = hls_cache_origin.as_ref().map(|_| {
        build_hls_origin_source_for_playback(input, stream_ref.clone(), archive_reference, Some(url.as_str()))
    });
    let server_info = app_state.app_config.get_user_server_info(user);
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        stream_context.identity().upstream_user_agent(),
    );

    if hls_cache_enabled_for_target(app_state, target) {
        let Some(origin_source) = hls_origin_source.clone() else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        return create_hls_cache_entry_master_playlist_response(
            app_state,
            fingerprint,
            user,
            origin_source,
            virtual_id,
            user_session,
            stream_context.known_bitrate_bps(),
            session_token_hint,
            if archive_reference.is_some() {
                url.as_str()
            } else {
                hls_cache_origin.as_ref().map_or(url.as_str(), |origin| origin.session_entry_url.as_str())
            },
            input,
            connection_permission,
            connection_kind,
            server_info.path.as_deref(),
        )
        .await;
    }

    let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
    let (request_url, session_token, provider_handle, _selected_provider_config) = if let Some(session) = user_session {
        let pinned_provider = if session.provider.is_empty() { &input.name } else { &session.provider };
        let provider_handle = if let Some(handle) = app_state
            .active_provider
            .acquire_exact_connection_with_grace_for_session(
                pinned_provider,
                &fingerprint.addr,
                false,
                connection_priority_for_kind(
                    user,
                    session.connection_kind.or(connection_kind).unwrap_or(crate::api::model::ConnectionKind::Normal),
                ),
                session.connection_kind.or(connection_kind).unwrap_or(crate::api::model::ConnectionKind::Normal),
                Some(session.token.as_str()),
            )
            .await
        {
            Some(handle)
        } else {
            debug_if_enabled!(
                "HLS pinned provider {} unavailable for {}; aborting allocation to prevent mid-session migration",
                sanitize_sensitive_info(pinned_provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            None
        };

        if provider_handle.is_none() {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        }
        match provider_handle.as_ref().map(|handle| &handle.allocation) {
            Some(ProviderAllocation::Exhausted) => (url, None, provider_handle, None),
            Some(ProviderAllocation::Available(cfg) | ProviderAllocation::GracePeriod(cfg)) => {
                let selected_provider_config = Arc::clone(cfg);
                let Some(stream_url) = get_stream_alternative_url(&url, input, cfg) else {
                    app_state.connection_manager.release_provider_handle(provider_handle).await;
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                let session_token = app_state
                    .active_users
                    .create_user_session(crate::api::model::CreateUserSessionParams {
                        user,
                        session_token: &session.token,
                        virtual_id,
                        provider: &cfg.name,
                        stream_url: &stream_url,
                        addr: &fingerprint.addr,
                        connection_permission,
                        connection_kind: session.connection_kind.or(connection_kind),
                        socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                    })
                    .await;
                let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);
                app_state
                    .active_provider
                    .refresh_provider_reservation(&cfg.name, &session_token, hls_session_ttl_secs)
                    .await;
                (stream_url, Some(session_token), provider_handle, Some(selected_provider_config))
            }
            None => (url, None, None, None),
        }
    } else {
        // Append/shift catchup must keep an m3u-catchup session token even when shared HLS
        // cache is off; otherwise rewritten segments register as LiveHls in the panel.
        let user_session_token = hls_entry_user_session_token(
            fingerprint,
            &user.username,
            virtual_id,
            session_token_hint,
            archive_reference,
        );
        let hls_session_owner = if hls_cache_enabled_for_target(app_state, target) {
            let session_key = HlsSessionKey::new(input.id, stream_context.stream_ref());
            let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
            Some(build_hls_origin_session_owner(&proxy_session_id))
        } else {
            None
        };
        let session_owner = hls_session_owner.as_deref().unwrap_or(user_session_token.as_str());
        let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);
        let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
            app_state,
            fingerprint,
            user,
            input,
            virtual_id,
            &url,
            &user_session_token,
            session_owner,
            hls_session_ttl_secs,
            connection_permission,
            fallback_connection_kind,
            true,
        )
        .await
        else {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };
        debug_if_enabled!(
            "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} provider={} stream_url={}",
            sanitize_sensitive_info(&user.username),
            reservation.selected_provider_config.as_ref().map_or("<unknown>", |provider| provider.name.as_ref()),
            sanitize_sensitive_info(&reservation.request_url)
        );
        (
            reservation.request_url,
            Some(reservation.session_token),
            reservation.provider_handle,
            reservation.selected_provider_config,
        )
    };

    // Playlist requests only need the chosen provider account to derive the URL and pin the session.
    // Holding the provider slot until the first segment request causes stale active connections and
    // breaks forced same-account reuse on the next HLS/Catchup stream request.
    app_state.connection_manager.release_provider_handle(provider_handle).await;

    let input_source = InputSource::from(input).with_url(request_url);
    let download_result = download_legacy_hls_manifest(app_state, &input_source, &headers).await;
    match download_result {
        Ok((content, response_url, response_headers)) => {
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id: target.id,
                virtual_id,
                input_id: input.id,
                user_token: session_token.as_deref(),
            };
            let hls_content = rewrite_hls(user, &rewrite_hls_props);
            if let Some(session_token) = session_token.as_deref() {
                let session_headers = extract_hls_provider_session_headers(&response_headers);
                if !session_headers.is_empty() {
                    app_state
                        .active_users
                        .update_session_provider_headers(&user.username, session_token, &session_headers)
                        .await;
                }
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                    .await;
            }
            hls_response(hls_content).into_response()
        }
        Err(err) => {
            error!("Failed to download m3u8: {}", request::text_response_error_log_label(&err));
            if let Some(session_token) = session_token.as_deref() {
                terminate_failed_hls_manifest_session(app_state, &user.username, session_token).await;
            }

            hls_custom_video_manifest_response(
                app_state,
                user,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await
        }
    }
}

async fn get_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<StreamChannel> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(pli) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            return Some(pli.to_stream_channel(target.id));
        }
    }
    let target_id = target.id;
    m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
        .await
        .ok()
        .map(|pli| pli.to_stream_channel(target_id))
}

fn hls_stream_context_or_unavailable(
    item: &impl PlaylistEntry,
    virtual_id: u32,
) -> Result<HlsEntryStreamContext, StatusCode> {
    HlsEntryStreamContext::from_playlist_item(item).ok_or_else(|| {
        warn!("HLS input stream identity missing for virtual_id={virtual_id}; refresh target playlist");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

pub(in crate::api) async fn resolve_hls_virtual_source_for_target(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Result<HlsResolvedVirtualSource, StatusCode> {
    let (input_name, stream_context) = if target.has_output(TargetType::Xtream) {
        if let Ok(item) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
            (Arc::clone(&item.input_name), stream_context)
        } else {
            let item = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
                .await
                .map_err(|_| StatusCode::NOT_FOUND)?;
            let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
            (Arc::clone(&item.input_name), stream_context)
        }
    } else {
        let item = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
        (Arc::clone(&item.input_name), stream_context)
    };
    let input = app_state.app_config.get_input_by_name(&input_name).ok_or(StatusCode::NOT_FOUND)?;
    Ok(HlsResolvedVirtualSource { input, stream_context })
}

async fn resolve_hls_origin_playlist_url(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
    fallback_url: &str,
) -> Result<String, StatusCode> {
    if input.input_type.is_xtream() && target.has_output(TargetType::Xtream) {
        let pli = xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let hls_extension = format!(".{HLS_EXT}");
        let (query_path, _) = get_query_path("", Some(&hls_extension), &pli, app_state);
        return get_xtream_player_api_stream_url(input, ApiStreamContext::Live, &query_path, &pli.url)
            .map(|url| url.to_string())
            .ok_or(StatusCode::SERVICE_UNAVAILABLE);
    }

    Ok(fallback_url.to_string())
}

async fn resolve_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &Arc<ConfigInput>,
    virtual_id: u32,
    hls_url: &str,
    archive_reference: Option<i64>,
    session_token: Option<&str>,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    let mut channel = match get_stream_channel(app_state, target, virtual_id).await {
        Some(mut channel) => {
            channel.url = Arc::from(hls_url);
            channel
        }
        None => StreamChannel {
            target_id: target.id,
            virtual_id,
            provider_id: 0,
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::LiveHls,
            cluster: XtreamCluster::Live,
            group: unknown.clone(),
            title: unknown,
            url: Arc::from(hls_url),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        },
    };

    let archive_reference = archive_reference.or_else(|| epg_reference_ts_from_date_tree_path(hls_url));
    // Append/shift catchup often loses utc/utcstart on rewritten segment URLs; the session
    // token still identifies archive playback for Streams/History (Catchup, not Live/HLS).
    let is_archive_playback = archive_reference.is_some()
        || looks_like_archive_media_path(hls_url)
        || session_token.is_some_and(is_m3u_catchup_session_token);
    if is_archive_playback {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.epg_reference_ts = None;
    }
    channel
}

fn hls_entry_user_session_token(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    session_token_hint: Option<&str>,
    archive_reference: Option<i64>,
) -> String {
    if let Some(hint) = session_token_hint.filter(|token| is_m3u_catchup_session_token(token)) {
        return hint.to_string();
    }
    if let Some(timestamp) = archive_reference {
        return create_m3u_catchup_session_key(fingerprint, username, virtual_id, &format!("archive|{timestamp}|0"));
    }
    create_playback_session_fingerprint(fingerprint, username, virtual_id, PlaylistItemType::LiveHls, None)
}

struct HlsAccessManifestRequestContext {
    input: Arc<ConfigInput>,
    hls_url: String,
    session_entry_url: HlsOriginEntryUrl,
    original_hls_entry_path: String,
    origin_source: HlsOriginSource,
    headers: HeaderMap,
    server_path: Option<String>,
}

async fn resolve_hls_playback_manifest_request_context(
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
    if app_state.active_users.is_user_blocked_for_stream(&user.username, access_context.virtual_id).await {
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

async fn hls_manifest_preflight_refresh_ordering(
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

async fn hls_proxy_manifest(
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

#[allow(clippy::too_many_lines)]
async fn hls_api_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let api_proxy_user = create_api_proxy_user(&app_state);
    let (user, target) = if params.username == api_proxy_user.username
        && crate::auth::constant_time_eq(params.password.as_bytes(), api_proxy_user.password.as_bytes())
    {
        let Some(target) = app_state.app_config.get_target_by_id(params.target_id) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        (Arc::new(api_proxy_user), target)
    } else {
        let Some((user, target)) = app_state.app_config.get_target_for_user(&params.username, &params.password) else {
            // Credential failure is an auth error, not a malformed request
            return app_state.app_config.get_auth_error_status().into_response();
        };
        if target.id != params.target_id {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        (user, target)
    };

    // Nested path = relative origin segment that leaked past rewrite_hls (e.g. dvr-YYYY/...).
    if params.token.contains('/') {
        let Some((token, relative_path)) = params.token.split_once('/') else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let encrypt_secret = app_state.get_encrypt_secret();
        let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, token) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let lookup_session_token = decoded_hls_token
            .0
            .clone()
            .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, params.stream_id, false));
        let Some(input) = app_state.app_config.get_input_by_id(params.input_id) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let Some(session) = app_state
            .active_users
            .find_latest_session_for_target_stream(
                &user.username,
                target.id,
                input.name.as_ref(),
                params.stream_id,
                lookup_session_token.as_str(),
            )
            .await
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !legacy_hls_route_allowed_with_cache(
            hls_cache_enabled_for_target(&app_state, &target),
            decoded_hls_token.0.as_deref(),
            Some(session.token.as_str()),
        ) {
            return hls_custom_video_manifest_response_for_username(
                &app_state,
                &user.username,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await;
        }
        return hls_api_stream_leaked_relative(
            fingerprint,
            req_headers,
            app_state,
            user,
            target,
            input,
            params.stream_id,
            session,
            decoded_hls_token.1,
            relative_path.to_string(),
            raw_query.as_deref(),
        )
        .await;
    }

    hls_api_stream_resolved(
        fingerprint,
        req_headers,
        app_state,
        user,
        target,
        params.input_id,
        params.stream_id,
        params.token,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn admit_recovered_archive_stream(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &Arc<ProxyUserCredentials>,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    mut session: UserSession,
    stream_channel: StreamChannel,
) -> Result<(UserSession, StreamChannel, Option<GraceMode>), Box<axum::response::Response>> {
    if session.permission == UserConnectionPermission::Exhausted {
        return Err(Box::new(
            hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                user,
                stream_channel,
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await,
        ));
    }
    if app_state.active_provider.is_over_limit(&session.provider).await {
        return Err(Box::new(
            hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                user,
                stream_channel,
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            )
            .await,
        ));
    }
    let (connection_admission, grace_mode, _) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        user,
        fingerprint,
        PlaylistItemType::Catchup,
        Some(&session),
        &session.token,
        true,
        crate::api::api_utils::EvictionReentryGuard::Session(&session.token),
        false,
        false,
    )
    .await;
    let connection_permission = connection_admission.permission;
    let connection_kind = connection_admission.kind.or(session.connection_kind);
    session.permission = connection_permission;
    if let Some(connection_kind) = connection_kind {
        session.connection_kind = Some(connection_kind);
    }
    if connection_permission == UserConnectionPermission::Exhausted
        || (connection_permission == UserConnectionPermission::GracePeriod && connection_kind.is_none())
    {
        let provider = if session.provider.is_empty() { input.name.clone() } else { session.provider.clone() };
        return Err(Box::new(
            hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                user,
                stream_channel,
                provider,
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await,
        ));
    }
    Ok((session, stream_channel, grace_mode))
}

#[allow(clippy::too_many_arguments)]
async fn hls_api_stream_leaked_relative(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input: Arc<ConfigInput>,
    stream_id: u32,
    mut session: UserSession,
    session_stream_url: String,
    relative_path: String,
    request_query: Option<&str>,
) -> axum::response::Response {
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state.app_config, &app_state.geoip) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let Some(origin_url) = resolve_leaked_hls_relative_origin(&session_stream_url, &relative_path, request_query)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let archive_reference = resolve_m3u_archive_reference(&origin_url, Some(session.token.as_str()))
        .or_else(|| epg_reference_ts_from_date_tree_path(&relative_path))
        .or_else(|| epg_reference_ts_from_date_tree_path(&origin_url));
    let is_archive_media = looks_like_archive_media_path(&relative_path) || looks_like_archive_media_path(&origin_url);
    session.stream_url = origin_url.intern();
    let mut stream_channel = resolve_stream_channel(
        &app_state,
        &target,
        &input,
        stream_id,
        &session.stream_url,
        archive_reference,
        Some(session.token.as_str()),
    )
    .await;
    // Leaked DVR/date-tree segments are always archive playback for the panel, even when the
    // prior session was live and the date-tree timestamp could not be parsed.
    if is_archive_media {
        stream_channel.item_type = PlaylistItemType::Catchup;
        stream_channel.cluster = XtreamCluster::Video;
        if stream_channel.epg_reference_ts.is_none() {
            stream_channel.epg_reference_ts = archive_reference;
        }
    }
    let (session, stream_channel, grace_mode) = match admit_recovered_archive_stream(
        &app_state,
        &fingerprint,
        &user,
        &req_headers,
        &input,
        session,
        stream_channel,
    )
    .await
    {
        Ok(admission) => admission,
        Err(response) => return *response,
    };
    force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        crate::api::api_utils::ForceStreamRequestContext {
            req_headers: &req_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        grace_mode,
    )
    .await
    .into_response()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn hls_api_stream_resolved(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input_id: u16,
    stream_id: u32,
    token: String,
) -> axum::response::Response {
    // Network access check only - permission check is done later with full stream info
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state.app_config, &app_state.geoip) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let target_name = &target.name;
    let virtual_id = stream_id;
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_id(input_id),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}, hls", input_id)
    );

    if user.permission_denied(&app_state.app_config) {
        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, "", None, None).await;
        return hls_admission_failure_manifest_response(
            &app_state,
            &fingerprint,
            &user,
            stream_channel,
            input.name.clone(),
            &req_headers,
            ConnectFailureReason::UserAccountExpired,
        )
        .await;
    }

    debug_if_enabled!("ID chain for hls endpoint: request_stream_id={stream_id} -> virtual_id={virtual_id}");
    let encrypt_secret = app_state.get_encrypt_secret();
    let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, &token) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let lookup_session_token = decoded_hls_token
        .0
        .clone()
        .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, virtual_id, false));
    let mut user_session =
        app_state.active_users.get_and_update_user_session(&user.username, &lookup_session_token).await;
    if !legacy_hls_route_allowed_with_cache(
        hls_cache_enabled_for_target(&app_state, &target),
        decoded_hls_token.0.as_deref(),
        user_session.as_ref().map(|session| session.token.as_str()),
    ) {
        return hls_manifest_channel_unavailable_response_for_username(&app_state, &user.username).await;
    }

    if let Some(session) = &mut user_session {
        let decoded_archive_reference =
            resolve_m3u_archive_reference(&decoded_hls_token.1, Some(lookup_session_token.as_str()));
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.1,
                decoded_archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.1,
                decoded_archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            )
            .await;
        }

        let hls_url = match decoded_hls_token {
            (Some(session_token), hls_url) if session.token.eq(&session_token) => hls_url,
            (None, hls_url) => hls_url,
            _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        let hls_url = hls_url.intern();
        // Recover utc/utcstart from the prior playlist URL before overwriting with a segment URL
        // that usually drops append/shift query params.
        let archive_reference = resolve_m3u_archive_reference(&hls_url, Some(session.token.as_str()))
            .or_else(|| m3u_archive_epg_reference_ts(session.stream_url.as_ref()));
        session.stream_url = hls_url.clone();
        if session.virtual_id == virtual_id {
            app_state.connection_manager.touch_http_activity(&user.username, &session.token, &fingerprint.addr).await;
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &hls_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            if is_seekable_media_request(stream_channel.cluster, &req_headers, extract_extension_from_url(&hls_url)) {
                // partial request means we are in reverse proxy mode, seek happened
                return force_provider_stream_response(
                    &fingerprint,
                    &app_state,
                    session,
                    stream_channel,
                    crate::api::api_utils::ForceStreamRequestContext {
                        req_headers: &req_headers,
                        input: &input,
                        user: &user,
                        session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
                        content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                    },
                    None,
                )
                .await
                .into_response();
            }
        } else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let (connection_admission, grace_mode, request_class) =
            crate::api::api_utils::resolve_playback_request_admission(
                &app_state.admission_ctx(),
                &user,
                &fingerprint,
                if is_m3u_catchup_session_token(&session.token) || archive_reference.is_some() {
                    PlaylistItemType::Catchup
                } else {
                    PlaylistItemType::LiveHls
                },
                Some(session),
                &session.token,
                true,
                crate::api::api_utils::EvictionReentryGuard::Session(&session.token),
                // HLS playlist requests (.m3u8) are explicit Prepare: they set up session metadata
                // but do not consume an admission slot. Segment and other media requests use Activate.
                is_hls_url(&hls_url),
                false,
            )
            .await;
        let connection_permission = connection_admission.permission;
        let connection_kind = connection_admission.kind.or(session.connection_kind);
        session.permission = connection_permission;
        if let Some(connection_kind) = connection_kind {
            session.connection_kind = Some(connection_kind);
        }
        if connection_permission == UserConnectionPermission::Exhausted
            || (connection_permission == UserConnectionPermission::GracePeriod && connection_kind.is_none())
        {
            let provider = if session.provider.is_empty() { input.name.clone() } else { session.provider.clone() };
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &session.stream_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                provider,
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }
        let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);

        if is_hls_url(&session.stream_url) {
            let source = match resolve_hls_virtual_source_for_target(&app_state, &target, virtual_id).await {
                Ok(source) if source.input.id == input.id => source,
                Ok(source) => {
                    warn!(
                        "HLS input context mismatch for virtual_id={virtual_id}: expected_input_id={}, resolved_input_id={}",
                        input.id, source.input.id
                    );
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(status) => return status.into_response(),
            };
            let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, virtual_id);
            return handle_hls_stream_request(
                &fingerprint,
                &app_state,
                &user,
                &target,
                Some(session),
                None,
                &session.stream_url,
                archive_reference,
                source.stream_context,
                &input,
                &req_headers,
                connection_permission,
                connection_kind,
                &original_hls_entry_path,
            )
            .await
            .into_response();
        }

        if is_file_url(&session.stream_url) {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &hls_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return local_stream_response(
                &fingerprint,
                &app_state,
                stream_channel,
                &req_headers,
                &input,
                &target,
                &user,
                connection_permission,
                fallback_connection_kind,
                Some(&session.token),
                Some(request_class),
                false,
            )
            .await
            .into_response();
        }

        let stream_channel = resolve_stream_channel(
            &app_state,
            &target,
            &input,
            virtual_id,
            &hls_url,
            archive_reference,
            Some(session.token.as_str()),
        )
        .await;
        force_provider_stream_response(
            &fingerprint,
            &app_state,
            session,
            stream_channel,
            crate::api::api_utils::ForceStreamRequestContext {
                req_headers: &req_headers,
                input: &input,
                user: &user,
                session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            grace_mode,
        )
        .await
        .into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

pub fn hls_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/manifest.m3u8",
            axum::routing::get(hls_proxy_manifest),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/terminal/{generation}/{terminal_file}",
            axum::routing::get(hls_proxy_terminal_segment),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/{segment_file}",
            axum::routing::get(hls_proxy_segment),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/map/{map_file}",
            axum::routing::get(hls_proxy_map),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/r/{resource_file}",
            axum::routing::get(hls_proxy_resource),
        )
        .route(
            "/hls/{username}/{password}/{target_id}/{input_id}/{stream_id}/{*token}",
            axum::routing::get(hls_api_stream),
        )
    //cfg.service(web::resource("/hls/{token}/{stream}").route(web::get().to(xtream_player_api_hls_stream)));
    //cfg.service(web::resource("/play/{token}/{type}").route(web::get().to(xtream_player_api_play_stream)));
}

#[cfg(test)]
mod tests {
    use super::{
        super::hls_terminal_response::{
            hls_manifest_terminal_preflight, hls_terminal_endpoint_action, hls_terminal_failed_closed_response,
            HlsManifestTerminalPreflight, HlsTerminalEndpointAction,
        },
        build_hls_manifest_request_headers, extract_hls_provider_session_headers, hls_api_register,
        hls_availability_reevaluation_registration_failure_response, hls_canonical_owner_registration,
        hls_temporary_resource_unavailable_response, m3u_archive_epg_reference_ts,
        m3u_catchup_epg_reference_from_session_token, resolve_leaked_hls_relative_origin,
        HlsCanonicalOwnerRegistration, HlsCanonicalOwnerRegistrationFailure, HlsCanonicalOwnerRegistrationKind,
        MAX_HLS_MANIFEST_BYTES,
    };
    use crate::{
        api::model::{
            begin_hls_origin_account_io, build_hls_custom_video_manifest_body, build_proxy_session_id,
            build_terminal_tail_plan, build_transient_resource_id,
            commit_terminal_tail_if_lease_reserve_requires_cutover, finish_hls_origin_account_io,
            prepare_terminal_base_evidence, prepared_terminal_bundle_key, snapshot_terminal_media_asset,
            trigger_origin_refresh_sync, ActiveProviderManager, ActiveUserManager, AppState, CacheAccessState,
            CancelTokens, ConnectionKind, ConnectionManager, CreateUserSessionParams, CustomVideoStreamType,
            EventManager, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsAccessAdmissionMode,
            HlsAccessContext, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsAccessLeaseTiming,
            HlsAccessLeaseTouch, HlsAccessLeaseValidationError, HlsAvailabilityReevaluationFinishReason,
            HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationRegistration, HlsBandwidthPersistenceState,
            HlsEffectiveOriginAcquirePolicy, HlsFreshManifestRequiredReason, HlsLeaseManifestSegment,
            HlsLeaseManifestSnapshot, HlsLeasePlaybackMode, HlsLifecycleEvent, HlsLifecycleEventKey,
            HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome,
            HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceTrigger, HlsManifestCommitRequirement,
            HlsManifestDeliveryMode, HlsManifestSourceRenderMarker, HlsMapSignature, HlsMediaContainer,
            HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsOriginAccountBinding, HlsOriginAccountBindingMode,
            HlsOriginAccountDetachedReason, HlsOriginIoContext, HlsOriginPathCondition, HlsOriginSource,
            HlsOriginSourceKind, HlsPlaybackFamilyKey, HlsPreparedTerminalBundleState, HlsProxyManager,
            HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload, HlsRuntimeCustomTailAssetIdentity,
            HlsRuntimeCustomTailReason, HlsSegmentFile, HlsSession, HlsSessionHandle, HlsSessionKey, HlsSessionMode,
            HlsSessionStoreOutcome, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState, HlsTerminalBaseProtection,
            HlsTerminalBaseSegmentAvailability, HlsTerminalFailedClosedReason, HlsTerminalMediaAsset,
            HlsTerminalMediaPreparationState, HlsTerminalResolution, HlsTerminalSegmentPath, HlsTerminalTailBuildInput,
            HlsTerminalTailCompatibility, HlsTerminalTailGeneration, HlsTerminalTailPlan, HlsTerminalTailProtection,
            HlsTransitionMarginMs, LiveHlsOriginEntry, ManualPlaylistUpdateRequest, MapCacheStatus, MapEntry,
            MetadataUpdateManager, OriginMapKey, OriginRefreshRequest, OriginSegmentFetchRef, OriginSegmentKey,
            PlaybackLifecycle, PlaylistStorageState, ProviderConfig as RuntimeProviderConfig, ProviderConfigConnection,
            ProxyMapId, ProxySessionId, RenderedManifest, RetryPolicy, SegmentCacheKey, SegmentCacheStatus,
            SegmentEntry, SegmentFetchPriority, SharedStreamManager, TransientObjectCacheKey,
            TransientObjectCacheStatus, TransientResourceId, TransientResourceKind, TransientResourceRef,
            TransportStreamBuffer, UpdateGuard, UserSession, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        },
        auth::Fingerprint,
        model::{
            ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, ConfigProvider, ConfigSource,
            ConfigTarget, CustomStreamResponse, HlsCacheConfig, ProcessTargets, ProxyUserCredentials,
            ReverseProxyConfig, ReverseProxyDisabledHeaderConfig, SourcesConfig, StripConfig, TargetUser,
        },
        processing::parser::hls::{
            origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
            rewrite_hls, RewriteHlsProps,
        },
        repository::GeoIp,
    };
    use aes::{
        cipher::{Block, BlockEncrypt, KeyInit},
        Aes128,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode},
        response::IntoResponse,
    };
    use http_body_util::BodyExt;
    use shared::{
        model::{
            provider_saturation::build_group_lookup, ConfigPaths, ConfigProviderDto, ConfigTargetDto,
            ConfigTargetOptions, ConfigTargetShareLiveStreams, HlsCacheConfigDto, HlsManifestRecoveryBurstConfigDto,
            HlsManifestRecoveryBurstLevel, HlsSegmentRepairMode, HlsStripConfigDto, HlsStripMode, InputType,
            M3uPlaylistItem, M3uTargetOutputDto, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
            ProviderUrlSelectionPolicy, ReverseProxyConfigDto, StreamConfigDto, StreamProperties, TargetOutputDto,
            UserConnectionPermission, XtreamCluster, XtreamTargetOutputDto,
        },
        utils::Internable,
    };
    use std::{
        collections::HashMap,
        fmt::Write as _,
        net::SocketAddr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{mpsc, RwLock},
    };
    use tower::ServiceExt;

    fn path_has_extension(path: &str, extension: &str) -> bool {
        std::path::Path::new(path).extension().is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
    }

    #[test]
    fn archive_epg_reference_supports_query_and_path_formats() {
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?utc=1700000000&lutc=1700003600"),
            Some(1_700_000_000)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/archive-1700003600-1700007200.m3u8"),
            Some(1_700_003_600)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/timeshift_abs-1700007200.ts"),
            Some(1_700_007_200)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000&end=1700003600"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn archive_epg_reference_rejects_plain_start_queries() {
        assert_eq!(m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000"), None);
    }

    #[test]
    fn date_tree_path_recovers_bittv_archive_epg_reference() {
        assert_eq!(super::epg_reference_ts_from_date_tree_path("2026/07/24/14/13/38-06800.ts"), Some(1_784_902_418));
        assert_eq!(
            super::epg_reference_ts_from_date_tree_path("dvr-2026/07/24/14/13/38-06800.ts"),
            super::epg_reference_ts_from_date_tree_path("2026/07/24/14/13/38-06800.ts")
        );
        assert!(super::looks_like_archive_media_path("2026/07/24/14/13/38-06800.ts"));
    }

    #[test]
    fn archive_media_path_does_not_accept_plain_202_prefixed_segments() {
        assert!(!super::looks_like_archive_media_path("2026.ts"));
        assert!(!super::looks_like_archive_media_path("202_media.ts"));
    }

    #[test]
    fn session_token_recovers_archive_epg_reference_when_media_url_lost_markers() {
        assert_eq!(
            m3u_catchup_epg_reference_from_session_token("m3u-catchup|user|42|archive|1717200000|3600"),
            Some(1_717_200_000)
        );
        assert_eq!(m3u_catchup_epg_reference_from_session_token("m3u-catchup|user|42|live"), None);
    }

    #[test]
    fn append_catchup_session_hint_keeps_m3u_catchup_token_without_shared_hls_cache() {
        let fingerprint = test_fingerprint();
        let hint = "m3u-catchup|fp|alice|42|deadbeef";
        let token = super::hls_entry_user_session_token(&fingerprint, "alice", 42, Some(hint), Some(1_717_200_000));
        assert_eq!(token, hint);
        assert!(super::is_m3u_catchup_session_token(&token));

        let from_archive = super::hls_entry_user_session_token(&fingerprint, "alice", 42, None, Some(1_717_200_000));
        assert!(from_archive.contains("|archive|1717200000|0"));
        assert!(super::is_m3u_catchup_session_token(&from_archive));
        assert!(super::is_m3u_catchup_session_token("m3u-catchup|fp|alice|42|timeshift_abs|1717200000|0"));
    }

    #[test]
    fn leaked_dvr_relative_joins_against_media_playlist_and_dvr_session_root() {
        assert_eq!(
            resolve_leaked_hls_relative_origin(
                "http://cdn.example/big/aa_1/media.m3u8",
                "dvr-2026/07/26/15/30/59-06000.ts",
                Some("token=abc"),
            ),
            Some("http://cdn.example/big/aa_1/dvr-2026/07/26/15/30/59-06000.ts?token=abc".to_string())
        );
        assert_eq!(
            resolve_leaked_hls_relative_origin(
                "http://cdn.example/big/aa_1/dvr-2026/07/26/15/30/59-06000.ts?token=old",
                "dvr-2026/07/26/15/31/05-06000.ts",
                Some("token=new"),
            ),
            Some("http://cdn.example/big/aa_1/dvr-2026/07/26/15/31/05-06000.ts?token=new".to_string())
        );
        assert_eq!(
            resolve_leaked_hls_relative_origin("http://cdn.example/big/aa_1/media.m3u8", "segment001.ts", None,),
            None
        );
    }

    #[test]
    fn archive_epg_reference_supports_contextual_start_aliases() {
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?offset=-3600&utcstart=1717200000"),
            Some(1_717_200_000)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?timestamp=1717200000&offset=120"),
            Some(1_717_200_000)
        );
    }

    #[test]
    fn canonical_manifest_joins_authoritative_owner_and_fails_closed_without_one() {
        assert_eq!(
            hls_canonical_owner_registration(HlsAvailabilityReevaluationRegistration::Scheduled),
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::Scheduled)
        );
        assert_eq!(
            hls_canonical_owner_registration(HlsAvailabilityReevaluationRegistration::AlreadyOwned),
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::AlreadyOwned)
        );
        assert_eq!(
            hls_canonical_owner_registration(HlsAvailabilityReevaluationRegistration::Superseded),
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::AlreadyOwned)
        );
        for (registration, failure) in [
            (
                HlsAvailabilityReevaluationRegistration::CapacityExceeded,
                HlsCanonicalOwnerRegistrationFailure::CapacityExceeded,
            ),
            (
                HlsAvailabilityReevaluationRegistration::RuntimeUnavailable,
                HlsCanonicalOwnerRegistrationFailure::RuntimeUnavailable,
            ),
        ] {
            assert_eq!(
                hls_canonical_owner_registration(registration),
                HlsCanonicalOwnerRegistration::FailClosed(failure)
            );
            let response = hls_availability_reevaluation_registration_failure_response(failure);
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(response.headers().contains_key(header::RETRY_AFTER));
        }
    }

    struct CanonicalOwnerHandoffFixture {
        app_state: Arc<AppState>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        leases: Vec<(HlsAccessLeaseId, u64)>,
        strip: StripConfig,
    }

    impl CanonicalOwnerHandoffFixture {
        async fn new(lease_ids: &[&str]) -> Self {
            let app_state = test_app_state();
            enable_hls_cache(&app_state);
            let session = app_state
                .hls_proxy
                .get_or_create_session(HlsSessionKey::new(1, "owner-handoff"), &app_state.get_encrypt_secret(), 100)
                .await;
            let proxy_session_id = session.read().await.proxy_session_id.clone();
            let issued_at_ms = super::current_time_millis();
            let mut leases = Vec::with_capacity(lease_ids.len());
            for lease_id in lease_ids {
                let lease_id = HlsAccessLeaseId((*lease_id).to_string());
                app_state
                    .hls_proxy
                    .prepare_access_lease(HlsAccessLease::pending(
                        lease_id.clone(),
                        HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                        proxy_session_id.clone(),
                        "hls-user".to_string(),
                        format!("{}-session", lease_id.0),
                        1,
                        "owner-handoff".to_string(),
                        12345,
                        issued_at_ms,
                        60_000,
                    ))
                    .await;
                leases.push((lease_id, issued_at_ms));
            }
            Self {
                app_state,
                session,
                proxy_session_id,
                leases,
                strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
            }
        }

        async fn safe_session(&self) -> String {
            let session = self.session.read().await;
            super::safe_session_key(&session.key)
        }

        fn handoff_context(
            &self,
            lease_index: usize,
            safe_session: String,
            request_deadline_ms: u64,
        ) -> super::HlsCanonicalOwnerHandoffContext<'_> {
            let (lease_id, issued_at_ms) = &self.leases[lease_index];
            super::HlsCanonicalOwnerHandoffContext {
                app_state: &self.app_state,
                proxy_session_id: &self.proxy_session_id,
                access_lease_id: lease_id,
                expected_lease_issued_at_ms: Some(*issued_at_ms),
                strip: &self.strip,
                server_path: None,
                manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
                manifest_boundary_rendered_at_ms: 0,
                bandwidth_learning: super::HlsRuntimeBandwidthLearningContext::Disabled,
                request_deadline_ms,
                safe_session,
            }
        }
    }

    async fn publish_owner_handoff_test_manifest(session: &HlsSessionHandle) {
        let now_ms = super::current_time_millis();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
        );
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("owner handoff manifest maps");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1_000, ready_at_ms: now_ms };
        }
        session.advance_media_readiness_generation();
        session.render_and_store_manifest(now_ms).expect("owner handoff manifest renders");
        session.mark_authorized_media_access(now_ms);
    }

    #[tokio::test]
    async fn scheduled_owner_does_not_return_transient_503() {
        let fixture = CanonicalOwnerHandoffFixture::new(&["scheduled-lease"]).await;
        let owner_key = fixture
            .app_state
            .hls_proxy
            .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
            .await
            .expect("owner key");
        let coordinator = fixture.app_state.hls_proxy.availability_reevaluations();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let task_started = Arc::clone(&started);
        let task_release = Arc::clone(&release);
        let task_completed = Arc::clone(&completed);
        let task_session = Arc::clone(&fixture.session);
        let task_app_state = Arc::clone(&fixture.app_state);
        let task_proxy_session_id = fixture.proxy_session_id.clone();
        let task_owner_key = owner_key.clone();
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    task_started.notify_one();
                    task_release.notified().await;
                    publish_owner_handoff_test_manifest(&task_session).await;
                    task_app_state.hls_proxy.notify_session_evidence_changed(&task_proxy_session_id);
                    let _ = ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                    task_completed.notify_one();
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        started.notified().await;
        let deadline_ms = super::current_time_millis().saturating_add(60_000);
        let safe_session = fixture.safe_session().await;
        let mut response = Box::pin(super::join_hls_canonical_manifest_owner(
            fixture.handoff_context(0, safe_session, deadline_ms),
            HlsCanonicalOwnerRegistrationKind::Scheduled,
        ));

        assert!(matches!(futures::poll!(response.as_mut()), std::task::Poll::Pending));
        release.notify_one();

        let response = response.await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("live manifest utf8");
        assert!(body.contains("/scheduled-lease/"));
        completed.notified().await;
    }

    #[tokio::test]
    async fn already_owned_work_is_joined_without_duplicate_refresh() {
        let fixture = CanonicalOwnerHandoffFixture::new(&["first-lease", "second-lease"]).await;
        let owner_key = fixture
            .app_state
            .hls_proxy
            .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
            .await
            .expect("owner key");
        let coordinator = fixture.app_state.hls_proxy.availability_reevaluations();
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let task_release = Arc::clone(&release);
        let task_completed = Arc::clone(&completed);
        let task_session = Arc::clone(&fixture.session);
        let task_app_state = Arc::clone(&fixture.app_state);
        let task_proxy_session_id = fixture.proxy_session_id.clone();
        let task_owner_key = owner_key.clone();
        assert_eq!(
            coordinator.register(
                owner_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    task_release.notified().await;
                    publish_owner_handoff_test_manifest(&task_session).await;
                    task_app_state.hls_proxy.notify_session_evidence_changed(&task_proxy_session_id);
                    let _ = ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                    task_completed.notify_one();
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        let duplicate_owner_runs = Arc::new(AtomicUsize::new(0));
        let task_duplicate_owner_runs = Arc::clone(&duplicate_owner_runs);
        assert_eq!(
            coordinator.register(owner_key, HlsAvailabilityReevaluationMode::RecoveryPressure, move |_| async move {
                task_duplicate_owner_runs.fetch_add(1, Ordering::SeqCst);
            },),
            HlsAvailabilityReevaluationRegistration::AlreadyOwned
        );
        let deadline_ms = super::current_time_millis().saturating_add(60_000);
        let mut first = Box::pin(super::join_hls_canonical_manifest_owner(
            fixture.handoff_context(0, fixture.safe_session().await, deadline_ms),
            HlsCanonicalOwnerRegistrationKind::AlreadyOwned,
        ));
        let mut second = Box::pin(super::join_hls_canonical_manifest_owner(
            fixture.handoff_context(1, fixture.safe_session().await, deadline_ms),
            HlsCanonicalOwnerRegistrationKind::AlreadyOwned,
        ));
        assert!(matches!(futures::poll!(first.as_mut()), std::task::Poll::Pending));
        assert!(matches!(futures::poll!(second.as_mut()), std::task::Poll::Pending));

        release.notify_one();
        let (first, second) = tokio::join!(first, second);

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        let first_body = String::from_utf8(response_body(first).await.to_vec()).expect("first manifest utf8");
        let second_body = String::from_utf8(response_body(second).await.to_vec()).expect("second manifest utf8");
        assert!(first_body.contains("/first-lease/"));
        assert!(!first_body.contains("/second-lease/"));
        assert!(second_body.contains("/second-lease/"));
        assert!(!second_body.contains("/first-lease/"));
        assert_eq!(duplicate_owner_runs.load(Ordering::SeqCst), 0);
        completed.notified().await;
        tokio::task::yield_now().await;
        assert_eq!(coordinator.owner_count(), 0);
    }

    #[tokio::test]
    async fn canonical_owner_join_preserves_bounded_deadline_failure() {
        let fixture = CanonicalOwnerHandoffFixture::new(&["deadline-lease"]).await;
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &fixture.leases[0].0,
                &fixture.proxy_session_id,
                super::current_time_millis(),
            )
            .await
            .expect("pending deadline lease");
        assert_eq!(
            super::hls_canonical_owner_request_deadline_ms(&lease, Duration::ZERO, super::current_time_millis(),),
            lease.pending_deadline_ms().expect("pending lease deadline")
        );
        let owner_key = fixture
            .app_state
            .hls_proxy
            .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
            .await
            .expect("owner key");
        let coordinator = fixture.app_state.hls_proxy.availability_reevaluations();
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                |ownership| async move {
                    ownership.cancelled().await;
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        let response = super::join_hls_canonical_manifest_owner(
            fixture.handoff_context(0, fixture.safe_session().await, super::current_time_millis().saturating_sub(1)),
            HlsCanonicalOwnerRegistrationKind::Scheduled,
        )
        .await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        coordinator.cancel_session(&fixture.proxy_session_id);
    }

    async fn terminal_generation_for_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
    ) -> HlsTerminalTailGeneration {
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(lease_id, proxy_session_id, super::current_time_millis())
            .await
            .expect("terminal lease remains stored");
        let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode else {
            panic!("lease remains terminal");
        };
        plan.generation
    }

    #[tokio::test]
    async fn completed_owner_resolves_new_lease_standalone_without_reusing_old_terminal() {
        let temp_dir = tempfile::tempdir().expect("owner handoff cache tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        enable_channel_unavailable_custom_response(&app_state);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"old-terminal-base").await;
        let old_lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &old_lease_id.0, 123).await;
        let proxy_session_id = ProxySessionId(proxy_session_id);
        let now_ms = super::current_time_millis();
        let old_generation = terminal_generation_for_lease(&app_state, &proxy_session_id, &old_lease_id).await;
        let new_lease_id = HlsAccessLeaseId("new-owner-handoff-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                new_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "new-owner-handoff-session".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                60_000,
            ))
            .await;
        let session =
            app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.expect("shared session");
        let owner_key = app_state
            .hls_proxy
            .availability_reevaluation_owner_key(&session, &proxy_session_id)
            .await
            .expect("owner key");
        let release = Arc::new(tokio::sync::Notify::new());
        let task_release = Arc::clone(&release);
        let task_owner_key = owner_key.clone();
        let coordinator = app_state.hls_proxy.availability_reevaluations();
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    task_release.notified().await;
                    let _ = ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        let safe_session = {
            let session = session.read().await;
            super::safe_session_key(&session.key)
        };
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
        let mut response = Box::pin(super::join_hls_canonical_manifest_owner(
            super::HlsCanonicalOwnerHandoffContext {
                app_state: &app_state,
                proxy_session_id: &proxy_session_id,
                access_lease_id: &new_lease_id,
                expected_lease_issued_at_ms: Some(now_ms),
                strip: &strip,
                server_path: None,
                manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
                manifest_boundary_rendered_at_ms: 0,
                bandwidth_learning: super::HlsRuntimeBandwidthLearningContext::Disabled,
                request_deadline_ms: now_ms.saturating_add(60_000),
                safe_session,
            },
            HlsCanonicalOwnerRegistrationKind::Scheduled,
        ));
        assert!(matches!(futures::poll!(response.as_mut()), std::task::Poll::Pending));

        release.notify_one();
        let response = response.await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("standalone manifest utf8");
        assert!(body.contains("#EXT-X-ENDLIST"));
        assert!(!body.contains("/hls/shared/live/"));
        assert_eq!(
            terminal_generation_for_lease(&app_state, &proxy_session_id, &old_lease_id).await,
            old_generation,
            "new lease fallback cannot reactivate old terminal lease"
        );
    }

    #[test]
    fn hls_cache_session_tokens_separate_live_and_archive_playback() {
        let fingerprint = test_fingerprint();
        let live = super::create_hls_cache_user_session_token(&fingerprint, "user", 31, None, None);
        let archive = super::create_hls_cache_user_session_token(&fingerprint, "user", 31, None, Some(1_784_898_000));

        assert!(!super::is_m3u_catchup_session_token(&live));
        assert!(super::is_m3u_catchup_session_token(&archive));
        assert_ne!(live, archive);
    }

    #[test]
    fn hls_cache_session_token_preserves_existing_m3u_catchup_identity() {
        let fingerprint = test_fingerprint();
        let existing = "m3u-catchup|fp|user|31|archive|1784898000|3600";
        let token =
            super::create_hls_cache_user_session_token(&fingerprint, "user", 31, Some(existing), Some(1_784_898_000));

        assert!(token.starts_with(existing));
        assert!(token.contains("|hls-cache|"));
    }

    #[test]
    fn hls_terminal_commit_endpoint_resolution_mapping_is_exhaustive() {
        assert_eq!(
            hls_terminal_endpoint_action(HlsTerminalResolution::LiveAllowed),
            HlsTerminalEndpointAction::ServeLive
        );
        assert_eq!(
            hls_terminal_endpoint_action(HlsTerminalResolution::Committed),
            HlsTerminalEndpointAction::ReloadTerminal
        );
        assert_eq!(
            hls_terminal_endpoint_action(HlsTerminalResolution::Reevaluate),
            HlsTerminalEndpointAction::Reevaluate
        );
        assert_eq!(
            hls_terminal_endpoint_action(HlsTerminalResolution::Pending { retry_after_ms: 250 }),
            HlsTerminalEndpointAction::RetryAfter { retry_after_ms: 250 }
        );

        for reason in [
            HlsTerminalFailedClosedReason::LeaseStateUnavailable,
            HlsTerminalFailedClosedReason::BundleNotReadyWithoutOwner,
            HlsTerminalFailedClosedReason::BundleIncompatible,
            HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed,
            HlsTerminalFailedClosedReason::RetryCapacityExceeded,
            HlsTerminalFailedClosedReason::RetryAttemptsExhausted,
            HlsTerminalFailedClosedReason::RuntimeUnavailable,
        ] {
            assert_eq!(
                hls_terminal_endpoint_action(HlsTerminalResolution::FailedClosed { reason }),
                HlsTerminalEndpointAction::FailClosed { reason }
            );
        }
    }

    #[tokio::test]
    async fn hls_manifest_terminal_preflight_distinguishes_bootstrap_refresh_and_invalid_missing_snapshot() {
        let app_state = test_app_state();
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "preflight-stream"), &app_state.get_encrypt_secret(), 1_000)
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let mut lease = HlsAccessLease::pending(
            HlsAccessLeaseId("preflight-lease".to_string()),
            HlsPlaybackFamilyKey::new("hls-user", "preflight-client"),
            proxy_session_id,
            "hls-user".to_string(),
            "preflight-user-session".to_string(),
            1,
            "preflight-stream".to_string(),
            12345,
            1_000,
            120_000,
        );

        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 2_000).await,
            HlsManifestTerminalPreflight::BootstrapPendingLease
        );

        lease.state = HlsAccessLeaseState::Activated;
        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 2_000).await,
            HlsManifestTerminalPreflight::FailClosed { reason: HlsTerminalFailedClosedReason::LeaseStateUnavailable }
        );

        lease.last_manifest_snapshot = Some(HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(1_500),
            snapshot_generation: 1,
            delivered_at_ms: 1_500,
            first_proxy_seq: 0,
            last_proxy_seq: 0,
            visible_segments: Arc::from([]),
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            playlist_duration_ms: 0,
            last_visible_media_end_ms: 0,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        });
        {
            let mut session = session.write().await;
            session.origin_control.target_duration_snapshot_ms = Some(4_000);
            session.origin_control.last_media_progress_at_ms = Some(2_000);
        }
        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 8_000).await,
            HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation
        );
        session.write().await.origin_control.last_media_progress_at_ms = Some(7_999);
        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 8_000).await,
            HlsManifestTerminalPreflight::EvaluateTerminal
        );

        lease.playback_mode = HlsLeasePlaybackMode::Ended;
        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 8_000).await,
            HlsManifestTerminalPreflight::ServeCommittedPlayback
        );
    }

    #[tokio::test]
    async fn hls_manifest_terminal_preflight_keeps_capacity_recovery_out_of_sync_terminal_wait() {
        let app_state = test_app_state();
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "capacity-preflight"), &app_state.get_encrypt_secret(), 1_000)
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        {
            let mut session = session.write().await;
            let OriginManifestParseOutcome::Normal(manifest) = parse_origin_media_manifest(
                "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n\
                 #EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
                "http://origin.example/live/index.m3u8",
            ) else {
                panic!("capacity preflight manifest parses");
            };
            session.apply_origin_manifest(&manifest).expect("capacity preflight timeline applies");
            for segment in session.segments.values_mut() {
                segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1_000 };
            }
            session.segments.get_mut(&1).expect("deferred segment").status = SegmentCacheStatus::CapacityDeferred {
                priority: SegmentFetchPriority::Prefetch,
                deferred_at_ms: 2_000,
            };
            session.origin_control.target_duration_snapshot_ms = Some(8_000);
            session.origin_control.last_media_progress_at_ms = Some(1_000);
        }
        let mut lease = HlsAccessLease::pending(
            HlsAccessLeaseId("capacity-preflight-lease".to_string()),
            HlsPlaybackFamilyKey::new("capacity-user", "capacity-client"),
            proxy_session_id,
            "capacity-user".to_string(),
            "capacity-user-session".to_string(),
            1,
            "capacity-preflight".to_string(),
            1,
            1_000,
            60_000,
        );
        lease.state = HlsAccessLeaseState::Activated;
        lease.last_manifest_snapshot = Some(HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(1),
            snapshot_generation: 1,
            delivered_at_ms: 1_000,
            first_proxy_seq: 0,
            last_proxy_seq: 0,
            visible_segments: Arc::from([HlsLeaseManifestSegment {
                proxy_seq: 0,
                duration_ms: 4_000,
                uri: "000000.ts".to_string(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            }]),
            discontinuity_sequence: 0,
            target_duration_ms: 8_000,
            playlist_duration_ms: 4_000,
            last_visible_media_end_ms: 4_000,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        });

        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 20_000).await,
            HlsManifestTerminalPreflight::EvaluateTerminal,
        );

        session.write().await.segments.get_mut(&1).expect("recovered segment").status =
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20_001 };
        assert_eq!(
            hls_manifest_terminal_preflight(&session, &lease, 20_001).await,
            HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation,
        );
    }

    #[test]
    fn hls_terminal_commit_endpoint_pending_and_failed_closed_headers_are_distinct() {
        let pending = hls_temporary_resource_unavailable_response(250);
        assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(pending.headers().get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()), Some("1"));
        assert!(pending.headers().get(header::LOCATION).is_none());

        let failed_closed = hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::RetryAttemptsExhausted);
        assert_eq!(failed_closed.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(failed_closed.headers().get(header::RETRY_AFTER).is_none());
        assert!(failed_closed.headers().get(header::LOCATION).is_none());
    }

    #[test]
    fn extract_hls_provider_session_headers_converts_set_cookie_to_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", "sid=abc; Path=/; HttpOnly".parse().expect("valid cookie"));
        headers.append("set-cookie", "pref=1; Secure".parse().expect("valid cookie"));

        let session_headers = extract_hls_provider_session_headers(&headers);

        assert_eq!(session_headers.get("cookie").map(String::as_str), Some("sid=abc; pref=1"));
    }

    fn test_app_config() -> Arc<AppConfig> {
        let mut hls_user = ProxyUserCredentials::default();
        hls_user.username = "hls-user".to_string();
        hls_user.password = "hls-pass".to_string();
        hls_user.max_connections = 1;
        let api_proxy = ApiProxyConfig {
            server: vec![ApiProxyServerInfo {
                name: "default".to_string(),
                protocol: "https".to_string(),
                host: "example.test".to_string(),
                port: None,
                timezone: "UTC".to_string(),
                message: String::new(),
                path: Some("iptv".to_string()),
            }],
            user: vec![TargetUser { target: "default".to_string(), credentials: vec![Arc::new(hls_user)] }],
            ..Default::default()
        };
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                custom_stream_response_enabled: true,
                ..Default::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::from(Some(Arc::new(api_proxy)))),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        })
    }

    fn hls_custom_video_test_user() -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = "viewer".to_string();
        user.password = "secret".to_string();
        user
    }

    fn test_hls_share_target(hls_enabled: bool) -> ConfigTarget {
        ConfigTarget::from(&ConfigTargetDto {
            id: 1,
            name: "default".to_string(),
            options: Some(ConfigTargetOptions {
                share_live_streams: ConfigTargetShareLiveStreams { hls: hls_enabled, mpeg_ts: false },
                ..Default::default()
            }),
            output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto::default())],
            ..Default::default()
        })
    }

    fn test_m3u_hls_share_target() -> ConfigTarget {
        ConfigTarget::from(&ConfigTargetDto {
            id: 2,
            name: "m3u-target".to_string(),
            options: Some(ConfigTargetOptions {
                share_live_streams: ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
                ..Default::default()
            }),
            output: vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())],
            use_memory_cache: true,
            ..Default::default()
        })
    }

    fn test_m3u_hls_item(input: &ConfigInput, virtual_id: u32, input_stream_id: &str, url: &str) -> M3uPlaylistItem {
        M3uPlaylistItem::from(&PlaylistItem {
            header: PlaylistItemHeader {
                id: input_stream_id.intern(),
                virtual_id,
                input_name: Arc::clone(&input.name),
                url: url.intern(),
                item_type: PlaylistItemType::LiveHls,
                xtream_cluster: XtreamCluster::Live,
                input_stream_id: input_stream_id.intern(),
                ..PlaylistItemHeader::default()
            },
        })
    }

    fn test_hls_entry_stream_context(
        virtual_id: u32,
        input_stream_id: &str,
        known_bitrate_bps: Option<u32>,
    ) -> super::HlsEntryStreamContext {
        super::HlsEntryStreamContext {
            identity: super::HlsEntryStreamIdentity::new(virtual_id, input_stream_id)
                .expect("valid input stream identity"),
            known_bitrate_bps,
        }
    }

    async fn cache_test_m3u_hls_item(app_state: &Arc<AppState>, target: &ConfigTarget, item: M3uPlaylistItem) {
        let mut playlist = crate::repository::BPlusTree::new();
        playlist.insert(item.virtual_id, item);
        app_state
            .playlists
            .cache_playlist(&target.name, crate::api::model::PlaylistStorage::M3uPlaylist(Box::new(playlist)))
            .await;
    }

    fn test_hls_input() -> ConfigInput {
        ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: "http://origin.example.com".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        }
    }

    #[derive(Clone, Copy)]
    enum TestLiveBitrateRepositoryState {
        MissingDatabase,
        MissingStreamItem,
        ExistingHigher,
        Update,
        PermanentlyInapplicable,
        RepositoryIoError,
    }

    impl TestLiveBitrateRepositoryState {
        const fn input_name(self) -> &'static str {
            match self {
                Self::MissingDatabase => "bandwidth-missing-database",
                Self::MissingStreamItem => "bandwidth-missing-item",
                Self::ExistingHigher => "bandwidth-existing-higher",
                Self::Update => "bandwidth-update",
                Self::PermanentlyInapplicable => "bandwidth-inapplicable",
                Self::RepositoryIoError => "bandwidth-io-error",
            }
        }
    }

    fn prepare_test_live_bitrate_repository(
        input: &ConfigInput,
        storage_root: &std::path::Path,
        repository_state: TestLiveBitrateRepositoryState,
    ) {
        let storage_path =
            crate::repository::build_input_storage_path(&input.name, storage_root.to_string_lossy().as_ref());
        let database_path = crate::repository::get_input_m3u_playlist_file_path(&storage_path, &input.name);
        match repository_state {
            TestLiveBitrateRepositoryState::MissingDatabase
            | TestLiveBitrateRepositoryState::PermanentlyInapplicable => {}
            TestLiveBitrateRepositoryState::RepositoryIoError => {
                std::fs::create_dir_all(&storage_path).expect("input storage");
                std::fs::write(&database_path, b"invalid btree data").expect("corrupt input tree");
            }
            TestLiveBitrateRepositoryState::MissingStreamItem
            | TestLiveBitrateRepositoryState::ExistingHigher
            | TestLiveBitrateRepositoryState::Update => {
                std::fs::create_dir_all(&storage_path).expect("input storage");
                let (stream_ref, stored_bitrate) = match repository_state {
                    TestLiveBitrateRepositoryState::MissingStreamItem => ("different-channel", 0),
                    TestLiveBitrateRepositoryState::ExistingHigher => ("channel-a", 3_000_000),
                    TestLiveBitrateRepositoryState::Update => ("channel-a", 0),
                    TestLiveBitrateRepositoryState::MissingDatabase
                    | TestLiveBitrateRepositoryState::PermanentlyInapplicable
                    | TestLiveBitrateRepositoryState::RepositoryIoError => unreachable!(),
                };
                let mut item = test_m3u_hls_item(input, 12345, stream_ref, "http://origin.test/live.m3u8");
                item.additional_properties =
                    Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
                        bitrate: stored_bitrate,
                        ..Default::default()
                    })));
                let mut tree = crate::repository::BPlusTree::new();
                tree.insert(Arc::clone(&item.provider_id), item);
                tree.store(&database_path).expect("input tree");
            }
        }
    }

    async fn prepare_runtime_bandwidth_session(
        app_state: &Arc<AppState>,
        input: &ConfigInput,
        manifest_rendered_at_ms: u64,
    ) -> HlsSessionHandle {
        let origin_source = super::build_hls_origin_source(input, "channel-a");
        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                origin_source.session_key(),
                origin_source,
                &app_state.get_encrypt_secret(),
                manifest_rendered_at_ms,
            )
            .await;
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
        );
        {
            let mut session_guard = session.write().await;
            session_guard.apply_origin_manifest(&manifest).expect("runtime learning timeline");
            for entry in session_guard.segments.values_mut() {
                entry.status =
                    SegmentCacheStatus::Ready { content_length: 1_000_000, ready_at_ms: manifest_rendered_at_ms };
            }
            session_guard.advance_media_readiness_generation();
            session_guard.render_and_store_manifest(manifest_rendered_at_ms).expect("runtime learning manifest");
            session_guard.mark_authorized_media_access(manifest_rendered_at_ms);
        }
        session
    }

    async fn hls_runtime_bandwidth_manifest_case(
        repository_state: TestLiveBitrateRepositoryState,
    ) -> (HlsBandwidthPersistenceState, Option<u32>) {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_state = test_app_state();
        let current_config = app_state.app_config.config.load();
        app_state.app_config.config.store(Arc::new(Config {
            storage_dir: temp.path().to_string_lossy().into_owned(),
            ..current_config.as_ref().clone()
        }));
        let input = ConfigInput {
            id: 7,
            name: Arc::from(repository_state.input_name()),
            input_type: if matches!(repository_state, TestLiveBitrateRepositoryState::PermanentlyInapplicable) {
                InputType::Library
            } else {
                InputType::M3u
            },
            ..ConfigInput::default()
        };
        prepare_test_live_bitrate_repository(&input, temp.path(), repository_state);
        let manifest_rendered_at_ms = super::current_time_millis();
        let session = prepare_runtime_bandwidth_session(&app_state, &input, manifest_rendered_at_ms).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let access_lease_id = HlsAccessLeaseId(format!("{}-lease", repository_state.input_name()));
        let now_ms = super::current_time_millis();
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                access_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id,
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                input.id,
                "channel-a".to_string(),
                12345,
                now_ms,
                60_000,
            ))
            .await;

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 0 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
            super::HlsRuntimeBandwidthLearningContext::Eligible(&input),
        )
        .await
        .expect("cached media manifest response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response_body(response).await.is_empty());

        let bandwidth_persistence = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let state = session.read().await.bandwidth_persistence;
                if !matches!(state, HlsBandwidthPersistenceState::Idle | HlsBandwidthPersistenceState::InFlight { .. })
                {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bandwidth persistence completion");
        let stored_bitrate = if matches!(
            repository_state,
            TestLiveBitrateRepositoryState::ExistingHigher | TestLiveBitrateRepositoryState::Update
        ) {
            crate::repository::load_input_live_bitrate_bps(&app_state.app_config, &input, "channel-a")
                .await
                .expect("stored bitrate read")
        } else {
            None
        };
        (bandwidth_persistence, stored_bitrate)
    }

    #[tokio::test]
    async fn hls_runtime_bandwidth_missing_database_retries_without_failing_media_manifest() {
        let (state, _) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::MissingDatabase).await;

        assert!(matches!(state, HlsBandwidthPersistenceState::RetryAfter { .. }));
    }

    #[tokio::test]
    async fn hls_runtime_bandwidth_missing_item_retries_without_failing_media_manifest() {
        let (state, _) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::MissingStreamItem).await;

        assert!(matches!(state, HlsBandwidthPersistenceState::RetryAfter { .. }));
    }

    #[tokio::test]
    async fn hls_runtime_bandwidth_existing_higher_completes_without_failing_media_manifest() {
        let (state, stored_bitrate) =
            hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::ExistingHigher).await;

        assert!(matches!(state, HlsBandwidthPersistenceState::Persisted { bitrate_bps: 2_000_000 }));
        assert_eq!(stored_bitrate, Some(3_000_000));
    }

    #[tokio::test]
    async fn hls_runtime_bandwidth_update_completes_without_failing_media_manifest() {
        let (state, stored_bitrate) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::Update).await;

        assert!(matches!(state, HlsBandwidthPersistenceState::Persisted { bitrate_bps: 2_000_000 }));
        assert_eq!(stored_bitrate, Some(2_000_000));
    }

    #[tokio::test]
    async fn hls_runtime_bandwidth_inapplicable_and_io_error_do_not_fail_media_manifest() {
        let (inapplicable, _) =
            hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::PermanentlyInapplicable).await;
        let (io_error, _) =
            hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::RepositoryIoError).await;

        assert!(matches!(
            inapplicable,
            HlsBandwidthPersistenceState::PermanentlyInapplicable { bitrate_bps: 2_000_000 }
        ));
        assert!(matches!(io_error, HlsBandwidthPersistenceState::RetryAfter { .. }));
    }

    #[tokio::test]
    async fn hls_runtime_bandwidth_persistence_is_entry_gated_and_deduplicated() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_state = test_app_state();
        let current_config = app_state.app_config.config.load();
        app_state.app_config.config.store(Arc::new(Config {
            storage_dir: temp.path().to_string_lossy().into_owned(),
            ..current_config.as_ref().clone()
        }));
        let input = ConfigInput {
            id: 7,
            name: Arc::from("runtime-bandwidth-input"),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let storage_path =
            crate::repository::build_input_storage_path(&input.name, temp.path().to_string_lossy().as_ref());
        std::fs::create_dir_all(&storage_path).expect("input storage");
        let item = test_m3u_hls_item(&input, 12345, "channel-a", "http://origin.test/live.m3u8");
        let mut tree = crate::repository::BPlusTree::new();
        tree.insert(Arc::clone(&item.provider_id), item);
        tree.store(&crate::repository::get_input_m3u_playlist_file_path(&storage_path, &input.name))
            .expect("input tree");

        let origin_source = super::build_hls_origin_source(&input, "channel-a");
        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                origin_source.session_key(),
                origin_source,
                &app_state.get_encrypt_secret(),
                1,
            )
            .await;
        {
            let manifest = normal_manifest(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
                 #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
            );
            let mut session_guard = session.write().await;
            session_guard.apply_origin_manifest(&manifest).expect("runtime learning timeline");
            for entry in session_guard.segments.values_mut() {
                entry.status = SegmentCacheStatus::Ready { content_length: 1_000_000, ready_at_ms: 2 };
            }
        }

        assert!(super::spawn_hls_runtime_bandwidth_persistence(
            &app_state,
            &session,
            super::HlsRuntimeBandwidthLearningContext::Disabled,
        )
        .is_none());
        assert_eq!(
            crate::repository::load_input_live_bitrate_bps(&app_state.app_config, &input, "channel-a")
                .await
                .expect("unknown bitrate read"),
            None
        );

        let task = super::spawn_hls_runtime_bandwidth_persistence(
            &app_state,
            &session,
            super::HlsRuntimeBandwidthLearningContext::Eligible(&input),
        )
        .expect("runtime persistence task");
        task.await.expect("runtime persistence completion");

        assert_eq!(
            crate::repository::load_input_live_bitrate_bps(&app_state.app_config, &input, "channel-a")
                .await
                .expect("persisted bitrate read"),
            Some(2_000_000)
        );
        assert!(super::spawn_hls_runtime_bandwidth_persistence(
            &app_state,
            &session,
            super::HlsRuntimeBandwidthLearningContext::Eligible(&input),
        )
        .is_none());
    }

    fn store_test_sources_with_target(app_state: &Arc<AppState>, input: ConfigInput, target: ConfigTarget) {
        let input = Arc::new(input);
        let inputs = vec![Arc::clone(&input)];
        app_state.app_config.sources.store(Arc::new(SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            group_lookup: build_group_lookup(&inputs),
            inputs,
            sources: vec![ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![Arc::new(target)] }],
            templates: None,
        }));
    }

    fn configure_default_test_server(app_state: &Arc<AppState>) {
        let users = app_state
            .app_config
            .api_proxy
            .load_full()
            .as_ref()
            .map_or_else(Vec::new, |api_proxy| api_proxy.user.clone());
        app_state.app_config.api_proxy.store(Some(Arc::new(ApiProxyConfig {
            server: vec![ApiProxyServerInfo {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some("8901".to_string()),
                timezone: "UTC".to_string(),
                message: String::new(),
                path: None,
            }],
            user: users,
            ..Default::default()
        })));
    }

    #[test]
    fn hls_custom_video_manifest_body_is_none_for_non_provisioning() {
        let user = hls_custom_video_test_user();
        let manifest = build_hls_custom_video_manifest_body(
            "https://example.test/iptv/",
            &user,
            CustomVideoStreamType::UserConnectionsExhausted,
        );

        assert!(manifest.is_none(), "non-provisioning custom video types have no static manifest body");
    }

    #[tokio::test]
    async fn hls_initial_manifest_decision_wait_timeout_defaults_to_ninety_seconds() {
        let app_state = test_app_state();
        assert_eq!(super::hls_initial_manifest_decision_wait_timeout(&app_state), Duration::from_secs(90));
    }

    #[tokio::test]
    async fn hls_manifest_channel_unavailable_renders_inline_without_redirect() {
        let app_state = test_app_state();
        enable_channel_unavailable_custom_response(&app_state);

        let response = super::hls_manifest_channel_unavailable_response_for_username(&app_state, "hls-user").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
        assert!(body.contains("#EXT-X-ENDLIST"));
    }

    #[tokio::test]
    async fn hls_manifest_channel_unavailable_falls_back_to_not_found_when_custom_response_is_disabled() {
        let app_state = test_app_state();
        disable_custom_stream_response(&app_state);

        let response = super::hls_manifest_channel_unavailable_response_for_username(&app_state, "hls-user").await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    const RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES: u64 = 20 * 1024 * 1024;

    struct RecoveryBeforeCutoverFixture {
        _temp_dir: tempfile::TempDir,
        origin: TestSegmentOrigin,
        origin_phase: Arc<AtomicUsize>,
        app_state: Arc<AppState>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        refresh: OriginRefreshRequest,
    }

    async fn assert_initial_recovery_window(fixture: &RecoveryBeforeCutoverFixture) {
        let response = try_test_hls_cached_manifest_response(
            &fixture.app_state,
            &fixture.session,
            &fixture.lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_secs(10)),
        )
        .await
        .expect("READY initial manifest");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains("/000000.ts"));
        assert!(body.contains("/000002.ts"));
        assert!(!body.contains("/000003.ts"));
        let now_ms = super::current_time_millis();
        assert!(fixture
            .app_state
            .hls_proxy
            .activate_access_lease(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 },
            )
            .await
            .is_activated());
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, now_ms)
            .await
            .expect("active stripped lease");
        let snapshot = lease.last_manifest_snapshot.as_ref().expect("lease manifest snapshot");
        assert_eq!(snapshot.visible_segments.len(), 3);
        assert_eq!(snapshot.last_proxy_seq, 2);
        let evidence = prepare_terminal_base_evidence(
            &fixture.session,
            fixture.app_state.hls_proxy.segment_cache(),
            snapshot,
            now_ms,
        )
        .await;
        assert_eq!(evidence.track_signature(), Some(terminal_test_asset().track_signature().clone()));
        evidence.release();
        extend_ready_segment_as_sparse_file(
            &fixture.app_state,
            &fixture.session,
            2,
            RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES,
        )
        .await;
        let uri = format!("/hls/shared/live/{}/{}/000002.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
        let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(segment.status(), StatusCode::OK);
        assert_eq!(segment.headers()[header::CONTENT_LENGTH], RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES.to_string());
        assert_eq!(
            u64::try_from(response_body(segment).await.len()).unwrap_or(u64::MAX),
            RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES
        );
    }

    async fn recovery_before_cutover_fixture() -> RecoveryBeforeCutoverFixture {
        let temp_dir = tempfile::tempdir().expect("recovery cache tempdir");
        let unchanged_manifest = Arc::<[u8]>::from(regression_origin_manifest(100, 6));
        let progressed_manifest = Arc::<[u8]>::from(regression_origin_manifest(101, 6));
        let segment = Arc::<[u8]>::from(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .as_slice(),
        );
        let origin_phase = Arc::new(AtomicUsize::new(0));
        let phase = Arc::clone(&origin_phase);
        let origin = spawn_test_binary_origin(Arc::new(move |path| {
            if path_has_extension(path, "m3u8") {
                return match phase.load(Ordering::SeqCst) {
                    0 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&unchanged_manifest)),
                    1 => TestBinaryOriginResponse::new(
                        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                        Arc::<[u8]>::from(&b"retry"[..]),
                    ),
                    _ => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&progressed_manifest)),
                };
            }
            TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&segment))
        }))
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("recovery-regression-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state =
            test_app_state_with_hls_proxy_and_inputs(test_beast_hls_proxy(temp_dir.path()), vec![Arc::new(input)]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let manifest_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 1_000)
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("recovery-before-cutover".to_string());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &lease_id).await;
        let refresh =
            regression_origin_refresh_request(&app_state, Arc::clone(&session), &manifest_url, Some(lease_id.clone()));
        let fixture = RecoveryBeforeCutoverFixture {
            _temp_dir: temp_dir,
            origin,
            origin_phase,
            app_state,
            session,
            proxy_session_id,
            lease_id,
            refresh,
        };
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        wait_for_ready_timeline(&fixture.session, 6).await;
        assert_initial_recovery_window(&fixture).await;
        fixture
    }

    async fn run_recovery_outage(fixture: &mut RecoveryBeforeCutoverFixture) -> (u64, Option<u64>) {
        let progress_generation = fixture.session.read().await.origin_control.progress_generation;
        for _ in 0..2 {
            fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
            assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        }
        assert_eq!(fixture.session.read().await.origin_control.progress_generation, progress_generation);
        fixture.origin_phase.store(1, Ordering::SeqCst);
        let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
        assert_eq!(fixture.app_state.hls_proxy.manifest_recovery_burst().level.plan(), plan);
        fixture.refresh.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
        let requests_before = fixture.origin.manifest_request_count();
        let mut last_episode_generation = None;
        for _ in 0..3 {
            fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
            assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
            let lease = fixture
                .app_state
                .hls_proxy
                .access_lease_response_snapshot(
                    &fixture.lease_id,
                    &fixture.proxy_session_id,
                    super::current_time_millis(),
                )
                .await
                .expect("407 exhaustion cannot remove the lease");
            assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
            let session = fixture.session.read().await;
            let episode = session.origin_control.acceptance_episode.as_ref().expect("bounded acceptance evidence");
            assert!(episode.full_burst_completed);
            assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
            last_episode_generation = Some(episode.generation.0);
        }
        assert!(
            fixture.origin.manifest_request_count().saturating_sub(requests_before)
                >= plan.total_candidates().saturating_mul(3)
        );
        let uri = format!("/hls/shared/live/{}/{}/000003.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
        let cached = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(cached.status(), StatusCode::OK);
        assert!(!response_body(cached).await.is_empty());
        (progress_generation, last_episode_generation)
    }

    async fn assert_recovery_after_outage(
        fixture: &mut RecoveryBeforeCutoverFixture,
        progress_generation: u64,
        last_episode_generation: Option<u64>,
    ) {
        fixture.origin_phase.store(2, Ordering::SeqCst);
        fixture.refresh.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
        fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        wait_for_ready_timeline(&fixture.session, 7).await;
        {
            let session = fixture.session.read().await;
            assert_eq!(session.origin_seq_highwater, Some(106));
            assert_eq!(session.proxy_next_seq, Some(7));
            assert!(session.origin_control.progress_generation > progress_generation);
            assert!(session.origin_control.acceptance_episode.is_none());
            assert!(last_episode_generation
                .is_some_and(|generation| session.origin_control.acceptance_generation.0 > generation));
        }
        let response = try_test_hls_cached_manifest_response(
            &fixture.app_state,
            &fixture.session,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("recovered normal manifest");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("recovered manifest utf8");
        assert!(body.contains("/000006.ts"));
        assert!(!body.contains("/terminal/"));
        assert!(!body.contains("#EXT-X-ENDLIST"));
        let uri = format!("/hls/shared/live/{}/{}/000006.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
        let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(segment.status(), StatusCode::OK);
        assert!(!response_body(segment).await.is_empty());
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("recovered lease remains stored");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    }

    #[tokio::test]
    async fn recovers_before_lease_cutover_without_terminal_tail() {
        let mut fixture = recovery_before_cutover_fixture().await;
        let (progress_generation, last_episode_generation) = run_recovery_outage(&mut fixture).await;
        assert_recovery_after_outage(&mut fixture, progress_generation, last_episode_generation).await;
    }

    struct StaleOriginServers {
        pinned: TestSegmentOrigin,
        alternative: TestSegmentOrigin,
        pinned_phase: Arc<AtomicUsize>,
        alternative_phase: Arc<AtomicUsize>,
        burst_candidates: Arc<AtomicUsize>,
        pinned_candidates: Arc<AtomicUsize>,
    }

    async fn spawn_stale_origin_servers() -> StaleOriginServers {
        let pinned_manifest = Arc::<[u8]>::from(regression_origin_manifest(100, 6));
        let alternative_manifest = Arc::<[u8]>::from(regression_origin_manifest(200, 6));
        let continued_manifest = Arc::<[u8]>::from(regression_origin_manifest(201, 6));
        let segment = Arc::<[u8]>::from(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .as_slice(),
        );
        let alternative_phase = Arc::new(AtomicUsize::new(0));
        let alternative_phase_for_handler = Arc::clone(&alternative_phase);
        let alternative_segment = Arc::clone(&segment);
        let alternative = spawn_test_binary_origin(Arc::new(move |path| {
            if path_has_extension(path, "m3u8") {
                let body = if alternative_phase_for_handler.load(Ordering::SeqCst) == 0 {
                    Arc::clone(&alternative_manifest)
                } else {
                    Arc::clone(&continued_manifest)
                };
                return TestBinaryOriginResponse::new(StatusCode::OK, body);
            }
            TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&alternative_segment))
        }))
        .await;
        let alternative_url =
            format!("{}/live/user/pass/12345.m3u8", alternative.base_url.replacen("127.0.0.1", "localhost", 1));
        let pinned_phase = Arc::new(AtomicUsize::new(0));
        let burst_candidates = Arc::new(AtomicUsize::new(0));
        let pinned_candidates = Arc::new(AtomicUsize::new(0));
        let handler_phase = Arc::clone(&pinned_phase);
        let handler_burst_candidates = Arc::clone(&burst_candidates);
        let handler_pinned_candidates = Arc::clone(&pinned_candidates);
        let handler_pinned_manifest = Arc::clone(&pinned_manifest);
        let pinned_segment = Arc::clone(&segment);
        let pinned = spawn_test_binary_origin(Arc::new(move |path| {
            if !path_has_extension(path, "m3u8") {
                return TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&pinned_segment));
            }
            match handler_phase.load(Ordering::SeqCst) {
                0 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&handler_pinned_manifest)),
                1 => {
                    let index = handler_burst_candidates.fetch_add(1, Ordering::SeqCst);
                    if index.is_multiple_of(2) {
                        handler_pinned_candidates.fetch_add(1, Ordering::SeqCst);
                        TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&handler_pinned_manifest))
                    } else {
                        TestBinaryOriginResponse::redirect(alternative_url.clone())
                    }
                }
                _ => TestBinaryOriginResponse::redirect(alternative_url.clone()),
            }
        }))
        .await;
        StaleOriginServers { pinned, alternative, pinned_phase, alternative_phase, burst_candidates, pinned_candidates }
    }

    struct StaleOriginFixture {
        _temp_dir: tempfile::TempDir,
        servers: StaleOriginServers,
        app_state: Arc<AppState>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        refresh: OriginRefreshRequest,
        initial_visible_tail: u64,
        initial_progress_generation: u64,
        initial_origin_epoch: u64,
        initial_progress_at_ms: Option<u64>,
    }

    async fn stale_origin_fixture() -> StaleOriginFixture {
        let temp_dir = tempfile::tempdir().expect("stale-origin cache tempdir");
        let servers = spawn_stale_origin_servers().await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("reachable-stale-origin-input"),
            input_type: InputType::M3u,
            url: servers.pinned.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state =
            test_app_state_with_hls_proxy_and_inputs(test_beast_hls_proxy(temp_dir.path()), vec![Arc::new(input)]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let manifest_url = format!("{}/live/user/pass/12345.m3u8", servers.pinned.base_url);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 1_000)
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("reachable-stale-origin".to_string());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &lease_id).await;
        let refresh =
            regression_origin_refresh_request(&app_state, Arc::clone(&session), &manifest_url, Some(lease_id.clone()));
        assert!(trigger_origin_refresh_sync(refresh.clone()).await);
        wait_for_ready_timeline(&session, 6).await;
        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_secs(10)),
        )
        .await
        .expect("READY pinned-origin manifest");
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert_eq!(media_uri_count(&body), 3);
        let now_ms = super::current_time_millis();
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 },
            )
            .await
            .is_activated());
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
            .await
            .expect("activated pinned-origin lease");
        let initial_visible_tail =
            lease.last_manifest_snapshot.as_ref().expect("initial lease manifest snapshot").last_proxy_seq;
        let (initial_progress_generation, initial_origin_epoch, initial_progress_at_ms) = {
            let session = session.read().await;
            assert_eq!(session.origin_seq_highwater, Some(105));
            assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
            (
                session.origin_control.progress_generation,
                session.origin_epoch,
                session.origin_control.last_media_progress_at_ms,
            )
        };
        StaleOriginFixture {
            _temp_dir: temp_dir,
            servers,
            app_state,
            session,
            proxy_session_id,
            lease_id,
            refresh,
            initial_visible_tail,
            initial_progress_generation,
            initial_origin_epoch,
            initial_progress_at_ms,
        }
    }

    async fn observe_stale_origin(fixture: &mut StaleOriginFixture) -> HlsManifestAcceptanceDirective {
        let requests_before = fixture.servers.pinned.manifest_request_count();
        for _ in 0..2 {
            fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
            assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        }
        {
            let session = fixture.session.read().await;
            assert_eq!(session.origin_seq_highwater, Some(105));
            assert_eq!(session.origin_control.progress_generation, fixture.initial_progress_generation);
            assert_eq!(session.origin_control.last_media_progress_at_ms, fixture.initial_progress_at_ms);
            assert_eq!(session.origin_refresh.consecutive_failures, 0);
        }
        assert_eq!(fixture.servers.pinned.manifest_request_count().saturating_sub(requests_before), 2);
        assert_eq!(fixture.servers.alternative.manifest_request_count(), 0);
        let response = try_test_hls_cached_manifest_response(
            &fixture.app_state,
            &fixture.session,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("reachable stale origin keeps the committed live manifest");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("stale live manifest utf8");
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(!body.contains("/terminal/"));
        assert!(!body.contains("#EXT-X-ENDLIST"));
        {
            let mut session = fixture.session.write().await;
            for segment in
                session.segments.values_mut().filter(|segment| segment.proxy_seq > fixture.initial_visible_tail)
            {
                segment.duration_ms = 10_000;
            }
            session.origin_control.last_media_progress_at_ms = Some(0);
            session.advance_media_readiness_generation();
        }
        let directive = match crate::api::model::hls_manifest_acceptance_directive_for_session(
            &fixture.app_state.hls_ctx(),
            &fixture.session,
            &fixture.proxy_session_id,
        )
        .await
        {
            HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
            other => panic!("stale progress evidence must evaluate: {other:?}"),
        };
        assert!(directive.trigger.recovery_required());
        assert_eq!(fixture.session.read().await.origin_control.path_condition, HlsOriginPathCondition::PublicationLate);
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("stale progress evidence keeps the lease stored");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        directive
    }

    async fn assert_stale_origin_handoff(
        fixture: &mut StaleOriginFixture,
        directive: HlsManifestAcceptanceDirective,
    ) -> u64 {
        let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
        assert_eq!(fixture.app_state.hls_proxy.manifest_recovery_burst().level.plan(), plan);
        fixture.servers.pinned_phase.store(1, Ordering::SeqCst);
        let requests_before = fixture.servers.pinned.manifest_request_count();
        fixture.refresh.acceptance_directive = directive;
        fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        wait_for_ready_timeline(&fixture.session, 12).await;
        assert_eq!(
            fixture.servers.pinned.manifest_request_count().saturating_sub(requests_before),
            plan.total_candidates()
        );
        assert_eq!(fixture.servers.burst_candidates.load(Ordering::SeqCst), plan.total_candidates());
        assert!(fixture.servers.pinned_candidates.load(Ordering::SeqCst) > 0);
        assert!(fixture.servers.alternative.manifest_request_count() >= 2);
        let progress_generation = {
            let session = fixture.session.read().await;
            assert_eq!(session.origin_seq_highwater, Some(205));
            assert_eq!(session.proxy_next_seq, Some(12));
            assert_eq!(session.last_effective_manifest_host.as_deref(), Some("localhost"));
            assert_eq!(session.origin_epoch, fixture.initial_origin_epoch.saturating_add(1));
            assert!(session.origin_control.progress_generation > fixture.initial_progress_generation);
            session.origin_control.progress_generation
        };
        let response = try_test_hls_cached_manifest_response(
            &fixture.app_state,
            &fixture.session,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("cross-host recovery manifest");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("recovered manifest utf8");
        assert!(body.contains("/000006.ts"));
        assert!(body.contains("#EXT-X-DISCONTINUITY"));
        assert!(!body.contains("/terminal/"));
        assert!(!body.contains("#EXT-X-ENDLIST"));
        let uri = format!("/hls/shared/live/{}/{}/000006.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
        let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(segment.status(), StatusCode::OK);
        assert!(!response_body(segment).await.is_empty());
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("recovered lease remains stored");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        progress_generation
    }

    async fn assert_stale_origin_continuation(fixture: &mut StaleOriginFixture, progress_generation: u64) {
        fixture.servers.alternative_phase.store(1, Ordering::SeqCst);
        fixture.servers.pinned_phase.store(2, Ordering::SeqCst);
        fixture.refresh.acceptance_directive = HlsManifestAcceptanceDirective::none();
        fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        wait_for_ready_timeline(&fixture.session, 13).await;
        {
            let session = fixture.session.read().await;
            assert_eq!(session.origin_seq_highwater, Some(206));
            assert_eq!(session.proxy_next_seq, Some(13));
            assert_eq!(session.origin_epoch, fixture.initial_origin_epoch.saturating_add(1));
            assert!(session.origin_control.progress_generation > progress_generation);
        }
        let response = try_test_hls_cached_manifest_response(
            &fixture.app_state,
            &fixture.session,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("continued alternative-origin timeline");
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("continued manifest utf8");
        assert!(body.contains("/000012.ts"));
        assert!(!body.contains("/terminal/"));
        assert!(!body.contains("#EXT-X-ENDLIST"));
        let uri = format!("/hls/shared/live/{}/{}/000012.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
        let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(segment.status(), StatusCode::OK);
        assert!(!response_body(segment).await.is_empty());
    }

    #[tokio::test]
    async fn reachable_stale_origin_hands_off_to_progressed_origin_without_terminal_tail() {
        let mut fixture = stale_origin_fixture().await;
        let directive = observe_stale_origin(&mut fixture).await;
        let progress_generation = assert_stale_origin_handoff(&mut fixture, directive).await;
        assert_stale_origin_continuation(&mut fixture, progress_generation).await;
    }

    #[tokio::test]
    async fn terminal_lease_manifest_is_inline_immutable_endlist_on_canonical_path() {
        const LIVE_TAIL_BYTES: &[u8] = b"original-live-tail";
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", LIVE_TAIL_BYTES).await;
        let lease_id = format!("test-access-lease-{proxy_session_id}");
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
        let cursor_before = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId(lease_id.clone()),
                &ProxySessionId(proxy_session_id.clone()),
                super::current_time_millis(),
            )
            .await
            .expect("terminal lease before media read")
            .playback_cursor;
        let (generation, segment_count) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
        let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/manifest.m3u8");
        let mut reloaded_api_proxy =
            app_state.app_config.api_proxy.load_full().as_deref().cloned().expect("test API proxy config");
        reloaded_api_proxy.server[0].path = Some("reloaded".to_string());
        app_state.app_config.api_proxy.store(Some(Arc::new(reloaded_api_proxy)));

        let response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest is utf8");
        let repeated = get_response(Arc::clone(&app_state), &manifest_uri, None).await;
        assert_eq!(repeated.status(), StatusCode::OK);
        assert!(!repeated.headers().contains_key(header::LOCATION));
        assert_eq!(response_body(repeated).await, body.as_bytes());
        let live_tail = format!("/{proxy_session_id}/{lease_id}/000123.ts");
        let terminal_prefix = format!("/{proxy_session_id}/{lease_id}/terminal/{generation}/");
        assert!(body.contains(&live_tail));
        assert!(body.contains("/iptv/hls/shared/live/"));
        assert!(!body.contains("/reloaded/hls/shared/live/"));
        assert_eq!(body.matches(&terminal_prefix).count(), usize::from(segment_count));
        assert_eq!(body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
        assert!(body.find(&live_tail) < body.find("#EXT-X-DISCONTINUITY\n"));
        assert!(body.find("#EXT-X-DISCONTINUITY\n") < body.find(&terminal_prefix));

        let live_tail_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/000123.ts");
        let live_tail_response = get_response(Arc::clone(&app_state), &live_tail_uri, Some("bytes=0-")).await;
        assert_eq!(live_tail_response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(live_tail_response.headers()[header::CONTENT_LENGTH], LIVE_TAIL_BYTES.len().to_string());
        assert_eq!(
            live_tail_response.headers()[header::CONTENT_RANGE],
            format!("bytes 0-{}/{}", LIVE_TAIL_BYTES.len() - 1, LIVE_TAIL_BYTES.len())
        );
        assert_eq!(response_body(live_tail_response).await, bytes::Bytes::from_static(LIVE_TAIL_BYTES));
        let cursor_after = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId(lease_id),
                &ProxySessionId(proxy_session_id),
                super::current_time_millis(),
            )
            .await
            .expect("terminal lease after media read")
            .playback_cursor;
        assert_eq!(cursor_after, cursor_before);
    }

    struct PreparedTerminalCutoverFixture {
        _temp_dir: tempfile::TempDir,
        origin: TestSegmentOrigin,
        app_state: Arc<AppState>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        request_url: String,
        base_manifest: HlsLeaseManifestSnapshot,
        asset_buffer: TransportStreamBuffer,
        asset: Arc<HlsTerminalMediaAsset>,
    }

    async fn prepare_terminal_cutover_bundle(
        app_state: &Arc<AppState>,
        base_manifest: &HlsLeaseManifestSnapshot,
    ) -> (TransportStreamBuffer, Arc<HlsTerminalMediaAsset>) {
        let asset_buffer = app_state
            .app_config
            .custom_stream_response
            .load_full()
            .as_ref()
            .and_then(|responses| responses.channel_unavailable.as_ref())
            .cloned()
            .expect("configured terminal renderer");
        let asset = snapshot_terminal_media_asset(&asset_buffer).expect("terminal asset snapshot");
        let key =
            prepared_terminal_bundle_key(&asset, base_manifest.target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        let state = app_state.hls_proxy.start_prepared_terminal_bundle(
            Arc::clone(&asset),
            base_manifest.target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = match state {
            HlsPreparedTerminalBundleState::Preparing { .. } => app_state
                .hls_proxy
                .wait_for_prepared_terminal_bundle(key)
                .await
                .expect("prepared terminal bundle completion"),
            state => state,
        };
        assert!(matches!(
            state,
            HlsPreparedTerminalBundleState::Ready { ref bundle }
                if bundle.key == key
                    && bundle.segments.len() == usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT)
        ));
        assert_eq!(asset_buffer.finite_hls_render_count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
        (asset_buffer, asset)
    }

    async fn prepared_terminal_cutover_fixture() -> PreparedTerminalCutoverFixture {
        let temp_dir = tempfile::tempdir().expect("terminal cutover tempdir");
        let origin = spawn_test_encrypted_hls_origin(
            AES_TEST_MANIFEST,
            Arc::from(AES_TEST_KEY_BYTES),
            Arc::from(AES_TEST_PLAINTEXT_SEGMENT),
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("terminal-regression-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_hls_proxy_and_inputs(
            test_beast_hls_proxy(temp_dir.path()),
            vec![Arc::new(input.clone())],
        );
        enable_hls_cache(&app_state);
        enable_channel_unavailable_custom_response(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/channel/index.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let lease_id = HlsAccessLeaseId("prepared-terminal-cutover".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &lease_id).await;
        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("AES shared session cold start");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("AES manifest utf8");
        assert!(body.contains("#EXT-X-KEY:METHOD=AES-128"));
        let session = app_state.hls_proxy.sessions().get_by_key(&session_key).await.expect("AES shared session");
        wait_for_ready_timeline(&session, 6).await;
        let now_ms = super::current_time_millis();
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 },
            )
            .await
            .is_activated());
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
            .await
            .expect("live AES lease");
        let base_manifest = lease.last_manifest_snapshot.as_ref().expect("frozen AES lease manifest").clone();
        assert_eq!(base_manifest.target_duration_ms, 12_000);
        assert_eq!(base_manifest.visible_segments.len(), 3);
        assert!(base_manifest.active_encryption.is_some());
        let (asset_buffer, asset) = prepare_terminal_cutover_bundle(&app_state, &base_manifest).await;
        for segment in base_manifest.visible_segments.iter() {
            let response = get_response(Arc::clone(&app_state), &segment.uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response_body(response).await.is_empty());
        }
        PreparedTerminalCutoverFixture {
            _temp_dir: temp_dir,
            origin,
            app_state,
            session,
            proxy_session_id,
            lease_id,
            request_url,
            base_manifest,
            asset_buffer,
            asset,
        }
    }

    async fn apply_terminal_cutover_pressure(fixture: &PreparedTerminalCutoverFixture) {
        let mut session = fixture.session.write().await;
        session.origin_control.path_condition = HlsOriginPathCondition::HardFetchFailure;
        let transition_buffer_seq = fixture.base_manifest.last_proxy_seq.saturating_add(1);
        let commit_guard_seq = transition_buffer_seq.saturating_add(1);
        for segment in
            session.segments.values_mut().filter(|segment| segment.proxy_seq > fixture.base_manifest.last_proxy_seq)
        {
            if segment.proxy_seq == transition_buffer_seq {
                segment.duration_ms = 1_000;
            } else if segment.proxy_seq == commit_guard_seq {
                segment.duration_ms = 2_800;
            } else {
                segment.status = SegmentCacheStatus::Expired;
            }
        }
        session.advance_media_readiness_generation();
    }

    async fn terminal_cutover_acceptance_directive(
        fixture: &PreparedTerminalCutoverFixture,
    ) -> HlsManifestAcceptanceDirective {
        apply_terminal_cutover_pressure(fixture).await;
        let directive = match crate::api::model::hls_manifest_acceptance_directive_for_session(
            &fixture.app_state.hls_ctx(),
            &fixture.session,
            &fixture.proxy_session_id,
        )
        .await
        {
            HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
            other => panic!("deterministic recovery-pressure snapshot must evaluate: {other:?}"),
        };
        assert_eq!(directive.trigger, HlsManifestAcceptanceTrigger::RecoveryRequired);
        let bundle_key = prepared_terminal_bundle_key(
            &fixture.asset,
            fixture.base_manifest.target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let timing = directive.timing_seed.expect("prepared acceptance timing seed");
        assert_eq!(timing.required_terminal_media_key, Some(bundle_key));
        assert_eq!(timing.terminal_media_preparation, HlsTerminalMediaPreparationState::Ready { key: bundle_key });
        directive
    }

    async fn exhaust_terminal_cutover_recovery(
        fixture: &PreparedTerminalCutoverFixture,
        directive: HlsManifestAcceptanceDirective,
    ) -> HlsAccessLease {
        let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
        let requests_before = fixture.origin.manifest_request_count();
        let mut refresh = regression_origin_refresh_request(
            &fixture.app_state,
            Arc::clone(&fixture.session),
            &fixture.request_url,
            Some(fixture.lease_id.clone()),
        );
        refresh.acceptance_directive = directive;
        refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(refresh).await);
        assert!(fixture.origin.manifest_request_count().saturating_sub(requests_before) >= plan.total_candidates());
        apply_terminal_cutover_pressure(fixture).await;
        let bundle_key = prepared_terminal_bundle_key(
            &fixture.asset,
            fixture.base_manifest.target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        {
            let session = fixture.session.read().await;
            let episode = session.origin_control.acceptance_episode.as_ref().expect("exhausted acceptance episode");
            assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
            assert!(episode.full_burst_completed);
            assert_eq!(episode.timing().required_terminal_media_key, Some(bundle_key));
            assert_eq!(
                episode.timing().terminal_media_preparation,
                HlsTerminalMediaPreparationState::Ready { key: bundle_key }
            );
            assert!(episode.exhaustion_reason().is_some());
        }
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("pressured live lease");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert_eq!(
            lease.playback_cursor.highest_contiguous_completed_proxy_seq,
            Some(fixture.base_manifest.last_proxy_seq)
        );
        {
            let session = fixture.session.read().await;
            let ready_after_tail = session
                .segments
                .values()
                .filter(|segment| segment.proxy_seq > fixture.base_manifest.last_proxy_seq)
                .filter_map(|segment| {
                    matches!(segment.status, SegmentCacheStatus::Ready { .. })
                        .then_some((segment.proxy_seq, segment.duration_ms))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                ready_after_tail,
                vec![
                    (fixture.base_manifest.last_proxy_seq.saturating_add(1), 1_000),
                    (fixture.base_manifest.last_proxy_seq.saturating_add(2), 2_800),
                ]
            );
            assert!(session.origin_control.path_condition.is_degraded());
        }
        lease
    }

    async fn commit_prepared_terminal_cutover(
        fixture: &PreparedTerminalCutoverFixture,
        pressured_lease: &HlsAccessLease,
    ) -> (u64, u64) {
        let cutover_now_ms = pressured_lease
            .playback_cursor
            .first_segment_completed_at_ms
            .expect("measured lease playback start")
            .saturating_add(fixture.base_manifest.playlist_duration_ms);
        let first = commit_terminal_tail_if_lease_reserve_requires_cutover(
            &fixture.app_state.hls_ctx(),
            &fixture.session,
            &fixture.proxy_session_id,
            pressured_lease,
            cutover_now_ms,
        )
        .await;
        assert_eq!(first, HlsTerminalResolution::Committed);
        let terminal_lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("terminal lease remains stored");
        let second = commit_terminal_tail_if_lease_reserve_requires_cutover(
            &fixture.app_state.hls_ctx(),
            &fixture.session,
            &fixture.proxy_session_id,
            &terminal_lease,
            cutover_now_ms.saturating_add(1),
        )
        .await;
        assert_eq!(second, HlsTerminalResolution::Committed);
        let HlsLeasePlaybackMode::TerminalTail(plan) = terminal_lease.playback_mode else {
            panic!("prepared terminal tail must commit");
        };
        assert_eq!(plan.segment_count, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        (plan.generation.0, cutover_now_ms)
    }

    async fn assert_prepared_terminal_cutover_manifest(fixture: &PreparedTerminalCutoverFixture, generation: u64) {
        let manifest_uri =
            format!("/hls/shared/live/{}/{}/manifest.m3u8", fixture.proxy_session_id.0, fixture.lease_id.0);
        let response = get_response(Arc::clone(&fixture.app_state), &manifest_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest utf8");
        let live_tail_path = format!(
            "/{}/{}/{:06}.ts",
            fixture.proxy_session_id.0, fixture.lease_id.0, fixture.base_manifest.last_proxy_seq
        );
        let terminal_prefix = format!("/{}/{}/terminal/{generation}/", fixture.proxy_session_id.0, fixture.lease_id.0);
        assert!(body.contains(&live_tail_path));
        assert_eq!(body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        assert!(body.find(&live_tail_path) < body.find("#EXT-X-DISCONTINUITY\n"));
        let key_reset = body.find("#EXT-X-KEY:METHOD=NONE\n").expect("AES-to-clear key reset");
        let discontinuity = body.find("#EXT-X-DISCONTINUITY\n").expect("terminal discontinuity");
        assert!(key_reset < discontinuity);
        assert_eq!(body.matches(&terminal_prefix).count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
        for index in 0..HLS_TERMINAL_TAIL_SEGMENT_COUNT {
            assert!(body.contains(&format!("{terminal_prefix}{index}.ts")));
        }
        let duration_ms = fixture.asset.duration_ms();
        let extinf = format!("#EXTINF:{}.{:03},", duration_ms / 1_000, duration_ms % 1_000);
        assert_eq!(body.matches(&extinf).count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
        let renders_before = fixture.asset_buffer.finite_hls_render_count();
        let zero_uri = format!(
            "/hls/shared/live/{}/{}/terminal/{generation}/0.ts",
            fixture.proxy_session_id.0, fixture.lease_id.0
        );
        let one_uri = format!(
            "/hls/shared/live/{}/{}/terminal/{generation}/1.ts",
            fixture.proxy_session_id.0, fixture.lease_id.0
        );
        let zero = response_body(get_response(Arc::clone(&fixture.app_state), &zero_uri, None).await).await;
        let one = response_body(get_response(Arc::clone(&fixture.app_state), &one_uri, None).await).await;
        assert_ne!(zero, one);
        assert_eq!(fixture.asset_buffer.finite_hls_render_count(), renders_before);
    }

    async fn assert_terminal_cutover_sticky_after_recovery(
        fixture: &PreparedTerminalCutoverFixture,
        generation: u64,
        cutover_now_ms: u64,
    ) {
        {
            let mut session = fixture.session.write().await;
            let recovered_manifest =
                normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:83\n#EXT-X-TARGETDURATION:12\n#EXTINF:12.0,\n83.ts\n");
            session.apply_origin_manifest(&recovered_manifest).expect("later shared-session recovery");
            session.origin_control.record_media_progress(cutover_now_ms.saturating_add(1), 12_000);
        }
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("sticky terminal lease");
        assert!(matches!(
            lease.playback_mode,
            HlsLeasePlaybackMode::TerminalTail(ref plan) if plan.generation.0 == generation
        ));
    }

    #[tokio::test]
    async fn commits_prepared_terminal_tail_once_when_recovery_misses_deadline() {
        let fixture = prepared_terminal_cutover_fixture().await;
        let directive = terminal_cutover_acceptance_directive(&fixture).await;
        let pressured_lease = exhaust_terminal_cutover_recovery(&fixture, directive).await;
        let (generation, cutover_now_ms) = commit_prepared_terminal_cutover(&fixture, &pressured_lease).await;
        assert_prepared_terminal_cutover_manifest(&fixture, generation).await;
        assert_terminal_cutover_sticky_after_recovery(&fixture, generation, cutover_now_ms).await;
    }

    #[tokio::test]
    async fn warm_fmp4_map_cutover_without_ready_reserve_fails_closed_without_a_ts_splice() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        enable_channel_unavailable_custom_response(&app_state);
        let proxy_session_id = ProxySessionId(map_hls_map(&app_state, b"fmp4-init", true).await);
        let lease_id = HlsAccessLeaseId(format!("test-access-lease-{}", proxy_session_id.0));
        let now_ms = super::current_time_millis();
        let (base_proxy_seq, duration_ms) = {
            let session = app_state
                .hls_proxy
                .sessions()
                .get_by_proxy_session_id(&proxy_session_id)
                .await
                .expect("fMP4 shared session");
            let session = session.read().await;
            let entry = session.segments.values().next().expect("fMP4 media entry");
            (entry.proxy_seq, entry.duration_ms)
        };
        let snapshot = HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(now_ms),
            snapshot_generation: 0,
            delivered_at_ms: now_ms,
            first_proxy_seq: base_proxy_seq,
            last_proxy_seq: base_proxy_seq,
            visible_segments: Arc::from([HlsLeaseManifestSegment {
                proxy_seq: base_proxy_seq,
                duration_ms,
                uri: format!("/iptv/hls/shared/live/{}/{}/{base_proxy_seq:06}.m4s", proxy_session_id.0, lease_id.0),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            }]),
            discontinuity_sequence: 0,
            target_duration_ms: terminal_test_asset().duration_ms().saturating_add(1_000),
            playlist_duration_ms: duration_ms,
            last_visible_media_end_ms: duration_ms,
            active_map: Some(HlsMapSignature { fingerprint: [7; 32], container: HlsMediaContainer::FragmentedMp4 }),
            active_encryption: None,
            container: HlsMediaContainer::FragmentedMp4,
        };
        publish_test_manifest_and_exhaust_configured_acceptance(
            &app_state,
            &proxy_session_id,
            &lease_id,
            snapshot,
            now_ms,
        )
        .await;
        let manifest_uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, lease_id.0);

        let response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert!(response_body(response).await.is_empty());
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, super::current_time_millis())
            .await
            .expect("failed-closed lease snapshot");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        let terminal_uri = format!("/hls/shared/live/{}/{}/terminal/1/0.ts", proxy_session_id.0, lease_id.0);
        let terminal_response = get_response(app_state, &terminal_uri, None).await;
        assert_eq!(terminal_response.status(), StatusCode::NOT_FOUND);
        assert!(!terminal_response.headers().contains_key(header::LOCATION));
        assert!(response_body(terminal_response).await.is_empty());
    }

    async fn terminal_head_content_length(
        app_state: &Arc<AppState>,
        proxy_session_id: &str,
        segment_uri: &str,
    ) -> usize {
        let head_response = request_response(Arc::clone(app_state), Method::HEAD, segment_uri, None).await;
        assert_eq!(head_response.status(), StatusCode::OK);
        assert_eq!(head_response.headers()[header::CONTENT_TYPE], "video/mp2t");
        assert_eq!(head_response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(head_response.headers()[header::CACHE_CONTROL].to_str().is_ok_and(|value| value.contains("immutable")));
        let content_length = head_response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .expect("HEAD content length")
            .parse::<usize>()
            .expect("HEAD content length value");
        assert!(content_length > 0);
        assert!(response_body(head_response).await.is_empty());
        assert_eq!(hls_session_last_media_at_ms(app_state, proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(app_state).await;

        let head_range = request_response(Arc::clone(app_state), Method::HEAD, segment_uri, Some("bytes=0-187")).await;
        assert_eq!(head_range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(head_range.headers()[header::CONTENT_LENGTH], "188");
        let expected_content_range = format!("bytes 0-187/{content_length}");
        assert_eq!(head_range.headers()[header::CONTENT_RANGE].to_str().ok(), Some(expected_content_range.as_str()));
        assert!(response_body(head_range).await.is_empty());
        assert_eq!(hls_session_last_media_at_ms(app_state, proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(app_state).await;
        content_length
    }

    #[tokio::test]
    async fn hls_terminal_response_serves_prepared_finite_full_and_range_bytes_per_index() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
        let lease_id = format!("test-access-lease-{proxy_session_id}");
        let terminal_renderer = terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
        let renders_before_requests = terminal_renderer.finite_hls_render_count();
        let finalizations_before_requests = terminal_renderer.finite_hls_finalize_count();
        assert_eq!(renders_before_requests, usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
        assert_eq!(finalizations_before_requests, usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
        let (generation, _) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
        let segment_zero_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.ts");
        let segment_one_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/1.ts");
        let repair_before = app_state.hls_proxy.segment_repair().stats().await;
        let provider_connections_before = app_state.active_provider.get_provider_connections_count().await;

        let head_content_length = terminal_head_content_length(&app_state, &proxy_session_id, &segment_zero_uri).await;

        let segment_zero_response = get_response(Arc::clone(&app_state), &segment_zero_uri, None).await;
        assert_eq!(segment_zero_response.status(), StatusCode::OK);
        assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&segment_zero_response));
        assert_eq!(segment_zero_response.headers()[header::CONTENT_TYPE], "video/mp2t");
        assert_eq!(segment_zero_response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(segment_zero_response.headers()[header::CACHE_CONTROL]
            .to_str()
            .is_ok_and(|value| value.contains("immutable")));
        let declared_length = segment_zero_response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .expect("finite content length header")
            .parse::<usize>()
            .expect("finite content length value");
        assert_eq!(declared_length, head_content_length);
        let segment_zero = response_body(segment_zero_response).await;
        assert_eq!(segment_zero.len(), declared_length);
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
        assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
        assert_eq!(app_state.hls_proxy.segment_repair().stats().await, repair_before);
        assert_eq!(app_state.active_provider.get_provider_connections_count().await, provider_connections_before);

        let segment_zero_again =
            response_body(get_response(Arc::clone(&app_state), &segment_zero_uri, None).await).await;
        let segment_one = response_body(get_response(Arc::clone(&app_state), &segment_one_uri, None).await).await;
        assert_eq!(segment_zero, segment_zero_again, "same terminal index is immutable");
        assert_ne!(segment_zero, segment_one, "successive terminal indices advance timestamps and continuity");

        let range_response = get_response(Arc::clone(&app_state), &segment_zero_uri, Some("bytes=0-187")).await;
        assert_eq!(range_response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&range_response));
        assert_eq!(range_response.headers()[header::CONTENT_LENGTH], "188");
        let expected_content_range = format!("bytes 0-187/{declared_length}");
        assert_eq!(
            range_response.headers()[header::CONTENT_RANGE].to_str().ok(),
            Some(expected_content_range.as_str())
        );
        assert_eq!(range_response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(range_response.headers()[header::CACHE_CONTROL]
            .to_str()
            .is_ok_and(|value| value.contains("immutable")));
        assert_eq!(response_body(range_response).await, segment_zero.slice(..188));

        let unsatisfiable_range = format!("bytes={declared_length}-");
        let unsatisfiable_response = get_response(app_state, &segment_zero_uri, Some(&unsatisfiable_range)).await;
        assert_eq!(unsatisfiable_response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&unsatisfiable_response));
        let expected_unsatisfied_content_range = format!("bytes */{declared_length}");
        assert_eq!(
            unsatisfiable_response.headers()[header::CONTENT_RANGE].to_str().ok(),
            Some(expected_unsatisfied_content_range.as_str())
        );
        assert!(response_body(unsatisfiable_response).await.is_empty());
        assert_eq!(
            terminal_renderer.finite_hls_render_count(),
            renders_before_requests,
            "terminal HTTP serving must not invoke the TS writer again"
        );
        assert_eq!(
            terminal_renderer.finite_hls_finalize_count(),
            finalizations_before_requests,
            "terminal HTTP serving must not invoke lease-specific TS finalization again"
        );
    }

    async fn assert_runtime_policy_terminal_segments(
        fixture: &RuntimePolicyEndpointFixture,
        terminal_prefix: &str,
        segment_count: u16,
    ) {
        for index in [0, 1, segment_count.saturating_sub(1)] {
            let uri = format!("{terminal_prefix}{index}.ts");
            let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
            assert_eq!(response.status(), StatusCode::OK, "terminal segment {index}");
            assert!(!response_body(response).await.is_empty());
        }
    }

    #[tokio::test]
    async fn resource_access_denial_commits_and_preserves_user_exhausted_tail() {
        let fixture = runtime_policy_endpoint_fixture(true).await;
        let session = fixture
            .app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&fixture.proxy_session_id)
            .await
            .expect("runtime policy session");
        let origin_refresh_before = session.read().await.origin_refresh.clone();
        mark_hls_user_session_exhausted(&fixture.app_state).await;

        let denied_live = get_response(Arc::clone(&fixture.app_state), &fixture.live_segment_uri, None).await;
        assert_eq!(denied_live.status(), StatusCode::FORBIDDEN);
        let revoking = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("revoking lease snapshot");
        assert_eq!(revoking.state, HlsAccessLeaseState::PolicyRevoking);
        assert_eq!(revoking.playback_mode, HlsLeasePlaybackMode::Live);
        assert_eq!(
            revoking.runtime_policy_revocation.as_ref().map(|revocation| revocation.reason),
            Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted)
        );

        let pending_manifest = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(pending_manifest.status(), StatusCode::SERVICE_UNAVAILABLE);
        let committed_plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
        let manifest_response = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(manifest_response.status(), StatusCode::OK);
        assert!(!manifest_response.headers().contains_key(header::LOCATION));
        let manifest = String::from_utf8(response_body(manifest_response).await.to_vec())
            .expect("runtime policy terminal manifest utf8");
        assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
        assert!(!manifest.contains("/cvs/hls/"));

        let committed = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("committed runtime policy lease");
        assert_eq!(committed.state, HlsAccessLeaseState::Denied);
        let HlsLeasePlaybackMode::TerminalTail(plan) = committed.playback_mode else {
            panic!("resource denial must commit a finite terminal plan");
        };
        assert_eq!(plan.generation, committed_plan.generation);
        assert_eq!(plan.reason, HlsRuntimeCustomTailReason::UserConnectionsExhausted);
        assert!(plan.segment_count >= 2);
        let terminal_prefix = format!(
            "/hls/shared/live/{}/{}/terminal/{}/",
            fixture.proxy_session_id.0, fixture.lease_id.0, plan.generation.0
        );
        assert_eq!(manifest.matches(&terminal_prefix).count(), usize::from(plan.segment_count));

        assert_runtime_policy_terminal_segments(&fixture, &terminal_prefix, plan.segment_count).await;
        let replay = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(
            String::from_utf8(response_body(replay).await.to_vec()).expect("replayed runtime policy manifest utf8"),
            manifest
        );
        assert_eq!(get_status(Arc::clone(&fixture.app_state), &fixture.live_segment_uri).await, StatusCode::FORBIDDEN);
        let retained = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("retained runtime policy plan");
        assert!(matches!(
            retained.playback_mode,
            HlsLeasePlaybackMode::TerminalTail(ref current)
                if current.generation == plan.generation
                    && current.reason == HlsRuntimeCustomTailReason::UserConnectionsExhausted
        ));
        assert_eq!(session.read().await.origin_refresh, origin_refresh_before);
    }

    #[tokio::test]
    async fn manifest_touch_denied_replays_policy_tail_instead_of_standalone_clock() {
        let fixture = runtime_policy_endpoint_fixture(true).await;
        let _ = fixture
            .app_state
            .hls_proxy
            .begin_runtime_policy_revocation(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                super::current_time_millis(),
            )
            .await;
        assert_eq!(
            fixture
                .app_state
                .hls_proxy
                .touch_manifest_access_lease(
                    &fixture.lease_id,
                    &fixture.proxy_session_id,
                    super::current_time_millis(),
                    None,
                    None,
                    60_000,
                )
                .await,
            HlsAccessLeaseTouch::Denied
        );

        let pending = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
        let committed_plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
        let response = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("policy touch manifest utf8");
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("touch-denied lease");
        let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode else {
            panic!("touch denial must retain a lease-bound terminal plan");
        };
        assert_eq!(plan.generation, committed_plan.generation);
        assert!(body.contains(&format!(
            "/hls/shared/live/{}/{}/terminal/{}/0.ts",
            fixture.proxy_session_id.0, fixture.lease_id.0, plan.generation.0
        )));
        assert!(!body.contains("/cvs/hls/"));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[tokio::test]
    async fn cold_user_denial_uses_standalone_finite_response() {
        let fixture = runtime_policy_endpoint_fixture(false).await;
        mark_hls_user_session_exhausted(&fixture.app_state).await;

        let denial = get_response(Arc::clone(&fixture.app_state), &fixture.live_segment_uri, None).await;
        assert_eq!(denial.status(), StatusCode::FORBIDDEN);
        let denied = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("cold denied lease");
        assert_eq!(denied.state, HlsAccessLeaseState::Denied);
        assert_eq!(denied.playback_mode, HlsLeasePlaybackMode::Ended);
        assert!(denied.runtime_policy_revocation.is_none());
        assert_eq!(denied.runtime_policy_denial_reason(), Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted));

        let response = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("standalone policy manifest utf8");
        assert!(body.contains(&format!("/cvs/hls/{}/", fixture.lease_id.0)));
        assert!(!body.contains("/hls-user/"));
        assert!(!body.contains("/hls-pass/"));
        assert!(!body.contains("/user_connections_exhausted/"));
        assert!(!body
            .contains(&format!("/hls/shared/live/{}/{}/terminal/", fixture.proxy_session_id.0, fixture.lease_id.0)));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[tokio::test]
    async fn hls_terminal_response_body_after_lease_denial_does_not_extend_shared_session_activity() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
        let lease_id = format!("test-access-lease-{proxy_session_id}");
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
        let (generation, _) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("terminal session");
        let segment_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.ts");
        let response = get_response(Arc::clone(&app_state), &segment_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let activity_after_authorized_response =
            session.read().await.activity.last_authorized_media_at_ms.expect("terminal GET marks media activity");

        let _ = app_state
            .hls_proxy
            .deny_access_lease(
                &HlsAccessLeaseId(lease_id),
                tuliprox_hls::HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,
            )
            .await;
        assert!(!response_body(response).await.is_empty());

        assert_eq!(
            session.read().await.activity.last_authorized_media_at_ms,
            Some(activity_after_authorized_response),
            "body completion after denial must not extend activity"
        );
    }

    #[tokio::test]
    async fn hls_terminal_response_rejects_stale_malformed_and_out_of_bounds_paths() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
        let lease_id = format!("test-access-lease-{proxy_session_id}");
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
        let (generation, segment_count) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
        let stale_generation = generation.saturating_add(1);
        let stale_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{stale_generation}/0.ts");
        let out_of_bounds_uri =
            format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/{segment_count}.ts");

        let malformed_generation = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/01/0.ts");
        let non_numeric_generation =
            format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/not-a-generation/0.ts");
        let overflowing_generation =
            format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/18446744073709551616/0.ts");
        let malformed_file = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/00.ts");
        let wrong_extension = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.m4s");

        for uri in [
            stale_uri,
            out_of_bounds_uri,
            malformed_generation,
            non_numeric_generation,
            overflowing_generation,
            malformed_file,
            wrong_extension,
        ] {
            let response = get_response(Arc::clone(&app_state), &uri, None).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
    }

    #[tokio::test]
    async fn expired_route_replays_already_committed_custom_tail() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
        let lease_id = format!("test-access-lease-{proxy_session_id}");
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
        let (generation, _) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
        let proxy_session_key = ProxySessionId(proxy_session_id.clone());
        let lease_key = HlsAccessLeaseId(lease_id.clone());
        let expired_at_ms = super::current_time_millis().saturating_sub(1);
        {
            let mut leases = app_state.hls_proxy.access_leases().write().await;
            let mut lease = leases.remove_access_lease(&lease_key).expect("terminal lease exists");
            lease.valid_until_ms = expired_at_ms;
            leases.prepare_access_lease(lease);
        }
        let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/manifest.m3u8");
        let segment_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.ts");

        let manifest_response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;
        let segment_response = get_response(Arc::clone(&app_state), &segment_uri, None).await;

        assert_eq!(manifest_response.status(), StatusCode::OK);
        assert!(!manifest_response.headers().contains_key(header::LOCATION));
        assert!(String::from_utf8(response_body(manifest_response).await.to_vec())
            .expect("expired committed manifest utf8")
            .ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(segment_response.status(), StatusCode::OK);
        assert!(!response_body(segment_response).await.is_empty());
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_key)
            .await
            .expect("terminal session exists");
        assert!(session.read().await.terminal_tail_protection(&lease_key).is_some());

        let cleanup_at_ms = super::current_time_millis();
        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: lease_key.clone(),
                        proxy_session_id: proxy_session_key,
                    },
                    due_at_ms: cleanup_at_ms,
                },
                cleanup_at_ms,
            )
            .await;

        assert!(!session.read().await.has_terminal_tail_protections());
        assert!(app_state
            .hls_proxy
            .access_lease_response_snapshot(&lease_key, &ProxySessionId(proxy_session_id), cleanup_at_ms)
            .await
            .is_none());
        assert_eq!(get_response(Arc::clone(&app_state), &manifest_uri, None).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get_response(app_state, &segment_uri, None).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn expired_route_without_session_evidence_returns_not_found_instead_of_unanchored_manifest() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"live-without-lease").await;
        enable_channel_unavailable_custom_response(&app_state);
        let missing_lease = "expired-without-base-evidence";
        let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{missing_lease}/manifest.m3u8");

        let response = get_response(app_state, &manifest_uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert!(response_body(response).await.is_empty());
    }

    #[tokio::test]
    async fn hls_terminal_response_normal_segment_map_and_resource_routes_never_serve_terminal_fallbacks() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
        let lease_id = format!("test-access-lease-{proxy_session_id}");
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
        let normal_segment_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/000123.ts");

        let normal_segment_response = get_response(Arc::clone(&app_state), &normal_segment_uri, None).await;

        assert_eq!(normal_segment_response.status(), StatusCode::OK);
        assert!(!normal_segment_response.headers().contains_key(header::LOCATION));
        assert_eq!(response_body(normal_segment_response).await, bytes::Bytes::from_static(b"original-live-tail"));

        let map_temp_dir = tempfile::tempdir().expect("map tempdir");
        let map_app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(map_temp_dir.path(), 300)));
        let map_proxy_session_id = map_hls_map(&map_app_state, b"original-map", true).await;
        let map_lease_id = format!("test-access-lease-{map_proxy_session_id}");
        let map_session = map_app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(map_proxy_session_id.clone()))
            .await
            .expect("map session exists");
        let map_base_proxy_seq = *map_session.read().await.segments.keys().next().expect("map manifest has media");
        terminalize_existing_test_lease(&map_app_state, &map_proxy_session_id, &map_lease_id, map_base_proxy_seq).await;
        let map_uri = format!("/hls/shared/live/{map_proxy_session_id}/{map_lease_id}/map/000000.mp4");
        let map_response = get_response(map_app_state, &map_uri, None).await;
        assert_eq!(map_response.status(), StatusCode::NOT_FOUND);
        assert!(!map_response.headers().contains_key(header::LOCATION));
        assert!(response_body(map_response).await.is_empty());

        let resource_temp_dir = tempfile::tempdir().expect("resource tempdir");
        let resource_app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(
            resource_temp_dir.path(),
            300,
        )));
        let (resource_proxy_session_id, resource_id) =
            map_transient_resource(&resource_app_state, "http://origin.example.com/old.ts", "ts", true).await;
        let resource_lease_id = format!("test-access-lease-{resource_proxy_session_id}");
        terminalize_existing_test_lease(&resource_app_state, &resource_proxy_session_id, &resource_lease_id, 0).await;
        let resource_uri =
            format!("/hls/shared/live/{resource_proxy_session_id}/{resource_lease_id}/r/{resource_id}.ts");
        let resource_response = get_response(resource_app_state, &resource_uri, None).await;
        assert_eq!(resource_response.status(), StatusCode::NOT_FOUND);
        assert!(!resource_response.headers().contains_key(header::LOCATION));
        assert!(response_body(resource_response).await.is_empty());
    }

    #[tokio::test]
    async fn delayed_live_resource_completion_revalidates_after_terminal_cutover_without_a_sleep() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"ready-before-cutover").await;
        let lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
        let proxy_session_key = ProxySessionId(proxy_session_id.clone());
        let access_context = test_hls_access_context(proxy_session_key.clone(), lease_id.clone());
        let live_identity = app_state
            .hls_proxy
            .access_lease_response_snapshot(&lease_id, &proxy_session_key, super::current_time_millis())
            .await
            .and_then(|lease| lease.media_identity())
            .expect("live lease identity");
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        let app_state_for_completion = Arc::clone(&app_state);
        let access_context_for_completion = access_context.clone();
        let completion = tokio::spawn(async move {
            let _ = release_receiver.await;
            super::hls_live_lease_identity_is_current(
                &app_state_for_completion,
                &access_context_for_completion,
                live_identity,
            )
            .await
        });

        terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id.0, 123).await;
        release_sender.send(()).expect("release delayed completion after cutover");

        assert!(!completion.await.expect("controlled completion task"));
    }

    async fn prepare_other_live_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> HlsAccessLeaseId {
        let lease_id = HlsAccessLeaseId("other-live-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                60_000,
            ))
            .await;
        lease_id
    }

    fn assert_terminal_plan_unchanged(
        playback_mode: &HlsLeasePlaybackMode,
        expected_generation: HlsTerminalTailGeneration,
        terminal_path: HlsTerminalSegmentPath,
        expected_bytes: &bytes::Bytes,
    ) {
        let HlsLeasePlaybackMode::TerminalTail(plan) = playback_mode else {
            panic!("shared lease operation cannot reactivate the terminal lease");
        };
        assert_eq!(plan.generation, expected_generation);
        assert_eq!(plan.segment_bytes(terminal_path).as_ref(), Some(expected_bytes));
    }

    async fn assert_conflicted_standalone_fallback(
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
    ) {
        let response =
            super::hls_unpublished_lease_channel_unavailable_response(app_state, proxy_session_id, lease_id).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("standalone manifest utf8");
        assert!(body.contains("#EXT-X-ENDLIST"));
        assert!(
            !body.contains("/hls/shared/live/"),
            "standalone response must not expose a normal segment without a readiness path"
        );
        assert_eq!(
            session.read().await.origin_control.path_condition,
            HlsOriginPathCondition::AcceptanceConflict,
            "lease-local fallback must not relax deterministic conflict evidence"
        );
    }

    #[tokio::test]
    async fn reused_conflicted_session_standalone_fallback_preserves_terminal_lease_during_recovery() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        enable_channel_unavailable_custom_response(&app_state);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
        let terminal_lease_id = format!("test-access-lease-{proxy_session_id}");
        terminalize_existing_test_lease(&app_state, &proxy_session_id, &terminal_lease_id, 123).await;
        let proxy_session_key = ProxySessionId(proxy_session_id.clone());
        let now_ms = super::current_time_millis();
        let terminal_before = app_state
            .hls_proxy
            .access_lease_response_snapshot(&HlsAccessLeaseId(terminal_lease_id.clone()), &proxy_session_key, now_ms)
            .await
            .expect("terminal lease exists before the other lease");
        let HlsLeasePlaybackMode::TerminalTail(terminal_plan_before) = terminal_before.playback_mode else {
            panic!("original lease is terminal");
        };
        let terminal_path = HlsTerminalSegmentPath { generation: terminal_plan_before.generation, index: 0 };
        let terminal_bytes_before =
            terminal_plan_before.segment_bytes(terminal_path).expect("terminal segment zero is immutable");
        let other_lease_id = prepare_other_live_lease(&app_state, &proxy_session_key, now_ms).await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_key)
            .await
            .expect("shared session exists");
        session.write().await.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;

        assert_conflicted_standalone_fallback(&app_state, &session, &proxy_session_key, &other_lease_id).await;

        let terminal_after_fallback = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId(terminal_lease_id.clone()),
                &proxy_session_key,
                now_ms.saturating_add(1),
            )
            .await
            .expect("terminal lease remains stored after standalone fallback");
        assert_terminal_plan_unchanged(
            &terminal_after_fallback.playback_mode,
            terminal_plan_before.generation,
            terminal_path,
            &terminal_bytes_before,
        );

        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &other_lease_id,
                &proxy_session_key,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 5_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());

        let recovered_manifest =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:124\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n124.ts\n");
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&recovered_manifest).expect("recovery manifest commits");
            session.origin_control.record_media_progress(now_ms.saturating_add(1), 4_000);
        }

        let terminal = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId(terminal_lease_id),
                &proxy_session_key,
                now_ms.saturating_add(2),
            )
            .await
            .expect("terminal lease remains stored");
        let other = app_state
            .hls_proxy
            .access_lease_response_snapshot(&other_lease_id, &proxy_session_key, now_ms.saturating_add(2))
            .await
            .expect("other live lease remains stored");
        assert_terminal_plan_unchanged(
            &terminal.playback_mode,
            terminal_plan_before.generation,
            terminal_path,
            &terminal_bytes_before,
        );
        assert_eq!(other.playback_mode, HlsLeasePlaybackMode::Live);
    }

    #[test]
    fn hls_custom_video_manifest_uses_live_six_segment_window_for_provisioning() {
        let user = hls_custom_video_test_user();
        let manifest = build_hls_custom_video_manifest_body(
            "https://example.test/iptv",
            &user,
            CustomVideoStreamType::Provisioning,
        )
        .expect("provisioning manifest does not depend on one looping asset duration");

        assert!(manifest.contains("#EXT-X-TARGETDURATION:2"));
        assert!(manifest.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(manifest.contains("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n"));
        assert!(!manifest.contains("#EXT-X-DISCONTINUITY-SEQUENCE"));
        assert!(!manifest.contains("#EXT-X-SESSION-DATA"));
        assert!(!manifest.contains("#EXT-X-ENDLIST"));
        assert!(!manifest.contains("#EXT-X-DISCONTINUITY\n"));
        assert_eq!(manifest.matches("#EXTINF:2.000000,").count(), 6);
        for index in 0..6 {
            assert!(manifest
                .contains(&format!("https://example.test/iptv/cvs/hls/viewer/secret/provisioning_{index:03}.ts")));
        }
        assert!(!manifest.contains("https://example.test/iptv/cvs/hls/viewer/secret/provisioning.ts"));
        assert_eq!(
            crate::api::model::hls_panel_provisioning_manifest_path(&user, 80510),
            "/cvs/hls/viewer/secret/provisioning.m3u8?id=80510"
        );
        assert!(!manifest.contains("provisioning.ts?"));
        assert!(!manifest.contains("virtual_id"));
        assert!(!manifest.contains("&seq="));
    }

    #[test]
    fn hls_response_uses_rfc8216_content_type_and_remains_tower_compressible() {
        let response = super::hls_response("#EXTM3U\n".to_string()).into_response();

        assert_eq!(response.headers().get(header::CONTENT_TYPE).unwrap(), "application/vnd.apple.mpegurl");
        assert!(tuliprox_core::utils::response_compression::should_compress_response(&response));
    }

    #[test]
    fn virtual_hls_entry_path_uses_single_manifest_extension() {
        let user = hls_custom_video_test_user();
        let xtream_target = ConfigTarget::from(&shared::model::ConfigTargetDto {
            output: vec![shared::model::TargetOutputDto::Xtream(shared::model::XtreamTargetOutputDto::default())],
            ..Default::default()
        });
        let xtream_input = ConfigInput { input_type: InputType::Xtream, ..ConfigInput::default() };
        let m3u_target = ConfigTarget::from(&shared::model::ConfigTargetDto::default());
        let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };

        let xtream_path = super::build_virtual_hls_entry_path(&xtream_target, &xtream_input, &user, 59);
        let m3u_path = super::build_virtual_hls_entry_path(&m3u_target, &m3u_input, &user, 59);

        assert_eq!(xtream_path, "/live/viewer/secret/59.m3u8");
        assert_eq!(m3u_path, "/m3u-stream/live/viewer/secret/59.m3u8");
        assert!(!xtream_path.contains("..m3u8"));
        assert!(!m3u_path.contains("..m3u8"));
    }

    #[test]
    fn hls_manifest_headers_apply_disabled_headers_and_default_user_agent_policy() {
        let mut input_headers = HashMap::new();
        input_headers.insert("User-Agent".to_string(), "Input-UA".to_string());
        input_headers.insert("Accept-Language".to_string(), "de".to_string());
        input_headers.insert("Accept-Encoding".to_string(), "gzip".to_string());
        input_headers.insert("Authorization".to_string(), "Bearer input-secret".to_string());
        input_headers.insert("X-Origin-Secret".to_string(), "input-secret".to_string());

        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        request_headers.insert(header::USER_AGENT, HeaderValue::from_static("Client-UA"));
        request_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer client-secret"));
        request_headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        request_headers
            .insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        request_headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
        request_headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("client"));
        request_headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));

        let disabled = ReverseProxyDisabledHeaderConfig {
            referer_header: false,
            x_header: true,
            cloudflare_header: true,
            custom_header: vec!["X-Origin-Secret".to_string()],
        };
        let headers = build_hls_manifest_request_headers(
            &input_headers,
            &request_headers,
            Some(&disabled),
            Some("Default-UA"),
            Some("Channel-UA"),
        );

        assert_eq!(headers.get(header::USER_AGENT).expect("user agent"), "Channel-UA");
        assert_eq!(headers.get("accept-language").expect("accept language"), "de");
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("accept encoding"), "identity");
        assert!(!headers.contains_key(header::RANGE));
        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key(header::HOST));
        assert!(!headers.contains_key("x-origin-secret"));
        assert!(!headers.contains_key("x-blocked"));
        assert!(!headers.contains_key("cf-ray"));
    }

    #[tokio::test]
    async fn legacy_hls_manifest_decodes_supported_origin_codings_and_enforces_identity() {
        const MANIFEST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nsegment.ts\n";

        for coding in ["gzip", "deflate", "br", "zstd"] {
            let encoded = encode_test_manifest(coding, MANIFEST.as_bytes()).await;
            let origin = spawn_test_encoded_manifest_origin(Some(coding), encoded, Duration::ZERO).await;
            let input = legacy_manifest_test_input(&origin);
            let client_headers = legacy_manifest_test_client_headers();

            let (manifest, final_url, _) =
                super::download_legacy_hls_manifest(&test_app_state(), &input, &client_headers)
                    .await
                    .unwrap_or_else(|error| panic!("{coding} manifest should decode: {error}"));

            assert_eq!(manifest, MANIFEST, "coding={coding}");
            assert_eq!(final_url, input.url, "coding={coding}");
            let requests = origin.requests.lock().await;
            assert_eq!(requests.len(), 1, "coding={coding}");
            assert!(
                requests[0].to_ascii_lowercase().contains("\r\naccept-encoding: identity\r\n"),
                "coding={coding}, request={}",
                requests[0]
            );
        }
    }

    #[tokio::test]
    async fn legacy_hls_manifest_handles_identity_and_headerless_gzip_magic() {
        const MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n";
        let cases = [("identity", MANIFEST.to_vec()), ("gzip-magic", encode_test_manifest("gzip", MANIFEST).await)];

        for (case, body) in cases {
            let origin = spawn_test_encoded_manifest_origin(None, body, Duration::ZERO).await;
            let input = legacy_manifest_test_input(&origin);

            let (manifest, _, _) =
                super::download_legacy_hls_manifest(&test_app_state(), &input, &legacy_manifest_test_client_headers())
                    .await
                    .unwrap_or_else(|error| panic!("{case} manifest should decode: {error}"));

            assert_eq!(manifest.as_bytes(), MANIFEST, "case={case}");
        }
    }

    #[tokio::test]
    async fn legacy_hls_manifest_limit_applies_after_decompression() {
        let decoded = vec![b'x'; MAX_HLS_MANIFEST_BYTES + 1];
        let origin = spawn_test_encoded_manifest_origin(
            Some("gzip"),
            encode_test_manifest("gzip", &decoded).await,
            Duration::ZERO,
        )
        .await;
        let input = legacy_manifest_test_input(&origin);

        let error =
            super::download_legacy_hls_manifest(&test_app_state(), &input, &legacy_manifest_test_client_headers())
                .await
                .expect_err("decoded manifest above limit must fail");

        assert!(matches!(
            error.get_ref().and_then(|source| source.downcast_ref()),
            Some(crate::utils::content_coding::ContentBodyReadError::LimitExceeded { limit })
                if *limit == MAX_HLS_MANIFEST_BYTES
        ));
    }

    #[tokio::test]
    async fn legacy_hls_manifest_deadline_includes_full_body_read() {
        let origin = spawn_test_encoded_manifest_origin(None, b"#EXTM3U\n".to_vec(), Duration::from_millis(100)).await;
        let input = legacy_manifest_test_input(&origin);
        let hls_config = HlsCacheConfig::from(&HlsCacheConfigDto {
            origin_manifest_timeout_ms: shared::model::Millis::new(10),
            ..Default::default()
        });
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));

        let error = super::download_legacy_hls_manifest(&app_state, &input, &legacy_manifest_test_client_headers())
            .await
            .expect_err("complete manifest body read must honor the deadline");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn legacy_hls_manifest_distinguishes_invalid_utf8_from_decoder_failure() {
        let invalid_utf8_origin = spawn_test_encoded_manifest_origin(None, vec![0xff], Duration::ZERO).await;
        let invalid_utf8_input = legacy_manifest_test_input(&invalid_utf8_origin);
        let invalid_utf8 = super::download_legacy_hls_manifest(
            &test_app_state(),
            &invalid_utf8_input,
            &legacy_manifest_test_client_headers(),
        )
        .await
        .expect_err("invalid UTF-8 must fail");

        let corrupt_origin =
            spawn_test_encoded_manifest_origin(Some("gzip"), vec![0x1f, 0x8b, 0x08, 0x00], Duration::ZERO).await;
        let corrupt_input = legacy_manifest_test_input(&corrupt_origin);
        let decoder_failure = super::download_legacy_hls_manifest(
            &test_app_state(),
            &corrupt_input,
            &legacy_manifest_test_client_headers(),
        )
        .await
        .expect_err("corrupt gzip must fail");

        assert_eq!(invalid_utf8.kind(), std::io::ErrorKind::InvalidData);
        assert!(crate::utils::content_coding::content_decoding_error_from_io(&decoder_failure).is_some());
    }

    #[test]
    fn hls_proxy_public_path_prefix_rewrites_only_proxy_hls_uri_surfaces() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy-id/r/key.key\",IV=0x1\n",
            "#EXT-X-MAP:URI=\"/hls/shared/live/proxy-id/map/000000.mp4\",BYTERANGE=\"10@0\"\n",
            "#EXT-X-PART:DURATION=1.0,URI=\"/hls/shared/live/proxy-id/r/part.m4s\"\n",
            "#EXT-X-MEDIA-SEQUENCE:7\n",
            "#EXTINF:4.0,\n",
            "/hls/shared/live/proxy-id/000007.ts\n",
            "https://origin.example.com/not-proxy.ts\n",
        );

        let prefixed = super::apply_hls_proxy_public_path_prefix(body.to_string(), Some("/iptv/"));

        assert!(prefixed.contains("URI=\"/iptv/hls/shared/live/proxy-id/r/key.key\""));
        assert!(prefixed.contains("URI=\"/iptv/hls/shared/live/proxy-id/map/000000.mp4\""));
        assert!(prefixed.contains("URI=\"/iptv/hls/shared/live/proxy-id/r/part.m4s\""));
        assert!(prefixed.contains("\n/iptv/hls/shared/live/proxy-id/000007.ts\n"));
        assert!(prefixed.contains("#EXT-X-MEDIA-SEQUENCE:7"));
        assert!(prefixed.contains("https://origin.example.com/not-proxy.ts"));
    }

    #[test]
    fn hls_proxy_public_path_prefix_keeps_body_unchanged_without_server_path() {
        let body = "#EXTM3U\n#EXTINF:4.0,\n/hls/shared/live/proxy-id/000007.ts\n".to_string();

        assert_eq!(super::apply_hls_proxy_public_path_prefix(body.clone(), None), body);
        assert_eq!(super::apply_hls_proxy_public_path_prefix(body.clone(), Some("/")), body);
    }

    #[test]
    fn hls_manifest_materialization_uses_proxy_paths_without_provider_or_legacy_route() {
        let body = format!(
            "#EXTM3U\n#EXTINF:4.0,\n/hls/shared/live/proxy-id/{}/000123.ts\n",
            crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
        );
        let lease_id = HlsAccessLeaseId("access-lease".to_string());

        let materialized = super::materialize_hls_access_manifest(&body, &lease_id, Some("/iptv"));

        assert!(materialized.contains("/iptv/hls/shared/live/proxy-id/access-lease/000123.ts"));
        assert!(!materialized.contains("provider://"));
        assert!(!materialized.contains("/hls/hls-user/"));
        assert!(!materialized.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    }

    #[test]
    fn hls_cache_origin_entry_url_preserves_provider_scheme_as_failover() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://origin.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input = ConfigInput { provider_configs: Some(vec![Arc::clone(&provider)]), ..ConfigInput::default() };

        let origin =
            super::resolve_hls_cache_origin_entry_url(&input, "provider://demo/live/account-a/token-a/1025130.m3u8")
                .expect("provider entry url should resolve");

        assert_eq!(origin.session_entry_url.as_str(), "provider://demo/live/account-a/token-a/1025130.m3u8");
        assert_eq!(
            origin.session_entry_url.url_failover_provider().expect("provider failover config").name.as_ref(),
            "demo"
        );
        let provider_key = super::build_hls_origin_source(&input, "1025130").session_key();
        let direct_key = super::build_hls_origin_source(&input, "1025130").session_key();
        assert_eq!(provider_key, direct_key);
        assert_eq!(provider_key.stable_value(), "input:0|hls|1025130");
        assert!(!provider_key.stable_value().contains("provider://"));
        assert!(!provider_key.stable_value().contains("origin.example.com"));
    }

    #[test]
    fn hls_cache_origin_entry_url_does_not_attach_url_failover_provider_to_http_url() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://origin.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input = ConfigInput { provider_configs: Some(vec![provider]), ..ConfigInput::default() };

        let origin = super::resolve_hls_cache_origin_entry_url(
            &input,
            "http://origin.example.com/live/account-a/token-a/1025130.m3u8",
        )
        .expect("http entry url should resolve");

        assert_eq!(origin.session_entry_url.as_str(), "http://origin.example.com/live/account-a/token-a/1025130.m3u8");
        assert!(origin.session_entry_url.url_failover_provider().is_none());
    }

    #[test]
    fn hls_origin_source_kind_covers_xtream_m3u_and_direct_media_playlist() {
        assert_eq!(super::hls_origin_source_kind(InputType::Xtream), HlsOriginSourceKind::XtreamLive);
        assert_eq!(super::hls_origin_source_kind(InputType::M3u), HlsOriginSourceKind::M3uMediaPlaylist);
        assert_eq!(super::hls_origin_source_kind(InputType::Library), HlsOriginSourceKind::DirectMediaPlaylist);
    }

    #[test]
    fn hls_manifest_extension_helper_does_not_create_double_dot_urls() {
        assert_eq!(
            super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123.m3u8"),
            "http://origin.example.com/live/user/pass/1025123.m3u8"
        );
        assert_eq!(
            super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123..m3u8"),
            "http://origin.example.com/live/user/pass/1025123.m3u8"
        );
        assert_eq!(
            super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123..?token=1"),
            "http://origin.example.com/live/user/pass/1025123.m3u8?token=1"
        );
        assert_eq!(
            super::ensure_hls_manifest_extension("provider://mirror/live/user/pass/1025123..m3u8"),
            "provider://mirror/live/user/pass/1025123.m3u8"
        );
    }

    #[test]
    fn hls_origin_resolution_preserves_legacy_built_xtream_origin_url() {
        let input = ConfigInput {
            id: 7,
            name: Arc::from("xtream"),
            input_type: InputType::Xtream,
            url: "http://origin.example.com/base".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            ..ConfigInput::default()
        };

        let origin =
            super::build_hls_origin_resolution(&input, "http://other.example.com/live/other/creds/1025126.m3u8")
                .expect("xtream origin should resolve");

        assert_eq!(origin.session_entry_url.as_str(), "http://other.example.com/live/other/creds/1025126.m3u8");
        assert_eq!(origin.hls_url, origin.session_entry_url.as_str());
        assert!(origin.session_entry_url.url_failover_provider().is_none());
    }

    #[test]
    fn hls_origin_resolution_keeps_provider_failover_out_of_identity() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "mirror-group".into(),
            urls: vec!["http://mirror-a.example.com".into(), "http://mirror-b.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input = ConfigInput {
            id: 7,
            name: Arc::from("xtream"),
            input_type: InputType::Xtream,
            url: "provider://mirror-group".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            provider_configs: Some(vec![Arc::clone(&provider)]),
            ..ConfigInput::default()
        };

        let origin = super::build_hls_origin_resolution(
            &input,
            "provider://mirror-group/live/source-user/source-pass/1025126.m3u8",
        )
        .expect("provider failover origin should resolve");
        let failover_key = super::build_hls_origin_source(&input, "80510").session_key();
        let direct_key = super::build_hls_origin_source(&input, "80510").session_key();

        assert_eq!(
            origin.session_entry_url.as_str(),
            "provider://mirror-group/live/source-user/source-pass/1025126.m3u8"
        );
        assert!(origin.session_entry_url.url_failover_provider().is_some());
        assert_eq!(failover_key, direct_key);
        assert!(!failover_key.stable_value().contains("provider://"));
        assert!(!failover_key.stable_value().contains("mirror-a.example.com"));
        assert!(!failover_key.stable_value().contains("mirror-b.example.com"));
    }

    #[test]
    fn hls_origin_resolution_uses_m3u_playlist_item_url() {
        let input = ConfigInput {
            id: 9,
            name: Arc::from("m3u"),
            input_type: InputType::M3u,
            url: "http://playlist.example.com/list.m3u".to_string(),
            ..ConfigInput::default()
        };

        let origin = super::build_hls_origin_resolution(&input, "http://media.example.com/live/channel/index.m3u8")
            .expect("m3u hls origin should resolve");
        let source = super::build_hls_origin_source(&input, "stable-item");

        assert_eq!(origin.session_entry_url.as_str(), "http://media.example.com/live/channel/index.m3u8");
        assert_eq!(source.source_kind, HlsOriginSourceKind::M3uMediaPlaylist);
        assert_eq!(source.session_key().stable_value(), "input:9|hls|stable-item");
    }

    #[test]
    fn provider_failover_mirror_change_keeps_same_hls_session_identity() {
        let provider_a = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "mirror-group".into(),
            urls: vec!["http://mirror-a.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let provider_b = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "mirror-group".into(),
            urls: vec!["http://mirror-b.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input_a = ConfigInput {
            id: 7,
            name: Arc::from("xtream"),
            input_type: InputType::Xtream,
            url: "provider://mirror-group".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            provider_configs: Some(vec![provider_a]),
            ..ConfigInput::default()
        };
        let input_b = ConfigInput { provider_configs: Some(vec![provider_b]), ..input_a.clone() };

        let origin_a = super::build_hls_origin_resolution(&input_a, "provider://mirror-group/a.m3u8")
            .expect("provider failover origin a should resolve");
        let origin_b = super::build_hls_origin_resolution(&input_b, "provider://mirror-group/b.m3u8")
            .expect("provider failover origin b should resolve");
        let key_a = super::build_hls_origin_source(&input_a, "80510").session_key();
        let key_b = super::build_hls_origin_source(&input_b, "80510").session_key();
        let secret = b"rewrite-secret";

        assert!(origin_a.session_entry_url.url_failover_provider().is_some());
        assert!(origin_b.session_entry_url.url_failover_provider().is_some());
        assert_eq!(key_a, key_b);
        assert_eq!(key_a.stable_value(), "input:7|hls|80510");
        assert_eq!(build_proxy_session_id(&key_a, secret), build_proxy_session_id(&key_b, secret));
        assert!(!key_a.stable_value().contains("provider://"));
        assert!(!key_a.stable_value().contains("mirror-a.example.com"));
        assert!(!key_a.stable_value().contains("mirror-b.example.com"));
    }

    #[test]
    fn hls_runtime_origin_fetch_url_uses_selected_provider_account() {
        let input = ConfigInput {
            name: Arc::from("source"),
            url: "http://source.example.com".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let provider_input = ConfigInput {
            id: 7,
            name: Arc::from("selected-provider"),
            url: "http://provider.example.com".to_string(),
            username: Some("provider-user".to_string()),
            password: Some("provider-pass".to_string()),
            input_type: InputType::Xtream,
            max_connections: 1,
            ..ConfigInput::default()
        };
        let provider = Arc::new(RuntimeProviderConfig::new(
            &provider_input,
            Arc::new(tokio::sync::RwLock::new(ProviderConfigConnection::default())),
            Arc::new(|_, _| {}),
        ));

        let fetch_url = super::build_hls_origin_fetch_url(
            &input,
            "http://source.example.com/live/source-user/source-pass/12345.m3u8",
            "http://source.example.com/live/source-user/source-pass/12345.m3u8",
            Some(&provider),
        )
        .expect("fetch url should be rewritten");

        assert_eq!(fetch_url, "http://provider.example.com/live/provider-user/provider-pass/12345.m3u8");

        let failover_provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://mirror-a.example.com".into(), "http://mirror-b.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let provider_scheme_input = ConfigInput {
            name: Arc::from("source"),
            url: "provider://demo".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            provider_configs: Some(vec![Arc::clone(&failover_provider)]),
            ..ConfigInput::default()
        };
        let provider_scheme_without_account_rewrite = super::build_hls_origin_fetch_url(
            &provider_scheme_input,
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            None,
        )
        .expect("provider scheme fetch url should remain failover capable");
        assert_eq!(provider_scheme_without_account_rewrite, "provider://demo/live/source-user/source-pass/12345.m3u8");

        let provider_scheme_fetch_url = super::build_hls_origin_fetch_url(
            &provider_scheme_input,
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            Some(&provider),
        )
        .expect("provider scheme fetch url should use selected runtime account without losing failover");

        assert_eq!(provider_scheme_fetch_url, "provider://demo/live/provider-user/provider-pass/12345.m3u8");
        let failover_context = super::hls_url_failover_provider_for_origin_context(
            &provider_scheme_input,
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            &provider_scheme_fetch_url,
        )
        .expect("provider failover context");
        assert_eq!(failover_context.name.as_ref(), "demo");
    }

    #[test]
    fn hls_origin_entry_attaches_url_failover_provider_only_to_provider_scheme_fetch_url() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://mirror.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));

        let http_provider = super::effective_hls_url_failover_provider_for_fetch_url(
            "http://provider.example.com/live/user/pass/12345.m3u8",
            None,
            Some(Arc::clone(&provider)),
        );
        let http_entry = super::LiveHlsOriginEntry::parse_with_url_failover_provider(
            "http://provider.example.com/live/user/pass/12345.m3u8",
            http_provider,
        )
        .expect("http origin entry");
        assert!(http_entry.url_failover_provider().is_none());

        let provider_scheme_provider = super::effective_hls_url_failover_provider_for_fetch_url(
            "provider://demo/live/user/pass/12345.m3u8",
            None,
            Some(Arc::clone(&provider)),
        );
        let provider_entry = super::LiveHlsOriginEntry::parse_with_url_failover_provider(
            "provider://demo/live/user/pass/12345.m3u8",
            provider_scheme_provider,
        )
        .expect("provider origin entry");
        assert_eq!(provider_entry.url_failover_provider().expect("provider").name.as_ref(), "demo");
    }

    #[tokio::test]
    async fn hls_access_lease_validity_uses_session_idle_timeout_not_cache_duration() {
        let hls_dto = HlsCacheConfigDto {
            cache_duration: shared::model::Secs::new(900),
            session_idle_timeout: shared::model::Secs::new(42),
            ..Default::default()
        };
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));

        assert_eq!(super::hls_access_lease_ttl_ms(&app_state), 42_000);
    }

    #[tokio::test]
    async fn hls_access_lease_active_window_uses_two_target_durations() {
        let app_state = test_app_state();
        let key = HlsSessionKey::new(1, "access-window-stream");
        let (session, _) =
            app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", super::current_time_millis()).await;
        session.write().await.target_duration = Some(11);

        let timing = super::hls_access_lease_timing_for_session(&app_state, &session).await;

        assert_eq!(timing.active_window_ms, 22_000);
        assert_eq!(timing.valid_window_ms, super::hls_access_lease_ttl_ms(&app_state));
    }

    #[tokio::test]
    async fn hls_lifecycle_active_timer_moves_access_lease_to_idle() {
        let app_state = test_app_state();
        let now_ms = super::current_time_millis();
        let lease_id = HlsAccessLeaseId("lifecycle-lease".to_string());
        let key = HlsSessionKey::new(1, "lifecycle-stream");
        let (session, _) = app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();

        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "lifecycle-stream".to_string(),
                123,
                now_ms,
                60_000,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 1, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        session.write().await.activity.active_access_lease_count = 1;

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseActive {
                        lease_id: lease_id.clone(),
                        proxy_session_id: proxy_session_id.clone(),
                    },
                    due_at_ms: now_ms.saturating_add(1),
                },
                now_ms.saturating_add(2),
            )
            .await;

        assert_eq!(
            app_state.hls_proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
            Some(HlsAccessLeaseState::Idle)
        );
        assert_eq!(session.read().await.activity.active_access_lease_count, 0);
    }

    #[tokio::test]
    async fn hls_lifecycle_validity_timer_removes_expired_access_lease() {
        let mut hls_cache = HlsCacheConfigDto::default();
        hls_cache.segment_repair.max_level = HlsSegmentRepairMode::Low;
        let hls_config = HlsCacheConfig::from(&hls_cache);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        let now_ms = super::current_time_millis();
        let proxy_session_id = ProxySessionId("lifecycle-validity-proxy".to_string());
        let lease_id = HlsAccessLeaseId("lifecycle-validity-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "lifecycle-stream".to_string(),
                123,
                now_ms,
                1,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 1, valid_window_ms: 1 },
            )
            .await
            .is_activated());
        let repair_before = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_before.windows, 1);
        assert_eq!(repair_before.generations, 1);

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: lease_id.clone(),
                        proxy_session_id: proxy_session_id.clone(),
                    },
                    due_at_ms: now_ms.saturating_add(1),
                },
                now_ms.saturating_add(2),
            )
            .await;

        assert_eq!(
            app_state.hls_proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
            None
        );
        let repair_after = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_after.windows, 0);
        assert_eq!(repair_after.generations, 0);
    }

    #[tokio::test]
    async fn hls_lifecycle_validity_timer_removes_expired_pending_access_lease() {
        let app_state = test_app_state();
        let now_ms = super::current_time_millis();
        let key = HlsSessionKey::new(1, "pending-expiry-stream");
        let (session, _) = app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("pending-expiry-lease".to_string());
        let active_lease_id = HlsAccessLeaseId("active-soft-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "pending-expiry-stream".to_string(),
                123,
                now_ms,
                1,
            ))
            .await;
        app_state
            .hls_proxy
            .prepare_access_lease(
                HlsAccessLease::pending(
                    active_lease_id.clone(),
                    HlsPlaybackFamilyKey::new("soft-user", "client"),
                    proxy_session_id.clone(),
                    "soft-user".to_string(),
                    "soft-session-token".to_string(),
                    1,
                    "pending-expiry-stream".to_string(),
                    123,
                    now_ms,
                    60_000,
                )
                .with_origin_acquire_policy(ConnectionKind::Soft, 20),
            )
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &active_lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: lease_id.clone(),
                        proxy_session_id: proxy_session_id.clone(),
                    },
                    due_at_ms: now_ms.saturating_add(1),
                },
                now_ms.saturating_add(2),
            )
            .await;

        assert_eq!(
            app_state.hls_proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
            None
        );
        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 1);
        let effective_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(effective_policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(effective_policy.priority, 20);
    }

    #[tokio::test]
    async fn hls_lifecycle_session_idle_timer_removes_idle_session() {
        let mut hls_dto = HlsCacheConfigDto { session_idle_timeout: shared::model::Secs::new(1), ..Default::default() };
        hls_dto.segment_repair.max_level = HlsSegmentRepairMode::Low;
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        let now_ms = super::current_time_millis();
        let key = HlsSessionKey::new(1, "expired-session");
        let (session, _) =
            app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms.saturating_sub(2_000)).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("session-idle-cleanup-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "expired-session".to_string(),
                123,
                now_ms,
                60_000,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 1);
        assert_eq!(app_state.hls_proxy.segment_repair().stats().await.windows, 1);

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    due_at_ms: now_ms.saturating_sub(1_000),
                },
                now_ms,
            )
            .await;

        assert!(app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.is_none());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 0);
        let repair_after = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_after.windows, 0);
        assert_eq!(repair_after.generations, 0);
    }

    #[tokio::test]
    async fn hls_gc_session_removal_cleans_access_leases_and_repair_state() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut hls_dto = HlsCacheConfigDto {
            cache_path: Some(temp_dir.path().to_string_lossy().into_owned()),
            session_idle_timeout: shared::model::Secs::new(1),
            ..Default::default()
        };
        hls_dto.segment_repair.max_level = HlsSegmentRepairMode::Low;
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        let now_ms = super::current_time_millis();
        let key = HlsSessionKey::new(1, "gc-cleanup-session");
        let (session, _) =
            app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms.saturating_sub(2_000)).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("gc-cleanup-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "gc-cleanup-session".to_string(),
                123,
                now_ms,
                60_000,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 1);
        assert_eq!(app_state.hls_proxy.segment_repair().stats().await.windows, 1);

        let report = app_state.hls_proxy.run_garbage_collection_once(now_ms).await.expect("gc should run");

        assert_eq!(report.sessions_deleted, 1);
        assert!(app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.is_none());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 0);
        let repair_after = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_after.windows, 0);
        assert_eq!(repair_after.generations, 0);
    }

    fn test_app_state() -> Arc<AppState> { test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::new())) }

    fn test_beast_hls_proxy(cache_path: &std::path::Path) -> Arc<HlsProxyManager> {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto {
            cache_path: Some(cache_path.to_string_lossy().into_owned()),
            strip: HlsStripConfigDto { mode: HlsStripMode::Segments, value: 3 },
            max_segments_prefetch: 6,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto { level: HlsManifestRecoveryBurstLevel::Beast },
            ..HlsCacheConfigDto::default()
        });
        Arc::new(HlsProxyManager::from_hls_cache_config(Some(&config)))
    }

    fn disable_custom_stream_response(app_state: &Arc<AppState>) {
        app_state
            .app_config
            .config
            .store(Arc::new(Config { custom_stream_response_enabled: false, ..Default::default() }));
    }

    fn enable_provider_exhausted_custom_response(app_state: &Arc<AppState>) {
        app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: None,
            user_connections_exhausted: None,
            provider_connections_exhausted: Some(test_custom_video_buffer()),
            low_priority_preempted: None,
            user_account_expired: None,
            panel_api_provisioning: None,
            hls_session_or_lease_expired: None,
            panel_api_provisioning_hls_segments: Vec::new(),
        })));
    }

    fn enable_runtime_policy_custom_responses(app_state: &Arc<AppState>) {
        app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: None,
            user_connections_exhausted: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/user_connections_exhausted.ts"
                ))
                .to_vec(),
            )),
            provider_connections_exhausted: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/provider_connections_exhausted.ts"
                ))
                .to_vec(),
            )),
            low_priority_preempted: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/low_priority_preempted.ts"
                ))
                .to_vec(),
            )),
            user_account_expired: None,
            panel_api_provisioning: None,
            hls_session_or_lease_expired: None,
            panel_api_provisioning_hls_segments: Vec::new(),
        })));
    }

    fn enable_channel_unavailable_custom_response(app_state: &Arc<AppState>) {
        let config = app_state.app_config.config.load();
        app_state
            .app_config
            .config
            .store(Arc::new(Config { custom_stream_response_enabled: true, ..config.as_ref().clone() }));
        app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: Some(test_custom_video_buffer()),
            user_connections_exhausted: None,
            provider_connections_exhausted: None,
            low_priority_preempted: None,
            user_account_expired: None,
            panel_api_provisioning: None,
            hls_session_or_lease_expired: None,
            panel_api_provisioning_hls_segments: Vec::new(),
        })));
    }

    fn test_custom_video_buffer() -> TransportStreamBuffer {
        TransportStreamBuffer::new(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .to_vec(),
        )
    }

    fn enable_hls_provisioning_custom_response(app_state: &Arc<AppState>) {
        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        let provisioning_segments = (0..6)
            .map(|index| {
                let mut packet = ts_packet.clone();
                packet[1] = u8::try_from(index).expect("test index fits");
                TransportStreamBuffer::new(packet)
            })
            .collect();
        app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: None,
            user_connections_exhausted: None,
            provider_connections_exhausted: None,
            low_priority_preempted: None,
            user_account_expired: None,
            panel_api_provisioning: None,
            hls_session_or_lease_expired: None,
            panel_api_provisioning_hls_segments: provisioning_segments,
        })));
    }

    fn test_app_state_with_inputs(inputs: Vec<Arc<ConfigInput>>) -> Arc<AppState> {
        test_app_state_with_hls_proxy_and_inputs(Arc::new(HlsProxyManager::new()), inputs)
    }

    fn enable_hls_cache(app_state: &Arc<AppState>) {
        let config = Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                hls_cache: Some(HlsCacheConfigDto::default()),
                ..Default::default()
            })),
            ..Default::default()
        };
        app_state.app_config.config.store(Arc::new(config));
    }

    fn test_app_state_with_hls_proxy(hls_proxy: Arc<HlsProxyManager>) -> Arc<AppState> {
        test_app_state_with_hls_proxy_and_inputs(hls_proxy, Vec::new())
    }

    fn test_app_state_with_hls_proxy_and_inputs(
        hls_proxy: Arc<HlsProxyManager>,
        inputs: Vec<Arc<ConfigInput>>,
    ) -> Arc<AppState> {
        let app_config = test_app_config();
        if !inputs.is_empty() {
            app_config.sources.store(Arc::new(SourcesConfig {
                batch_files: vec![],
                provider: vec![],
                group_lookup: build_group_lookup(&inputs),
                inputs,
                sources: vec![],
                templates: None,
            }));
        }
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_config, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_config.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));
        let cancel_tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(cancel_tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<ManualPlaylistUpdateRequest>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("no-redirect client builds"),
            )),
            public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(crate::api::model::DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy,
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(cancel_tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    async fn create_bound_hls_test_session(
        app_state: &Arc<AppState>,
        input: &ConfigInput,
        stream_ref: &str,
        account_name: &str,
        now_ms: u64,
    ) -> HlsSessionHandle {
        let origin_source = super::build_hls_origin_source(input, stream_ref);
        let session_key = origin_source.session_key();
        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key,
                origin_source,
                &app_state.get_encrypt_secret(),
                now_ms,
            )
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        {
            let mut session_guard = session.write().await;
            session_guard.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from(account_name),
                &proxy_session_id,
                now_ms,
            ));
        }
        session
    }

    async fn create_unbound_hls_test_session(
        app_state: &Arc<AppState>,
        input: &ConfigInput,
        stream_ref: &str,
        now_ms: u64,
    ) -> HlsSessionHandle {
        let origin_source = super::build_hls_origin_source(input, stream_ref);
        let session_key = origin_source.session_key();
        app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key,
                origin_source,
                &app_state.get_encrypt_secret(),
                now_ms,
            )
            .await
            .0
    }

    fn test_hls_origin_io_context(app_state: &Arc<AppState>) -> HlsOriginIoContext {
        HlsOriginIoContext {
            ctx: app_state.hls_ctx(),
            client_addr: test_fingerprint().addr,
            allow_grace: false,
            priority: 0,
            connection_kind: ConnectionKind::Normal,
            reservation_ttl_secs: 60,
            preacquired_provider_handle: None,
            started_generation: None,
        }
    }

    fn single_hls_provider_input(name: &str) -> ConfigInput {
        ConfigInput {
            id: 1,
            name: Arc::from(name),
            input_type: InputType::Xtream,
            url: "http://account.example.com".to_string(),
            username: Some("account-user".to_string()),
            password: Some("account-pass".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            ..ConfigInput::default()
        }
    }

    fn overlap_provider_input() -> ConfigInput {
        ConfigInput {
            id: 1,
            name: Arc::from("overlap-input"),
            input_type: InputType::Xtream,
            url: "http://root.example.com".to_string(),
            username: Some("root-user".to_string()),
            password: Some("root-pass".to_string()),
            enabled: true,
            priority: 10,
            max_connections: 1,
            aliases: Some(vec![crate::model::ConfigInputAlias {
                id: 2,
                name: Arc::from("account-a"),
                url: "http://account.example.com".to_string(),
                username: Some("account-user".to_string()),
                password: Some("account-pass".to_string()),
                priority: 0,
                max_connections: 1,
                exp_date: None,
                enabled: true,
                stalker: None,
            }]),
            ..ConfigInput::default()
        }
    }

    #[tokio::test]
    async fn hls_origin_account_io_lease_allows_parallel_same_session_origin_work() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let binding = session.read().await.origin_account_binding.clone().expect("binding exists");
        let origin_io = test_hls_origin_io_context(&app_state);

        let first = begin_hls_origin_account_io(&origin_io, &session, &binding)
            .await
            .expect("first same-session origin io acquires provider account");
        wait_for_provider_connection_count(&app_state, 1).await;
        let second = begin_hls_origin_account_io(&origin_io, &session, &binding)
            .await
            .expect("second same-session origin io joins session lease");
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(
            session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .expect("session provider lease exists")
                .active_io_count,
            2
        );

        finish_hls_origin_account_io(&origin_io, &session, first, true).await;
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(
            session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .expect("session provider lease remains while second io is active")
                .active_io_count,
            1
        );

        finish_hls_origin_account_io(&origin_io, &session, second, true).await;
        wait_for_provider_connection_count(&app_state, 0).await;
        assert!(
            app_state
                .active_provider
                .is_provider_reserved_for_other_session(&binding.account_name, Some("other-hls-session"))
                .await
        );
    }

    #[tokio::test]
    async fn hls_origin_account_io_lease_blocks_other_hls_sessions_for_same_account() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let first_session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let second_session = create_bound_hls_test_session(&app_state, &input, "67890", "account-a", 1_000).await;
        let first_binding = first_session.read().await.origin_account_binding.clone().expect("binding exists");
        let second_binding = second_session.read().await.origin_account_binding.clone().expect("binding exists");
        let origin_io = test_hls_origin_io_context(&app_state);

        let first = begin_hls_origin_account_io(&origin_io, &first_session, &first_binding)
            .await
            .expect("first session acquires account");
        wait_for_provider_connection_count(&app_state, 1).await;

        assert!(begin_hls_origin_account_io(&origin_io, &second_session, &second_binding).await.is_err());
        wait_for_provider_connection_count(&app_state, 1).await;

        finish_hls_origin_account_io(&origin_io, &first_session, first, true).await;
        wait_for_provider_connection_count(&app_state, 0).await;
    }

    #[tokio::test]
    async fn transient_origin_binding_requires_runtime_prepare_for_missing_or_detached_account() {
        let input = single_hls_provider_input("known-account");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let proxy_session_id = ProxySessionId("shared-hls-session".to_string());
        let known_binding =
            HlsOriginAccountBinding::new(Arc::clone(&input.name), Arc::clone(&input.name), &proxy_session_id, 1_000);
        let missing_binding = HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from("removed-account"),
            &proxy_session_id,
            1_000,
        );
        let mut detached_binding = known_binding.clone();
        detached_binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, 2_000);

        let hls_ctx = app_state.hls_ctx();
        assert!(!super::hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, &known_binding));
        assert!(super::hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, &missing_binding));
        assert!(super::hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, &detached_binding));
    }

    #[tokio::test]
    async fn hls_account_overlap_selects_soft_candidate_but_not_hard_active() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let session = create_bound_hls_test_session(&app_state, &input, "old", "account-a", 1_000).await;
        {
            let mut session = session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
        }
        let new_proxy_session_id = ProxySessionId("new-session".to_string());

        let hard_candidate =
            super::find_hls_account_overlap_candidate(&app_state, &input, &new_proxy_session_id, 5_000).await;
        assert!(hard_candidate.is_none(), "hard-active sessions must not be overbooked");

        let delayed_candidate =
            super::find_hls_account_overlap_candidate(&app_state, &input, &new_proxy_session_id, 12_000).await;
        assert!(delayed_candidate.is_none(), "soft-active candidate must respect the dynamic overlap delay");

        let soft_candidate =
            super::find_hls_account_overlap_candidate(&app_state, &input, &new_proxy_session_id, 21_000)
                .await
                .expect("soft-active session can be overbooked");
        assert_eq!(soft_candidate.account_name.as_ref(), "account-a");
        assert_eq!(soft_candidate.last_media_at_ms, 1_000);
        assert_eq!(soft_candidate.soft_overlap_eligible_at_ms, 21_000);
        assert_eq!(soft_candidate.soft_overlap_delay_ms, 20_000);
        assert_eq!(soft_candidate.reclaim_until_ms, 31_000);
    }

    #[test]
    fn hls_soft_overlap_delay_scales_with_tuliprox_target_pressure() {
        assert_eq!(super::hls_soft_overlap_delay_ms(10_000, 1, 1), 20_000);
        assert_eq!(super::hls_soft_overlap_delay_ms(10_000, 3, 2), 15_000);
        assert_eq!(super::hls_soft_overlap_delay_ms(10_000, 4, 2), 10_000);
    }

    #[tokio::test]
    async fn hls_account_overlap_reclaim_preempts_speculative_session() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let winner = create_bound_hls_test_session(&app_state, &input, "winner", "account-a", 1_000).await;
        let loser = create_bound_hls_test_session(&app_state, &input, "loser", "account-a", 1_000).await;
        let winner_proxy_session_id = winner.read().await.proxy_session_id.clone();
        let loser_proxy_session_id = loser.read().await.proxy_session_id.clone();
        {
            let mut loser = loser.write().await;
            loser.origin_account_binding = Some(HlsOriginAccountBinding::speculative_from(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &loser_proxy_session_id,
                winner_proxy_session_id.clone(),
                20_000,
                2_000,
            ));
        }
        let loser_generation = loser.read().await.activity.origin_work_generation;

        super::reclaim_hls_account_overlap_if_needed(&app_state, &winner, 10_000).await;

        assert!(
            app_state.hls_proxy.is_account_overlap_cooling_down(&input.name, &Arc::from("account-a"), 10_000).await
        );
        assert!(
            !app_state.hls_proxy.is_account_overlap_cooling_down(&input.name, &Arc::from("account-a"), 25_000).await
        );
        let loser_binding_mode = loser.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(
            loser_binding_mode,
            HlsOriginAccountBindingMode::Detached {
                reason: HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner,
                ..
            }
        ));
        assert_eq!(loser.read().await.activity.origin_work_generation, loser_generation + 1);
        let winner_binding_mode = winner.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(winner_binding_mode, HlsOriginAccountBindingMode::Active));
    }

    #[tokio::test]
    async fn hls_account_overlap_promotes_speculative_session_after_soft_window() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let displaced = create_bound_hls_test_session(&app_state, &input, "displaced", "account-a", 1_000).await;
        let promoted = create_bound_hls_test_session(&app_state, &input, "promoted", "account-a", 1_000).await;
        let displaced_proxy_session_id = displaced.read().await.proxy_session_id.clone();
        let promoted_proxy_session_id = promoted.read().await.proxy_session_id.clone();
        {
            let mut promoted = promoted.write().await;
            promoted.origin_account_binding = Some(HlsOriginAccountBinding::speculative_from(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &promoted_proxy_session_id,
                displaced_proxy_session_id,
                20_000,
                2_000,
            ));
        }
        let displaced_generation = displaced.read().await.activity.origin_work_generation;

        super::promote_elapsed_hls_account_overlaps(&app_state, 20_001).await;

        let displaced_binding_mode =
            displaced.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(
            displaced_binding_mode,
            HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
        ));
        assert_eq!(displaced.read().await.activity.origin_work_generation, displaced_generation + 1);
        let promoted_binding_mode = promoted.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(promoted_binding_mode, HlsOriginAccountBindingMode::Active));
    }

    #[tokio::test]
    async fn hls_account_binding_soft_expiry_retains_session_and_reacquires_on_authorized_manifest_work() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        {
            let mut session = session.write().await;
            session.target_duration = Some(1);
            session.mark_authorized_media_access(1_000);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        super::detach_unprotected_hls_origin_account_bindings(&app_state, 4_001).await;

        {
            let session = session.read().await;
            let binding = session.origin_account_binding.as_ref().expect("detached binding is retained");
            assert!(matches!(
                binding.binding_mode,
                HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
            ));
            assert_eq!(session.activity.origin_work_generation, old_generation + 1);
        }

        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &session,
            &input,
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            &proxy_session_id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            4_100,
        )
        .await
        .expect("authorized origin work can reacquire a detached binding");

        let binding =
            prepared_origin.origin_account_binding_to_store.as_ref().expect("new binding should be stored by caller");
        assert_eq!(binding.account_name.as_ref(), "account-a");
        assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
        assert_eq!(prepared_origin.fetch_url, "http://account.example.com/live/account-user/account-pass/12345.m3u8");
        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle).await;
    }

    #[tokio::test]
    async fn hls_origin_runtime_uses_soft_overlap_before_grace_for_interactive_work() {
        let input = single_hls_provider_input("soft-overlap-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let old_session = create_bound_hls_test_session(&app_state, &input, "old", input.name.as_ref(), 1_000).await;
        let old_proxy_session_id = old_session.read().await.proxy_session_id.clone();
        {
            let mut session = old_session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
        }
        let old_binding = old_session.read().await.origin_account_binding.clone().expect("binding exists");
        app_state
            .active_provider
            .refresh_provider_reservation(&old_binding.account_name, &old_binding.session_owner, 60)
            .await;

        let new_session = create_unbound_hls_test_session(&app_state, &input, "new", 12_000).await;
        let new_proxy_session_id = new_session.read().await.proxy_session_id.clone();
        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &new_session,
            &input,
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            &new_proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55201)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            21_000,
        )
        .await
        .expect("interactive work should use soft-active overlap before grace");

        let binding =
            prepared_origin.origin_account_binding_to_store.as_ref().expect("speculative binding should be prepared");
        assert_eq!(binding.account_name, input.name);
        assert!(matches!(
            &binding.binding_mode,
            HlsOriginAccountBindingMode::Speculative {
                displaced_proxy_session_id,
                ..
            } if displaced_proxy_session_id == &old_proxy_session_id
        ));
        assert!(matches!(
            prepared_origin.preacquired_origin_account_handle.as_ref().map(|handle| &handle.allocation),
            Some(super::ProviderAllocation::Available(_))
        ));

        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle).await;
    }

    #[tokio::test]
    async fn hls_origin_runtime_normal_policy_preempts_active_soft_hls_binding() {
        let input = single_hls_provider_input("policy-preempt-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let soft_session = create_bound_hls_test_session(&app_state, &input, "soft", input.name.as_ref(), 1_000).await;
        let soft_generation = {
            let mut session = soft_session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(10_000);
            session.reconcile_effective_origin_acquire_policy(
                Some(HlsEffectiveOriginAcquirePolicy::new(ConnectionKind::Soft, 0, 10_000)),
                10_000,
            );
            session.activity.origin_work_generation
        };
        let soft_binding = soft_session.read().await.origin_account_binding.clone().expect("soft binding exists");
        app_state
            .active_provider
            .refresh_provider_reservation(&soft_binding.account_name, &soft_binding.session_owner, 60)
            .await;

        let normal_session = create_unbound_hls_test_session(&app_state, &input, "normal", 10_500).await;
        let normal_proxy_session_id = normal_session.read().await.proxy_session_id.clone();
        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &normal_session,
            &input,
            "http://account.example.com/live/account-user/account-pass/normal.m3u8",
            "http://account.example.com/live/account-user/account-pass/normal.m3u8",
            &normal_proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55231)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            10_500,
        )
        .await
        .expect("normal HLS policy should preempt active soft HLS binding");

        let new_binding = prepared_origin
            .origin_account_binding_to_store
            .as_ref()
            .expect("preempting session should receive active binding");
        assert_eq!(new_binding.account_name, input.name);
        assert!(matches!(new_binding.binding_mode, HlsOriginAccountBindingMode::Active));
        assert!(matches!(
            prepared_origin.preacquired_origin_account_handle.as_ref().map(|handle| &handle.allocation),
            Some(super::ProviderAllocation::Available(_))
        ));
        let soft_session = soft_session.read().await;
        assert_eq!(soft_session.activity.origin_work_generation, soft_generation + 1);
        assert!(matches!(
            soft_session.origin_account_binding.as_ref().map(|binding| &binding.binding_mode),
            Some(HlsOriginAccountBindingMode::Detached {
                reason: HlsOriginAccountDetachedReason::PreemptedByHigherPriority,
                ..
            })
        ));
        drop(soft_session);

        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle).await;
    }

    #[tokio::test]
    async fn preempted_origin_runtime_commits_low_priority_tail_without_redirect_or_origin_fetch() {
        let fixture = runtime_policy_endpoint_fixture(true).await;
        let input = single_hls_provider_input("runtime-preempted-input");
        let session = fixture
            .app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&fixture.proxy_session_id)
            .await
            .expect("runtime policy session");
        let now_ms = super::current_time_millis();
        let mut binding = HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from("preempted-account"),
            &fixture.proxy_session_id,
            now_ms,
        );
        binding.detach(HlsOriginAccountDetachedReason::PreemptedByHigherPriority, now_ms);
        session.write().await.replace_origin_account_binding(Some(binding));
        let origin_refresh_before = session.read().await.origin_refresh.clone();
        let origin = super::HlsCacheManifestOrigin {
            raw_request_url: "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            session_entry_url: super::HlsOriginEntryUrl::direct_http(
                "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            ),
            input: &input,
            origin_source: super::build_hls_origin_source(&input, "12345"),
        };
        let context = test_hls_access_context(fixture.proxy_session_id.clone(), fixture.lease_id.clone());

        let result = super::prepare_hls_canonical_manifest_origin_runtime(
            &fixture.app_state,
            &session,
            &context,
            &origin,
            &fixture.proxy_session_id,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &test_fingerprint(),
            None,
            now_ms,
        )
        .await;
        let Err(response) = result else {
            panic!("detached origin binding must resolve to the lease-bound policy tail");
        };

        assert!(!response.headers().contains_key(header::LOCATION));
        let plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
        assert_eq!(plan.reason, HlsRuntimeCustomTailReason::LowPriorityPreempted);
        assert_eq!(plan.segment_duration_ms, 10_027);
        assert_eq!(session.read().await.origin_refresh, origin_refresh_before);
        let replay = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert!(!replay.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(replay).await.to_vec()).expect("preemption manifest utf8");
        assert!(body.contains("/terminal/"));
        assert!(!body.contains("/cvs/hls/"));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[tokio::test]
    async fn hls_origin_policy_preemption_rejects_soft_request_against_active_normal_binding() {
        let input = single_hls_provider_input("policy-no-preempt-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let normal_session =
            create_bound_hls_test_session(&app_state, &input, "normal", input.name.as_ref(), 1_000).await;
        {
            let mut session = normal_session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(10_000);
            session.reconcile_effective_origin_acquire_policy(
                Some(HlsEffectiveOriginAcquirePolicy::new(ConnectionKind::Normal, 0, 10_000)),
                10_000,
            );
        }
        let normal_binding = normal_session.read().await.origin_account_binding.clone().expect("normal binding exists");
        app_state
            .active_provider
            .refresh_provider_reservation(&normal_binding.account_name, &normal_binding.session_owner, 60)
            .await;
        let soft_session = create_unbound_hls_test_session(&app_state, &input, "soft", 10_500).await;
        let soft_proxy_session_id = soft_session.read().await.proxy_session_id.clone();

        let result = super::prepare_hls_origin_policy_preempt_runtime(
            &app_state,
            &soft_session,
            &input,
            "http://account.example.com/live/account-user/account-pass/soft.m3u8",
            "http://account.example.com/live/account-user/account-pass/soft.m3u8",
            &soft_proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55232)),
            ConnectionKind::Soft,
            -100,
            10_500,
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(
            normal_session.read().await.origin_account_binding.as_ref().map(|binding| &binding.binding_mode),
            Some(HlsOriginAccountBindingMode::Active)
        ));
        assert!(
            app_state
                .active_provider
                .is_provider_reserved_for_other_session(&normal_binding.account_name, Some("unrelated-session"))
                .await
        );
    }

    #[tokio::test]
    async fn hls_origin_runtime_uses_grace_as_interactive_fallback_after_overlap_fails() {
        let input = single_hls_provider_input("grace-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let occupied = app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &test_addr_with_port(55211),
                false,
                0,
                ConnectionKind::Normal,
                Some("external-owner"),
            )
            .await
            .expect("test should occupy the only provider account");
        let session = create_unbound_hls_test_session(&app_state, &input, "12345", 2_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();

        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &session,
            &input,
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            &proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55212)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            2_000,
        )
        .await
        .expect("interactive work can use grace when normal acquire and overlap fail");

        assert!(matches!(
            prepared_origin.preacquired_origin_account_handle.as_ref().map(|handle| &handle.allocation),
            Some(super::ProviderAllocation::GracePeriod(_))
        ));
        assert_eq!(
            prepared_origin
                .origin_account_binding_to_store
                .as_ref()
                .expect("grace binding should still bind the selected account")
                .account_name
                .as_ref(),
            input.name.as_ref()
        );

        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle).await;
        app_state.connection_manager.release_provider_handle(Some(occupied)).await;
    }

    #[tokio::test]
    async fn hls_provider_exhausted_without_provisioning_returns_custom_manifest() {
        let input = single_hls_provider_input("provider-exhausted-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        store_test_sources_with_target(
            &app_state,
            input.clone(),
            ConfigTarget::from(&ConfigTargetDto { id: 1, name: "default".to_string(), ..Default::default() }),
        );
        enable_provider_exhausted_custom_response(&app_state);
        let session = create_unbound_hls_test_session(&app_state, &input, "provider-exhausted-session", 1_000).await;
        let access_lease_id = HlsAccessLeaseId("provider-exhausted-lease".to_string());
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
        let strip = app_state.hls_proxy.strip();

        let response = super::hls_shared_provisioning_or_provider_exhausted_response(
            &app_state,
            &session,
            "hls-user",
            &input,
            59,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body(response).await;
        let manifest = String::from_utf8(body.to_vec()).expect("manifest is utf8");
        assert!(manifest.contains("#EXTM3U"));
        assert!(manifest.contains(&format!("/cvs/hls/{}/", access_lease_id.0)));
        assert!(!manifest.contains("/hls-user/"));
        assert!(!manifest.contains("/hls-pass/"));
        assert!(!manifest.contains("/provider_connections_exhausted/"));
    }

    #[tokio::test]
    async fn hls_provider_exhausted_grace_hold_waits_for_grace_period_before_retry() {
        let input = single_hls_provider_input("provider-grace-hold-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        app_state.app_config.config.store(Arc::new(Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                stream: Some(StreamConfigDto {
                    grace_period_millis: 20,
                    grace_period_timeout_secs: 10,
                    grace_period_hold_stream: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        }));
        let session = create_unbound_hls_test_session(&app_state, &input, "provider-grace-session", 1_000).await;
        let access_lease_id = HlsAccessLeaseId("provider-grace-lease".to_string());
        let strip = app_state.hls_proxy.strip();
        let resolution = super::hls_provider_connections_exhausted_manifest_resolution(
            &app_state,
            &session,
            "hls-user",
            &input,
            59,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            true,
        );

        let resolution = tokio::time::timeout(Duration::from_millis(500), resolution)
            .await
            .expect("grace hold deadline should wake");

        assert!(matches!(resolution, super::HlsProviderExhaustedResolution::RetryAcquire));
    }

    #[tokio::test]
    async fn provider_grace_expiry_commits_lease_bound_provider_exhausted_tail() {
        let fixture = runtime_policy_endpoint_fixture(true).await;
        let input = single_hls_provider_input("runtime-provider-grace-input");
        store_test_sources_with_target(
            &fixture.app_state,
            input.clone(),
            ConfigTarget::from(&ConfigTargetDto { id: 1, name: "default".to_string(), ..Default::default() }),
        );
        let current = fixture.app_state.app_config.config.load();
        fixture.app_state.app_config.config.store(Arc::new(Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                stream: Some(StreamConfigDto {
                    grace_period_millis: 20,
                    grace_period_timeout_secs: 10,
                    grace_period_hold_stream: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..current.as_ref().clone()
        }));
        let strip = fixture.app_state.hls_proxy.strip();
        let session = fixture
            .app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&fixture.proxy_session_id)
            .await
            .expect("runtime provider session");

        let grace = super::hls_provider_connections_exhausted_manifest_resolution(
            &fixture.app_state,
            &session,
            "hls-user",
            &input,
            12345,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            true,
        )
        .await;
        assert!(matches!(grace, super::HlsProviderExhaustedResolution::RetryAcquire));

        let response = super::hls_provider_connections_exhausted_manifest_resolution(
            &fixture.app_state,
            &session,
            "hls-user",
            &input,
            12345,
            &fixture.lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            false,
        )
        .await;
        let super::HlsProviderExhaustedResolution::Response(response) = response else {
            panic!("expired grace must resolve to a finite response");
        };
        assert!(!response.headers().contains_key(header::LOCATION));

        let plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
        assert_eq!(plan.reason, HlsRuntimeCustomTailReason::ProviderConnectionsExhausted);
        assert_eq!(plan.segment_duration_ms, 10_027);
        let replay = super::hls_provider_connections_exhausted_manifest_resolution(
            &fixture.app_state,
            &session,
            "hls-user",
            &input,
            12345,
            &fixture.lease_id,
            HlsAccessLeaseState::Denied,
            &strip,
            None,
            false,
        )
        .await;
        let super::HlsProviderExhaustedResolution::Response(replay) = replay else {
            panic!("committed provider tail must replay");
        };
        assert_eq!(replay.status(), StatusCode::OK);
        assert!(!replay.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(replay).await.to_vec()).expect("provider terminal manifest utf8");
        assert!(body.contains("/terminal/"));
        assert!(!body.contains("/cvs/hls/"));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[tokio::test]
    async fn shared_provisioning_timeline_manifest_uses_canonical_hls_session_segments() {
        let input = single_hls_provider_input("shared-provisioning-timeline-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_provisioning_custom_response(&app_state);
        let session = create_unbound_hls_test_session(&app_state, &input, "12345", 1_000).await;
        let access_lease_id = HlsAccessLeaseId("timeline-lease".to_string());
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        activate_test_hls_access_lease(
            &app_state,
            &proxy_session_id,
            &access_lease_id.0,
            super::current_time_millis(),
            60_000,
        )
        .await;
        let strip = app_state.hls_proxy.strip();

        let response = super::hls_shared_provisioning_timeline_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
        )
        .await
        .expect("provisioning manifest should render");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest is utf8");
        assert!(body.contains("#EXT-X-VERSION:7\n"));
        assert!(body.contains("#EXT-X-INDEPENDENT-SEGMENTS\n"));
        assert!(body.contains("#EXT-X-TARGETDURATION:2\n"));
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(body.contains("#EXTINF:2.000,\n"));
        assert!(!body.contains("#EXTINF:12.000,\n"));
        assert!(body.contains("/hls/shared/live/"));
        assert!(body.contains("/000000.ts?pseq=0"));
        assert!(body.contains("/000001.ts?pseq=1"));
        assert!(body.contains("/000002.ts?pseq=2"));
        assert!(body.matches("#EXTINF:").count() <= 6);
        assert!(!body.contains("/cvs/hls/"));
        {
            let session = session.read().await;
            assert_eq!(session.proxy_next_seq, Some(3));
            assert_eq!(session.publishable_origin_head_proxy_seq, Some(0));
            assert_eq!(session.publishable_origin_tail_proxy_seq, Some(2));
            assert_eq!(session.segments.len(), 3);
        }

        let response = super::hls_shared_provisioning_timeline_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
        )
        .await
        .expect("subsequent manifest should append one segment");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest is utf8");
        assert!(body.contains("/000003.ts?pseq=3"));
        assert!(body.matches("#EXTINF:").count() <= 6);
        assert_eq!(session.read().await.proxy_next_seq, Some(4));
    }

    #[tokio::test]
    async fn stale_provisioning_segments_do_not_trigger_canonical_handoff() {
        let input = single_hls_provider_input("stale-provisioning-handoff-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_provisioning_custom_response(&app_state);
        let session = create_unbound_hls_test_session(&app_state, &input, "12345", 1_000).await;
        let initial_lease_id = HlsAccessLeaseId("initial-provisioning-lease".to_string());
        let strip = app_state.hls_proxy.strip();

        super::hls_shared_provisioning_timeline_manifest_response(
            &app_state,
            &session,
            &initial_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
        )
        .await
        .expect("provisioning manifest should render local segments");
        {
            let session_guard = session.read().await;
            assert!(session_guard.segments.values().any(crate::api::model::is_hls_provisioning_segment));
            assert_eq!(session_guard.segments.len(), 3);
            assert_eq!(session_guard.pending_handoff_discontinuity_sequence, None);
        }

        let new_lease_id = HlsAccessLeaseId("new-playback-lease".to_string());
        let previous_rendered_at = super::maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
            &app_state,
            &session,
            &input,
            12345,
            &new_lease_id,
            2_000,
        )
        .await;

        assert_eq!(previous_rendered_at, None);
        let session_guard = session.read().await;
        assert_eq!(session_guard.segments.len(), 3);
        assert_eq!(session_guard.pending_handoff_discontinuity_sequence, None);
    }

    #[tokio::test]
    async fn provisioning_handoff_finds_shared_session_by_input_stream_id_not_virtual_id() {
        let input = single_hls_provider_input("origin-id-handoff-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        let origin_session = create_unbound_hls_test_session(&app_state, &input, "80510", 1_000).await;
        let virtual_id_session = create_unbound_hls_test_session(&app_state, &input, "1001", 1_000).await;
        let stream_identity = super::HlsEntryStreamIdentity::new(1001, "80510").expect("input stream identity");

        assert!(
            super::mark_hls_provisioning_handoff_discontinuity(&app_state, &input, &stream_identity, None, 2_000,)
                .await
        );

        assert!(origin_session.read().await.pending_handoff_discontinuity_sequence.is_some());
        assert_eq!(virtual_id_session.read().await.pending_handoff_discontinuity_sequence, None);
    }

    #[tokio::test]
    async fn shared_provisioning_handoff_continues_proxy_sequence_for_origin_segments() {
        let input = single_hls_provider_input("shared-provisioning-handoff-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_provisioning_custom_response(&app_state);
        let session = create_unbound_hls_test_session(&app_state, &input, "12345", 1_000).await;
        let access_lease_id = HlsAccessLeaseId("handoff-lease".to_string());
        let strip = app_state.hls_proxy.strip();
        super::hls_shared_provisioning_timeline_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
        )
        .await
        .expect("provisioning manifest should render");

        let manifest = match parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:4025\n#EXTINF:4.0,\n4025.ts\n#EXTINF:4.0,\n4026.ts\n#EXTINF:4.0,\n4027.ts\n",
            "http://origin.example/live/stream.m3u8",
        ) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        };
        let rendered = {
            let mut session_guard = session.write().await;
            session_guard.mark_pending_handoff_discontinuity(0);
            drop(session_guard);
            assert!(
                super::ensure_shared_hls_provisioning_handoff_gap(&app_state, &session, 2_000).await,
                "handoff should append one gap segment"
            );
            let mut session_guard = session.write().await;
            session_guard.apply_origin_manifest(&manifest).expect("origin manifest should map");
            for proxy_seq in 4..=6 {
                session_guard.segments.get_mut(&proxy_seq).expect("origin segment").status =
                    SegmentCacheStatus::Ready { content_length: 1024, ready_at_ms: 2_000 };
            }
            session_guard.render_and_store_manifest(2_000).expect("handoff manifest should render")
        };

        assert!(rendered.body.contains("#EXT-X-MEDIA-SEQUENCE:1\n"));
        assert!(rendered.body.contains("#EXT-X-TARGETDURATION:4\n"));
        assert!(rendered.body.contains("/000002.ts?pseq=2"));
        assert!(rendered.body.contains("/000004.ts"));
        assert!(rendered.body.contains("/000005.ts"));
        assert!(rendered.body.contains("/000006.ts"));
        assert!(!rendered.body.contains("/004025.ts"));
        let provisioning_tail = rendered.body.find("/000002.ts?pseq=2").expect("provisioning tail is rendered");
        let gap_tag = rendered.body.find("#EXT-X-GAP\n").expect("handoff gap tag is rendered");
        let gap_uri = rendered.body.find("/000003.ts?pseq=3").expect("handoff gap uri is rendered");
        let discontinuity = rendered
            .body
            .find("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,\n/hls/shared/live/")
            .expect("origin handoff discontinuity is rendered");
        let first_origin = rendered.body.find("/000004.ts").expect("first origin segment is rendered");
        assert!(provisioning_tail < gap_tag);
        assert!(gap_tag < gap_uri);
        assert!(gap_uri < discontinuity);
        assert!(discontinuity < first_origin);
    }

    #[tokio::test]
    async fn hls_origin_runtime_background_skips_soft_overlap_and_grace() {
        let input = single_hls_provider_input("background-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let old_session = create_bound_hls_test_session(&app_state, &input, "old", input.name.as_ref(), 1_000).await;
        {
            let mut session = old_session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
        }
        let old_binding = old_session.read().await.origin_account_binding.clone().expect("binding exists");
        app_state
            .active_provider
            .refresh_provider_reservation(&old_binding.account_name, &old_binding.session_owner, 60)
            .await;
        let new_session = create_unbound_hls_test_session(&app_state, &input, "new", 12_000).await;
        let new_proxy_session_id = new_session.read().await.proxy_session_id.clone();

        let result = super::prepare_hls_origin_runtime(
            &app_state,
            &new_session,
            &input,
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            &new_proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55221)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Segment,
            super::HlsOriginWorkClass::Background,
            12_000,
        )
        .await;

        assert_eq!(
            result.err(),
            Some(super::HlsOriginRuntimeAcquireError::NoAccountAvailable {
                reason: super::HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted
            })
        );
        let old_session = old_session.read().await;
        assert!(matches!(
            old_session.origin_account_binding.as_ref().expect("old binding remains").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert!(new_session.read().await.origin_account_binding.is_none());
    }

    #[test]
    fn hls_detached_origin_binding_reclaimed_by_owner_maps_to_preempted_no_account_reason() {
        let proxy_session_id = ProxySessionId("preempted-session".to_string());
        let mut binding =
            HlsOriginAccountBinding::new(Arc::from("input-a"), Arc::from("account-a"), &proxy_session_id, 1_000);
        binding.detach(HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner, 2_000);

        assert_eq!(
            super::hls_no_account_reason_for_binding(Some(&binding)),
            super::HlsOriginRuntimeNoAccountReason::OriginBindingPreempted
        );
    }

    #[test]
    fn hls_detached_origin_binding_soft_window_elapsed_maps_to_exhausted_no_account_reason() {
        let proxy_session_id = ProxySessionId("soft-window-session".to_string());
        let mut binding =
            HlsOriginAccountBinding::new(Arc::from("input-a"), Arc::from("account-a"), &proxy_session_id, 1_000);
        binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, 2_000);

        assert_eq!(
            super::hls_no_account_reason_for_binding(Some(&binding)),
            super::HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted
        );
    }

    #[tokio::test]
    async fn hls_account_binding_without_media_activity_is_not_detached() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        {
            let mut session = session.write().await;
            session.target_duration = Some(12);
            session.mark_authorized_manifest_access(1_000);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        super::detach_unprotected_hls_origin_account_bindings(&app_state, 60_000).await;

        let session = session.read().await;
        let binding = session.origin_account_binding.as_ref().expect("binding remains");
        assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
        assert_eq!(session.activity.origin_work_generation, old_generation);
        assert_eq!(session.activity.last_authorized_media_at_ms, None);
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
    }

    #[tokio::test]
    async fn hls_hard_manifest_failure_forces_next_fresh_commit() {
        let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
        {
            let mut session = session.write().await;
            session.last_rendered_manifest = Some(RenderedManifest {
                body: "#EXTM3U\n#EXTINF:4.0,\n000001.ts\n".to_string(),
                first_proxy_seq: 1,
                last_proxy_seq: 1,
                playlist_duration_ms: 4_000,
                valid_until_ms: 5_000,
                render_gap_segments: 0,
                rendered_at_ms: 1_000,
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![1],
            });
            session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
        }

        assert_eq!(
            super::hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, 2_000).await,
            HlsManifestCommitRequirement::FreshCommitRequired {
                reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure
            }
        );
    }

    #[tokio::test]
    async fn hls_normal_expired_session_allows_committed_manifest_while_manifest_valid() {
        let now_ms = 100_000;
        let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
        {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.clone();
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::from("test-input"),
                Arc::from("test-account"),
                &proxy_session_id,
                now_ms,
            ));
            session.last_rendered_manifest = Some(RenderedManifest {
                body: "#EXTM3U\n#EXTINF:4.0,\n000001.ts\n".to_string(),
                first_proxy_seq: 1,
                last_proxy_seq: 1,
                playlist_duration_ms: 4_000,
                valid_until_ms: now_ms.saturating_add(10_000),
                render_gap_segments: 0,
                rendered_at_ms: now_ms.saturating_sub(1_000),
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![1],
            });
        }

        assert_eq!(
            super::hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, now_ms).await,
            HlsManifestCommitRequirement::CommittedManifestAllowed
        );
    }

    #[tokio::test]
    async fn hls_normal_expired_session_requires_fresh_commit_after_manifest_validity() {
        let now_ms = 100_000;
        let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
        {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.clone();
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::from("test-input"),
                Arc::from("test-account"),
                &proxy_session_id,
                now_ms,
            ));
            session.last_rendered_manifest = Some(RenderedManifest {
                body: "#EXTM3U\n#EXTINF:4.0,\n000001.ts\n".to_string(),
                first_proxy_seq: 1,
                last_proxy_seq: 1,
                playlist_duration_ms: 4_000,
                valid_until_ms: now_ms.saturating_sub(1),
                render_gap_segments: 0,
                rendered_at_ms: now_ms.saturating_sub(10_000),
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![1],
            });
        }

        assert_eq!(
            super::hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, now_ms).await,
            HlsManifestCommitRequirement::FreshCommitRequired {
                reason: HlsFreshManifestRequiredReason::ExpiredRevalidation
            }
        );
    }

    #[tokio::test]
    async fn hls_ready_cache_hit_does_not_require_origin_reacquire_when_binding_is_detached() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        {
            let mut session = session.write().await;
            session
                .origin_account_binding
                .as_mut()
                .expect("binding exists")
                .detach(HlsOriginAccountDetachedReason::Cleanup, 2_000);
            session.segments.insert(
                123,
                SegmentEntry {
                    origin_key: OriginSegmentKey {
                        origin_epoch: 0,
                        effective_host_id: 0,
                        host_local_sequence: 123,
                        host_local_index: 123,
                    },
                    proxy_seq: 123,
                    duration_ms: 4_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/mp2t".to_string(),
                    cache_key: SegmentCacheKey::new(proxy_session_id, 123, "ts"),
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: Some(OriginSegmentFetchRef {
                        resolved_origin_url: "http://origin.example.com/123.ts".to_string(),
                        byte_range: None,
                        valid_until_ms: None,
                    }),
                    status: SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 1_000 },
                    last_rendered_at_ms: None,
                    access: Arc::new(CacheAccessState::new()),
                },
            );
        }

        let segment_file = HlsSegmentFile { proxy_seq: 123, extension: "ts".to_string() };
        assert!(!super::hls_segment_request_requires_origin_work(&session, &segment_file).await);
        assert!(super::hls_origin_binding_needs_reacquire(&session).await);
    }

    async fn activate_test_hls_access_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        lease_id: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) {
        let lease_id = HlsAccessLeaseId(lease_id.to_string());
        let valid_window_ms = ttl_ms.saturating_mul(10).max(ttl_ms);
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                valid_window_ms,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: ttl_ms, valid_window_ms },
            )
            .await
            .is_activated());
    }

    async fn register_test_hls_stream_for_lease_release(
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        provider: &str,
    ) {
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.max_connections = 1;
        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "hls-session-token",
                virtual_id: 12345,
                provider,
                stream_url: "http://origin.example.com/live/user/pass/12345.m3u8",
                addr: &test_addr(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let mut stream_channel =
            super::fallback_hls_cache_stream_channel(0, 12345, &session.read().await.origin_source, proxy_session_id);
        stream_channel.shared = true;
        stream_channel.shared_stream_id = Some(super::hls_cache_shared_stream_id(proxy_session_id));
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: ConnectionKind::Normal,
                priority: user.priority,
                soft_priority: user.soft_priority,
                fingerprint: &test_fingerprint(),
                provider: Arc::from(provider),
                stream_channel: &stream_channel,
                user_agent: std::borrow::Cow::Borrowed("test"),
                session_token: Some("hls-session-token"),
            })
            .await;
    }

    fn test_segment_entry(
        proxy_session_id: &ProxySessionId,
        proxy_seq: u64,
        status: SegmentCacheStatus,
    ) -> SegmentEntry {
        SegmentEntry {
            origin_key: OriginSegmentKey {
                origin_epoch: 0,
                effective_host_id: 0,
                host_local_sequence: proxy_seq,
                host_local_index: u32::try_from(proxy_seq).unwrap_or(u32::MAX),
            },
            proxy_seq,
            duration_ms: 4_000,
            proxy_file_ext: "ts".to_string(),
            content_type: "video/mp2t".to_string(),
            cache_key: SegmentCacheKey::new(proxy_session_id.clone(), proxy_seq, "ts"),
            discontinuity_before: false,
            program_date_time: None,
            daterange_tags_before: Vec::new(),
            origin_byte_range: None,
            map_ref: None,
            encryption: None,
            origin_fetch_ref: Some(OriginSegmentFetchRef {
                resolved_origin_url: format!("http://origin.example.com/{proxy_seq}.ts"),
                byte_range: None,
                valid_until_ms: None,
            }),
            status,
            last_rendered_at_ms: None,
            access: Arc::new(CacheAccessState::new()),
        }
    }

    #[tokio::test]
    async fn hls_access_lease_idle_releases_user_but_keeps_origin_binding_and_queues() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let binding = session.read().await.origin_account_binding.clone().expect("binding exists");
        let account_name = Arc::from("account-a");
        app_state.active_provider.refresh_provider_reservation(&account_name, &binding.session_owner, 60).await;
        assert!(
            app_state.active_provider.is_provider_reserved_for_other_session(&account_name, Some("other-owner")).await
        );
        activate_test_hls_access_lease(&app_state, &proxy_session_id, "detach-lease", 1_000, 1_000).await;
        register_test_hls_stream_for_lease_release(&app_state, &session, &proxy_session_id, input.name.as_ref()).await;
        assert_eq!(app_state.active_users.active_streams().await.len(), 1);
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                1_000,
            )
            .await;
        {
            let mut session = session.write().await;
            assert_eq!(session.activity.active_access_lease_count, 1);
            session.segments.insert(1, test_segment_entry(&proxy_session_id, 1, SegmentCacheStatus::Discovered));
            session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Prefetch, 1_100);
            session.segments.insert(
                2,
                test_segment_entry(
                    &proxy_session_id,
                    2,
                    SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 1_000 },
                ),
            );
            let map_id = ProxyMapId(1);
            let mut map = MapEntry::new(
                &proxy_session_id,
                map_id,
                OriginMapKey {
                    origin_epoch: 0,
                    resolved_origin_uri: "http://origin.example.com/init.mp4".to_string(),
                    byte_range: None,
                },
                "mp4".to_string(),
            );
            map.status = MapCacheStatus::Queued { queued_at_ms: 1_100 };
            session.maps.insert(map_id, map);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                3_000,
            )
            .await;

        {
            let session = session.read().await;
            assert_eq!(session.activity.active_access_lease_count, 0);
            let binding = session.origin_account_binding.as_ref().expect("binding is retained");
            assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
            assert_eq!(session.activity.origin_work_generation, old_generation);
            assert!(!session.segment_prefetch_queue.is_empty());
            assert!(matches!(
                session.segments.get(&1).expect("queued segment remains").status,
                SegmentCacheStatus::Queued { .. }
            ));
            assert!(matches!(
                session.segments.get(&2).expect("ready segment remains").status,
                SegmentCacheStatus::Ready { .. }
            ));
            assert!(matches!(
                session.maps.get(&ProxyMapId(1)).expect("map remains").status,
                MapCacheStatus::Queued { .. }
            ));
        }
        assert!(
            app_state.active_provider.is_provider_reserved_for_other_session(&account_name, Some("other-owner")).await
        );
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn hls_access_lease_sync_keeps_binding_and_queue_when_active_count_remains_positive() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        activate_test_hls_access_lease(&app_state, &proxy_session_id, "still-active-lease", 1_000, 10_000).await;
        {
            let mut session = session.write().await;
            session.activity.active_access_lease_count = 1;
            session.segments.insert(1, test_segment_entry(&proxy_session_id, 1, SegmentCacheStatus::Discovered));
            session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Prefetch, 1_100);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                1_500,
            )
            .await;

        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 1);
        assert!(matches!(
            session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation);
        assert!(!session.segment_prefetch_queue.is_empty());
        assert!(matches!(
            session.segments.get(&1).expect("segment remains queued").status,
            SegmentCacheStatus::Queued { .. }
        ));
    }

    #[tokio::test]
    async fn hls_access_lease_sync_zero_to_zero_is_idempotent() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                3_000,
            )
            .await;

        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 0);
        assert!(matches!(
            session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation);
    }

    #[tokio::test]
    async fn hls_access_lease_gc_prepass_releases_user_without_detaching_origin_binding() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        activate_test_hls_access_lease(&app_state, &proxy_session_id, "gc-expired-lease", 1_000, 1_000).await;
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                1_000,
            )
            .await;
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_all_session_access_leases_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                3_000,
            )
            .await;
        let _ = app_state.hls_proxy.run_garbage_collection_once(3_000).await.expect("gc should run");

        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("detach keeps shared hls session");
        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 0);
        assert!(matches!(
            session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation);
    }

    fn test_addr() -> SocketAddr { "127.0.0.1:55123".parse().unwrap_or_else(|_| unreachable!()) }

    fn test_addr_with_port(port: u16) -> SocketAddr { SocketAddr::from(([127, 0, 0, 1], port)) }

    fn test_fingerprint() -> Fingerprint { Fingerprint::new("test".to_string(), "127.0.0.1".to_string(), test_addr()) }

    fn test_fingerprint_with_addr(addr: SocketAddr) -> Fingerprint {
        Fingerprint::new(format!("test-{}", addr.port()), "127.0.0.1".to_string(), addr)
    }

    async fn create_active_hls_user_session(app_state: &Arc<AppState>) {
        create_active_hls_user_session_with(
            app_state,
            "hls-session-token",
            "origin-provider",
            "http://origin.example.com/live/12345.m3u8",
            test_addr(),
        )
        .await;
    }

    async fn create_active_hls_user_session_with(
        app_state: &Arc<AppState>,
        session_token: &str,
        provider: &str,
        stream_url: &str,
        addr: SocketAddr,
    ) {
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.max_connections = 1;
        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token,
                virtual_id: 12345,
                provider,
                stream_url,
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
    }

    async fn mark_hls_user_session_exhausted(app_state: &Arc<AppState>) {
        let user = app_state.app_config.get_user_credentials("hls-user").expect("configured HLS test user");
        let addr = test_addr();
        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "hls-session-token",
                virtual_id: 12345,
                provider: "origin-provider",
                stream_url: "http://origin.example.com/live/12345.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Exhausted,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
    }

    fn test_hls_access_context(
        proxy_session_id: ProxySessionId,
        access_lease_id: HlsAccessLeaseId,
    ) -> HlsAccessContext {
        test_hls_access_context_with(proxy_session_id, access_lease_id, "hls-session-token", test_fingerprint().key)
    }

    fn test_hls_access_context_with(
        proxy_session_id: ProxySessionId,
        access_lease_id: HlsAccessLeaseId,
        user_session_token: &str,
        client_fingerprint: String,
    ) -> HlsAccessContext {
        HlsAccessContext {
            username: "hls-user".to_string(),
            user_session_token: user_session_token.to_string(),
            proxy_session_id,
            input_id: 1,
            stream_ref: "12345".to_string(),
            virtual_id: 12345,
            known_bitrate_bps: None,
            lease_id: access_lease_id,
            family_key: HlsPlaybackFamilyKey::new("hls-user", client_fingerprint),
            epg_reference_ts: None,
            archive_origin_url: None,
        }
    }

    #[tokio::test]
    async fn hls_cache_stream_channel_uses_archive_epg_context() {
        let app_state = test_app_state();
        let mut access = test_hls_access_context(
            ProxySessionId("proxy-archive".to_string()),
            HlsAccessLeaseId("lease-archive".to_string()),
        );
        access.epg_reference_ts = Some(1_784_898_000);
        access.archive_origin_url = Some("http://provider/channel/timeshift_abs-1784898000.m3u8".to_string());
        let origin_source =
            HlsOriginSource::new(1, Arc::from("test-input"), "80510", HlsOriginSourceKind::M3uMediaPlaylist)
                .with_archive_reference(1_784_898_000);

        let channel = super::build_hls_cache_stream_channel(
            &app_state,
            &access,
            &origin_source,
            &ProxySessionId("proxy-archive".to_string()),
        )
        .await;

        assert_eq!(channel.item_type, PlaylistItemType::Catchup);
        assert_eq!(channel.cluster, XtreamCluster::Video);
        assert_eq!(channel.epg_reference_ts, Some(1_784_898_000));
    }

    #[tokio::test]
    async fn hls_cache_manifest_context_restores_leased_archive_origin() -> Result<(), StatusCode> {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::M3u,
            enabled: true,
            ..ConfigInput::default()
        };
        let mut target = test_m3u_hls_share_target();
        target.name = "default".to_string();
        store_test_sources_with_target(&app_state, input.clone(), target.clone());
        cache_test_m3u_hls_item(
            &app_state,
            &target,
            test_m3u_hls_item(&input, 12345, "80510", "http://provider/channel/mono.m3u8"),
        )
        .await;

        let archive_url = "http://provider/channel/timeshift_abs-1784898000.m3u8";
        let mut access = test_hls_access_context(
            ProxySessionId("proxy-archive".to_string()),
            HlsAccessLeaseId("lease-archive".to_string()),
        );
        access.stream_ref = "80510".to_string();
        access.epg_reference_ts = Some(1_784_898_000);
        access.archive_origin_url = Some(archive_url.to_string());

        let context =
            super::resolve_hls_playback_manifest_request_context(&app_state, &access, &HeaderMap::new()).await?;

        assert_eq!(context.hls_url, archive_url);
        assert_eq!(context.origin_source.stream_ref, "80510");
        assert_eq!(context.origin_source.archive_reference, Some(1_784_898_000));
        Ok(())
    }

    async fn prepare_pending_test_hls_access_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        access_lease_id: &HlsAccessLeaseId,
    ) {
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                access_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                super::current_time_millis(),
                super::hls_access_lease_ttl_ms(app_state),
            ))
            .await;
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_synchronously_returns_initial_manifest() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
        let input_name = Arc::<str>::from("test-input");
        let input = ConfigInput {
            id: 1,
            name: Arc::clone(&input_name),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("hls cache should handle valid live hls entrypoint");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(body.contains(&format!("/hls/shared/live/{}/{}/000000.ts", proxy_session_id.0, access_lease_id.0)));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_key(&session_key)
            .await
            .expect("cold start should create shared hls session");
        {
            let session = session.read().await;
            assert!(session.last_rendered_manifest.is_some());
            let binding = session.origin_account_binding.as_ref().expect("plain http input still has account binding");
            assert_eq!(binding.input_name.as_ref(), "test-input");
            assert_eq!(binding.account_name.as_ref(), "test-input");
        }
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn canonical_recovery_from_provisioning_marks_normal_handoff_boundary() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
        app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), 12345, super::current_time_millis());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("canonical hls cache should recover from provisioning");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
        assert!(body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,"));
        assert!(!app_state.hls_provisioning.has_consumer(&input.name, 12345, super::current_time_millis()));
    }

    #[tokio::test]
    async fn canonical_recovery_from_provisioning_marks_transient_handoff_boundary() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.key\"\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
        app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), 12345, super::current_time_millis());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("canonical hls cache should recover from provisioning");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
        assert!(body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.0,"));
        assert!(!app_state.hls_provisioning.has_consumer(&input.name, 12345, super::current_time_millis()));
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_supports_m3u_hls_origin_source() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:77\n#EXTINF:4.0,\nseg-77.ts\n#EXTINF:4.0,\nseg-78.ts\n#EXTINF:4.0,\nseg-79.ts\n",
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("m3u-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/channel/index.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("hls cache should handle m3u hls media playlists");

        assert_eq!(response.status(), StatusCode::OK);
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_key(&session_key)
            .await
            .expect("m3u hls should create shared session");
        let session = session.read().await;
        assert_eq!(session.origin_source.source_kind, HlsOriginSourceKind::M3uMediaPlaylist);
        let binding = session.origin_account_binding.as_ref().expect("m3u hls input still has account binding");
        assert_eq!(binding.input_name.as_ref(), "m3u-input");
        assert_eq!(binding.account_name.as_ref(), "m3u-input");
    }

    const AES_TEST_MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-VERSION:5\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:77\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n#EXTINF:12,\n77.ts\n#EXTINF:12,\n78.ts\n#EXTINF:12,\n79.ts\n#EXTINF:12,\n80.ts\n#EXTINF:12,\n81.ts\n#EXTINF:12,\n82.ts\n";
    const AES_TEST_KEY_BYTES: &[u8] = b"0123456789abcdef";
    const AES_TEST_ROTATED_KEY_BYTES: &[u8] = b"fedcba9876543210";
    const AES_TEST_PLAINTEXT_SEGMENT: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

    struct AesEndpointFixture {
        _temp_dir: tempfile::TempDir,
        origin: TestSegmentOrigin,
        input: ConfigInput,
        app_state: Arc<AppState>,
        request_url: String,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        access_lease_id: HlsAccessLeaseId,
        key_uri: String,
        base_manifest: HlsLeaseManifestSnapshot,
        asset: Arc<HlsTerminalMediaAsset>,
    }

    async fn assert_aes_live_endpoint(
        app_state: &Arc<AppState>,
        origin: &TestSegmentOrigin,
        access_lease_id: &HlsAccessLeaseId,
        live_body: &str,
    ) -> String {
        let key_uri = live_body
            .lines()
            .find(|line| line.starts_with("#EXT-X-KEY:METHOD=AES-128"))
            .and_then(|line| line.split_once("URI=\"").map(|(_, tail)| tail))
            .and_then(|tail| tail.split_once('"').map(|(uri, _)| uri.to_string()))
            .expect("opaque key URI");
        assert!(key_uri.contains(&format!("/{}/r/", access_lease_id.0)));
        assert!(!live_body.contains("key.bin"));
        assert!(live_body.contains("#EXT-X-VERSION:5\n"));
        let proxy_media_sequence = live_body
            .lines()
            .find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .and_then(|value| value.parse::<u64>().ok())
            .expect("proxy media sequence");
        assert_ne!(proxy_media_sequence, 77);
        assert!(live_body.contains("IV=0x0000000000000000000000000000004d"));
        assert_eq!(origin.key_request_count(), 1);
        let segment_uris = live_body
            .lines()
            .filter(|line| line.starts_with("/hls/shared/live/") && !line.contains("/r/"))
            .collect::<Vec<_>>();
        assert_eq!(segment_uris.len(), 6);
        for (index, segment_uri) in segment_uris.into_iter().enumerate() {
            let response = get_response(Arc::clone(app_state), segment_uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            let served = response_body(response).await;
            let origin_sequence = 77_u64.saturating_add(u64::try_from(index).expect("test segment index"));
            let expected = encrypt_test_aes128_cbc_pkcs7(
                AES_TEST_PLAINTEXT_SEGMENT,
                AES_TEST_KEY_BYTES,
                test_hls_sequence_iv(origin_sequence),
            );
            assert_eq!(served.as_ref(), expected.as_slice());
            assert_ne!(served.as_ref(), AES_TEST_PLAINTEXT_SEGMENT);
            assert_eq!(origin.key_request_count(), 1);
        }
        key_uri
    }

    async fn aes_endpoint_fixture() -> AesEndpointFixture {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let origin = spawn_test_encrypted_hls_origin(
            AES_TEST_MANIFEST,
            Arc::from(AES_TEST_KEY_BYTES),
            Arc::from(AES_TEST_PLAINTEXT_SEGMENT),
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("encrypted-m3u-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_hls_proxy_and_inputs(
            Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)),
            vec![Arc::new(input.clone())],
        );
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/channel/index.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("encrypted-normal-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("compatible AES origin should enter normal HLS cache");
        assert_eq!(response.status(), StatusCode::OK);
        let live_body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
        let key_uri = assert_aes_live_endpoint(&app_state, &origin, &access_lease_id, &live_body).await;
        assert_eq!(origin.segment_request_count(), 6);
        let session = app_state.hls_proxy.sessions().get_by_key(&session_key).await.expect("normal encrypted session");
        assert_eq!(session.read().await.mode, HlsSessionMode::NormalCacheTimeline);
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .await
            .expect("live encrypted lease snapshot");
        let base_manifest = lease.last_manifest_snapshot.expect("authoritative lease manifest");
        assert_eq!(base_manifest.delivery_mode, HlsManifestDeliveryMode::NormalCacheTimeline);
        assert_eq!(
            base_manifest.active_encryption.as_ref().and_then(|encryption| encryption.iv.as_deref()),
            Some("0x00000000000000000000000000000052")
        );
        let asset = terminal_test_asset();
        let evidence = prepare_terminal_base_evidence(
            &session,
            app_state.hls_proxy.segment_cache(),
            &base_manifest,
            super::current_time_millis(),
        )
        .await;
        assert_eq!(evidence.track_signature(), Some(asset.track_signature().clone()));
        assert_eq!(evidence.key_bindings().len(), 1);
        assert_eq!(origin.key_request_count(), 1);
        evidence.release();
        AesEndpointFixture {
            _temp_dir: temp_dir,
            origin,
            input,
            app_state,
            request_url,
            session,
            proxy_session_id,
            access_lease_id,
            key_uri,
            base_manifest,
            asset,
        }
    }

    async fn install_aes_terminal_plan(fixture: &AesEndpointFixture) -> TransientObjectCacheKey {
        let key_response = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await;
        assert_eq!(key_response.status(), StatusCode::OK);
        assert_eq!(response_body(key_response).await.as_ref(), AES_TEST_KEY_BYTES);
        assert_eq!(fixture.origin.key_request_count(), 1);
        let evidence_key_id = {
            let mut session = fixture.session.write().await;
            let key_id = session
                .transient
                .resources
                .values()
                .find(|resource| resource.kind == TransientResourceKind::Key)
                .map(|resource| resource.id.clone())
                .expect("normal key resource");
            session.last_rendered_manifest = None;
            session.transient.last_manifest_body = None;
            session.transient.resources.get_mut(&key_id).expect("key resource").expires_at_ms = 0;
            for (key, object) in &mut session.transient.object_cache {
                if key.transient_resource_id() == &key_id {
                    object.expires_at_ms = 0;
                }
            }
            key_id
        };
        let evidence = prepare_terminal_base_evidence(
            &fixture.session,
            fixture.app_state.hls_proxy.segment_cache(),
            &fixture.base_manifest,
            super::current_time_millis(),
        )
        .await;
        assert_eq!(evidence.track_signature(), Some(fixture.asset.track_signature().clone()));
        assert_eq!(fixture.origin.key_request_count(), 1);
        fixture
            .app_state
            .hls_proxy
            .run_garbage_collection_once(super::current_time_millis())
            .await
            .expect("evidence-pinned GC");
        {
            let session = fixture.session.read().await;
            assert!(session.transient.resources.contains_key(&evidence_key_id));
            assert!(session.transient.object_cache.keys().any(|key| key.transient_resource_id() == &evidence_key_id));
        }
        let plan = build_terminal_tail_plan(HlsTerminalTailBuildInput {
            generation: HlsTerminalTailGeneration(23),
            created_at_ms: super::current_time_millis(),
            base_availability: evidence.availability(),
            base_track_signature: evidence.track_signature(),
            base_splice_evidence: evidence.splice_evidence().cloned(),
            terminal_splice_evidence: Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(
                &fixture.asset,
            )),
            base_timing: evidence.timing().cloned(),
            base_key_bindings: evidence.key_bindings(),
            expected_asset: HlsRuntimeCustomTailAssetIdentity::channel_unavailable(
                HlsTerminalAssetIdentity::from_asset(&fixture.asset),
            ),
            base_manifest: fixture.base_manifest.clone(),
            anchored_bundle: HlsTerminalTailBuildInput::anchored_bundle_for_test(
                &fixture.asset,
                fixture.base_manifest.target_duration_ms,
            ),
            asset: Arc::clone(&fixture.asset),
        })
        .expect("READY AES key permits safe terminal reset");
        let protection = HlsTerminalTailProtection {
            generation: plan.generation,
            base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
            key_bindings: plan.key_bindings(),
        };
        assert_eq!(protection.key_bindings.len(), 1);
        assert_eq!(protection.key_bindings[0].resource_id(), &evidence_key_id);
        let frozen_source_cache_key = protection.key_bindings[0].source_cache_key().clone();
        {
            let mut leases = fixture.app_state.hls_proxy.access_leases().write().await;
            let mut lease = leases.remove_access_lease(&fixture.access_lease_id).expect("live lease");
            lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::new(plan));
            leases.prepare_access_lease(lease);
        }
        fixture.session.write().await.install_terminal_tail_protection(fixture.access_lease_id.clone(), protection);
        evidence.release();
        frozen_source_cache_key
    }

    async fn assert_aes_terminal_endpoints(fixture: &AesEndpointFixture) {
        let manifest_uri =
            format!("/hls/shared/live/{}/{}/manifest.m3u8", fixture.proxy_session_id.0, fixture.access_lease_id.0);
        let response = get_response(Arc::clone(&fixture.app_state), &manifest_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal utf8");
        let key = body.find("#EXT-X-KEY:METHOD=AES-128").expect("base AES key");
        let reset = body.find("#EXT-X-KEY:METHOD=NONE").expect("clear key reset");
        let discontinuity = body[reset..].find("#EXT-X-DISCONTINUITY").expect("terminal discontinuity") + reset;
        assert!(key < reset && reset < discontinuity);
        assert!(body.contains("IV=0x00000000000000000000000000000052"));
        assert!(body.contains("#EXT-X-VERSION:5\n"));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
        let key_response = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await;
        assert_eq!(key_response.status(), StatusCode::OK);
        assert_eq!(response_body(key_response).await.as_ref(), AES_TEST_KEY_BYTES);
        assert_eq!(fixture.origin.key_request_count(), 1);
        let range = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, Some("bytes=4-7")).await;
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response_body(range).await.as_ref(), &AES_TEST_KEY_BYTES[4..=7]);
        let unsatisfiable = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, Some("bytes=16-")).await;
        assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(unsatisfiable.headers()[header::CONTENT_RANGE], "bytes */16");
    }

    async fn assert_aes_rotated_live_key(
        fixture: &AesEndpointFixture,
        frozen_source_cache_key: &TransientObjectCacheKey,
    ) -> String {
        let manifest_requests_before = fixture.origin.manifest_request_count();
        let segment_requests_before = fixture.origin.segment_request_count();
        fixture.origin.set_key_bytes(Arc::from(AES_TEST_ROTATED_KEY_BYTES)).await;
        let live_lease_id = HlsAccessLeaseId("encrypted-rotated-live-lease".to_string());
        let access_context = test_hls_access_context(fixture.proxy_session_id.clone(), live_lease_id.clone());
        prepare_pending_test_hls_access_lease(&fixture.app_state, &fixture.proxy_session_id, &live_lease_id).await;
        let response = super::try_hls_cache_canonical_manifest_response(
            &fixture.app_state,
            &test_fingerprint(),
            &access_context,
            &fixture.proxy_session_id,
            &live_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &fixture.request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(&fixture.request_url),
                input: &fixture.input,
                origin_source: super::build_hls_origin_source(&fixture.input, "12345"),
            },
            HeaderMap::new(),
            None,
            "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("a new live lease should reuse the recovered shared session");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.origin.manifest_request_count(), manifest_requests_before.saturating_add(1));
        assert_eq!(fixture.origin.segment_request_count(), segment_requests_before);
        let live_lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&live_lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await
            .expect("rotated live lease snapshot");
        assert_eq!(live_lease.playback_mode, HlsLeasePlaybackMode::Live);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("rotated manifest utf8");
        let rotated_key_uri = body
            .lines()
            .find(|line| line.starts_with("#EXT-X-KEY:METHOD=AES-128"))
            .and_then(|line| line.split_once("URI=\"").map(|(_, tail)| tail))
            .and_then(|tail| tail.split_once('"').map(|(uri, _)| uri.to_string()))
            .expect("rotated live key URI");
        let key_response = get_response(Arc::clone(&fixture.app_state), &rotated_key_uri, None).await;
        assert_eq!(key_response.status(), StatusCode::OK);
        assert_eq!(response_body(key_response).await.as_ref(), AES_TEST_ROTATED_KEY_BYTES);
        assert_eq!(fixture.origin.key_request_count(), 2);
        assert!(fixture
            .app_state
            .hls_proxy
            .segment_cache()
            .metadata(frozen_source_cache_key)
            .await
            .expect("A metadata lookup")
            .is_none());
        let terminal_key = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await;
        assert_eq!(terminal_key.status(), StatusCode::OK);
        assert_eq!(response_body(terminal_key).await.as_ref(), AES_TEST_KEY_BYTES);
        assert_eq!(fixture.origin.key_request_count(), 2);
        rotated_key_uri
    }

    async fn expire_aes_terminal_lease(fixture: &AesEndpointFixture, rotated_key_uri: &str) {
        let expired_at_ms = super::current_time_millis().saturating_sub(1);
        {
            let mut leases = fixture.app_state.hls_proxy.access_leases().write().await;
            let mut lease = leases.remove_access_lease(&fixture.access_lease_id).expect("terminal lease");
            lease.valid_until_ms = expired_at_ms;
            leases.prepare_access_lease(lease);
        }
        fixture
            .app_state
            .hls_proxy
            .handle_lifecycle_event(
                &fixture.app_state.active_users,
                &fixture.app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: fixture.access_lease_id.clone(),
                        proxy_session_id: fixture.proxy_session_id.clone(),
                    },
                    due_at_ms: expired_at_ms,
                },
                super::current_time_millis(),
            )
            .await;
        fixture
            .app_state
            .hls_proxy
            .run_garbage_collection_once(super::current_time_millis())
            .await
            .expect("released GC");
        assert!(fixture.session.read().await.terminal_tail_protection(&fixture.access_lease_id).is_none());
        assert_eq!(
            get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await.status(),
            StatusCode::NOT_FOUND
        );
        let live_key = get_response(Arc::clone(&fixture.app_state), rotated_key_uri, None).await;
        assert_eq!(live_key.status(), StatusCode::OK);
        assert_eq!(response_body(live_key).await.as_ref(), AES_TEST_ROTATED_KEY_BYTES);
    }

    #[tokio::test]
    async fn aes_128_normal_origin_key_and_terminal_lifecycle_are_endpoint_safe() {
        let fixture = aes_endpoint_fixture().await;
        let frozen_source_cache_key = install_aes_terminal_plan(&fixture).await;
        assert_aes_terminal_endpoints(&fixture).await;
        let rotated_key_uri = assert_aes_rotated_live_key(&fixture, &frozen_source_cache_key).await;
        expire_aes_terminal_lease(&fixture, &rotated_key_uri).await;
    }

    #[tokio::test]
    async fn hls_cache_manifest_unpublished_lease_uses_same_finite_fallback_for_created_and_reused_session() {
        let mut rendered_bodies = Vec::new();
        for expected_outcome in [HlsSessionStoreOutcome::Created, HlsSessionStoreOutcome::Reused] {
            let input_name = Arc::<str>::from("test-input");
            let origin = spawn_test_status_origin(StatusCode::NOT_FOUND, b"missing").await;
            let input = ConfigInput {
                id: 1,
                name: Arc::clone(&input_name),
                input_type: InputType::Xtream,
                url: origin.base_url.clone(),
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                max_connections: 1,
                enabled: true,
                ..ConfigInput::default()
            };
            let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
            enable_hls_cache(&app_state);
            enable_channel_unavailable_custom_response(&app_state);
            create_active_hls_user_session(&app_state).await;
            let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
            let origin_source = super::build_hls_origin_source(&input, "12345");
            let session_key = origin_source.session_key();
            let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
            if expected_outcome == HlsSessionStoreOutcome::Reused {
                let (session, outcome) = app_state
                    .hls_proxy
                    .get_or_create_session_with_source_and_outcome(
                        session_key,
                        origin_source.clone(),
                        &app_state.get_encrypt_secret(),
                        super::current_time_millis(),
                    )
                    .await;
                assert_eq!(outcome, HlsSessionStoreOutcome::Created);
                session.write().await.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
            }
            let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
            let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
            prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

            let response = super::try_hls_cache_canonical_manifest_response(
                &app_state,
                &test_fingerprint(),
                &access_context,
                &proxy_session_id,
                &access_lease_id,
                HlsAccessLeaseState::Pending,
                super::HlsCacheManifestOrigin {
                    raw_request_url: request_url.as_str(),
                    session_entry_url: super::HlsOriginEntryUrl::direct_http(request_url.as_str()),
                    input: &input,
                    origin_source,
                },
                HeaderMap::new(),
                None,
                "/live/hls-user/hls-pass/12345.m3u8",
                super::HlsManifestRefreshOrdering::Background,
            )
            .await
            .expect("hls cache should handle valid live hls entrypoint");

            assert_eq!(response.status(), StatusCode::OK, "session outcome: {expected_outcome:?}");
            assert!(
                response.headers().get(header::RETRY_AFTER).is_none(),
                "custom response must not expose retry-after"
            );
            assert!(!response.headers().contains_key(header::LOCATION));
            let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
            assert!(body.contains("#EXT-X-ENDLIST"));
            assert!(!body.contains("/hls/shared/live/"), "standalone response must not expose an unready normal URI");
            rendered_bodies.push(body);

            let snapshot = app_state
                .hls_proxy
                .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
                .await
                .expect("lease remains available for strict cold-start handling");
            assert_eq!(snapshot.playback_mode, HlsLeasePlaybackMode::Live);
            assert!(snapshot.last_manifest_snapshot.is_none());
        }
        assert_eq!(rendered_bodies[0], rendered_bodies[1]);
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_client_abort_does_not_leave_refresh_in_flight() {
        let origin = spawn_test_transient_origin_with_delayed_response(
            "200 OK",
            &[("Content-Type", "application/vnd.apple.mpegurl")],
            "#EXTM3U\n#EXT-X-VERSION:3\n",
            Duration::from_millis(100),
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let hls_dto =
            HlsCacheConfigDto { origin_manifest_timeout_ms: shared::model::Millis::new(1_000), ..Default::default() };
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy_and_inputs(
            Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)),
            vec![Arc::new(input.clone())],
        );
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let app_state_for_request = Arc::clone(&app_state);
        let request_handle = tokio::spawn(async move {
            super::try_hls_cache_canonical_manifest_response(
                &app_state_for_request,
                &test_fingerprint(),
                &access_context,
                &proxy_session_id,
                &access_lease_id,
                HlsAccessLeaseState::Pending,
                super::HlsCacheManifestOrigin {
                    raw_request_url: &request_url,
                    session_entry_url: super::HlsOriginEntryUrl::direct_http(&request_url),
                    input: &input,
                    origin_source,
                },
                HeaderMap::new(),
                None,
                "/live/hls-user/hls-pass/12345.m3u8",
                super::HlsManifestRefreshOrdering::Background,
            )
            .await
        });

        let session = wait_for_hls_test_session(&app_state, &session_key).await;
        wait_for_hls_refresh_in_flight(&session).await;
        request_handle.abort();
        let _ = request_handle.await;

        for _ in 0..200 {
            if !session.read().await.origin_refresh.in_flight {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let session = session.read().await;
        assert!(!session.origin_refresh.in_flight);
        assert!(session.origin_refresh.last_fetch_finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_canonical_prepare_service_unavailable_sets_retry_after() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("alias-input"),
            aliases: Some(vec![crate::model::ConfigInputAlias {
                id: 2,
                name: Arc::from("alias-account"),
                url: "http://alias.example.com".to_string(),
                username: Some("alias-user".to_string()),
                password: Some("alias-pass".to_string()),
                priority: 0,
                max_connections: 1,
                exp_date: None,
                enabled: true,
                stalker: None,
            }]),
            ..ConfigInput::default()
        };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("canonical hls cache response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry after"), "2");
    }

    #[tokio::test]
    async fn hls_origin_account_rebind_failure_sets_backoff_without_changing_session_identity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let input = ConfigInput { id: 1, name: Arc::from("stale-input"), ..ConfigInput::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key.clone(),
                origin_source.clone(),
                &app_state.get_encrypt_secret(),
                1_000,
            )
            .await;
        {
            let mut session_guard = session.write().await;
            session_guard.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from("removed-account"),
                &proxy_session_id,
                1_000,
            ));
        }

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: request_url,
                session_entry_url: super::HlsOriginEntryUrl::direct_http(request_url),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("canonical hls cache response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry after"), "2");
        let session_guard = session.read().await;
        assert_eq!(session_guard.key, session_key);
        assert_eq!(session_guard.proxy_session_id, proxy_session_id);
        let binding = session_guard.origin_account_binding.as_ref().expect("stale binding remains");
        assert_eq!(binding.account_name.as_ref(), "removed-account");
        assert_eq!(binding.generation, 0);
        assert_eq!(session_guard.origin_account_rebind.consecutive_rebind_failures, 1);
        assert!(session_guard.origin_account_rebind.next_rebind_allowed_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_entry_returns_master_playlist_without_origin_refresh_or_session_creation() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();

        let response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            Some("/iptv"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
            Some("application/vnd.apple.mpegurl")
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).and_then(|value| value.to_str().ok()),
            Some("private, no-store, no-cache, must-revalidate")
        );
        assert!(response.headers().get(header::LOCATION).is_none());
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(response.headers().get(header::VARY).is_none());
        let content_length = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .expect("content length");
        let body = response_body(response).await;
        assert_eq!(content_length, body.len());
        let body = std::str::from_utf8(&body).expect("master playlist should be UTF-8");
        assert_eq!(body.matches("/iptv").count(), 1);
        let variant_uri = body.lines().nth(2).expect("single variant URI");
        assert!(variant_uri.starts_with("/iptv/hls/shared/live/"));
        assert!(variant_uri.ends_with("/manifest.m3u8"));
        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(variant_uri).to_string());
        assert_eq!(access_lease_id.0.len(), 22);
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(variant_uri).to_string());
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .await
            .expect("entry access lease");
        assert!(app_state
            .active_users
            .get_and_update_user_session(&lease.username, &lease.user_session_token)
            .await
            .is_some());
        assert!(app_state.hls_proxy.sessions().get_by_key(&session_key).await.is_none());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[test]
    fn hls_master_playlist_response_diagnostic_reports_complete_response_contract_for_every_bandwidth_source() {
        for selection in [
            super::HlsMasterBandwidthSelection::resolve(Some(2_500_000), Some(2_000_000)),
            super::HlsMasterBandwidthSelection::resolve(None, Some(2_000_000)),
            super::HlsMasterBandwidthSelection::resolve(None, None),
        ] {
            let rendered = super::hls_entry_master_playlist_response(
                &ProxySessionId("diagnostic-proxy-session".to_string()),
                &HlsAccessLeaseId("diagnostic-lease".to_string()),
                selection.bandwidth(),
                Some("/iptv"),
            );
            let fields = super::HlsMasterPlaylistResponseDiagnostic {
                lease: "lease-safe".to_string(),
                session: "session-safe".to_string(),
                proxy_session: "proxy-safe".to_string(),
                user_session: "user-safe".to_string(),
                virtual_id: 12345,
                bandwidth_bps: selection.bandwidth().advertised_bps(),
                bandwidth_source: selection.source().as_log_value(),
                content_length: rendered.content_length,
            }
            .to_string();

            assert!(fields.contains("lease=lease-safe session=session-safe proxy_session=proxy-safe"));
            assert!(fields.contains("user_session=user-safe virtual_id=12345"));
            assert!(fields.contains(&format!(
                "bandwidth_bps={} bandwidth_source={}",
                selection.bandwidth().advertised_bps(),
                selection.source().as_log_value()
            )));
            assert!(fields.contains("status=200"));
            assert!(fields.ends_with(&format!("content_length={}", rendered.content_length)));
            assert_eq!(
                rendered
                    .response
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<usize>().ok()),
                Some(rendered.content_length)
            );
        }
    }

    struct SharedRequestFlowFixture {
        _origin: TestSegmentOrigin,
        app_state: Arc<AppState>,
        input: ConfigInput,
        target: ConfigTarget,
        user: ProxyUserCredentials,
        origin_manifest_url: String,
        entry_path: String,
    }

    async fn shared_request_flow_fixture() -> SharedRequestFlowFixture {
        const MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n\
            #EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n\
            #EXTINF:4.0,\n000125.ts\n#EXTINF:4.0,\n000126.ts\n#EXTINF:4.0,\n000127.ts\n\
            #EXTINF:4.0,\n000128.ts\n";
        let origin = spawn_test_segment_origin(MANIFEST).await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("request-flow-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let mut target = test_m3u_hls_share_target();
        target.name = "default".to_string();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        configure_default_test_server(&app_state);
        store_test_sources_with_target(&app_state, input.clone(), target.clone());
        let origin_manifest_url = format!("{}/channel/index.m3u8", origin.base_url);
        cache_test_m3u_hls_item(
            &app_state,
            &target,
            test_m3u_hls_item(&input, 12345, "channel-a", &origin_manifest_url),
        )
        .await;
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.password = "hls-pass".to_string();
        let entry_path = super::build_virtual_hls_entry_path(&target, &input, &user, 12345);
        SharedRequestFlowFixture { _origin: origin, app_state, input, target, user, origin_manifest_url, entry_path }
    }

    async fn shared_request_flow_entry(
        fixture: &SharedRequestFlowFixture,
        fingerprint: &Fingerprint,
    ) -> Response<Body> {
        super::handle_hls_stream_request(
            fingerprint,
            &fixture.app_state,
            &fixture.user,
            &fixture.target,
            None,
            None,
            &fixture.origin_manifest_url,
            None,
            test_hls_entry_stream_context(12345, "channel-a", Some(2_500_000)),
            &fixture.input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &fixture.entry_path,
        )
        .await
        .into_response()
    }

    #[tokio::test]
    async fn hls_cache_archive_entry_uses_distinct_identity_and_preserves_origin() -> Result<(), &'static str> {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        configure_default_test_server(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.password = "hls-pass".to_string();
        let input = test_hls_input();
        let target = test_hls_share_target(true);
        let archive_url = "http://origin.example.com/live/user/pass/timeshift_abs-1784898000.m3u8";

        let response = super::handle_hls_stream_request(
            &test_fingerprint(),
            &app_state,
            &user,
            &target,
            None,
            None,
            archive_url,
            Some(1_784_898_000),
            test_hls_entry_stream_context(12345, "80510", None),
            &input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &super::build_virtual_hls_entry_path(&target, &input, &user, 12345),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let (_, media_playlist_uri) = single_variant_master_playlist(response).await;
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&media_playlist_uri).to_string());
        let live_source = super::build_hls_origin_source(&input, "80510");
        let archive_source =
            super::build_hls_origin_source_for_playback(&input, "80510", Some(1_784_898_000), Some(archive_url));
        assert_ne!(
            proxy_session_id,
            build_proxy_session_id(&live_source.session_key(), &app_state.get_encrypt_secret())
        );
        assert_eq!(
            proxy_session_id,
            build_proxy_session_id(&archive_source.session_key(), &app_state.get_encrypt_secret())
        );

        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&media_playlist_uri).to_string());
        let lease = app_state
            .hls_proxy
            .access_leases()
            .write()
            .await
            .response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .ok_or("archive access lease")?;
        assert_eq!(lease.stream_ref, "80510");
        assert_eq!(lease.epg_reference_ts, Some(1_784_898_000));
        assert_eq!(lease.archive_origin_url.as_deref(), Some(archive_url));
        assert!(super::is_m3u_catchup_session_token(&lease.user_session_token));
        Ok(())
    }

    #[tokio::test]
    async fn shared_hls_request_flow_keeps_media_playlist_lease_bound_across_reloads() {
        let fixture = shared_request_flow_fixture().await;
        let app_state = &fixture.app_state;
        let entry_response = shared_request_flow_entry(&fixture, &test_fingerprint()).await;

        assert_eq!(entry_response.status(), StatusCode::OK);
        assert!(!entry_response.headers().contains_key(header::LOCATION));
        let (_, media_playlist_uri) = single_variant_master_playlist(entry_response).await;
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&media_playlist_uri).to_string());
        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&media_playlist_uri).to_string());

        let first_media_response = get_response(Arc::clone(app_state), &media_playlist_uri, None).await;
        assert_eq!(first_media_response.status(), StatusCode::OK);
        assert!(!first_media_response.headers().contains_key(header::LOCATION));
        let first_media_body =
            String::from_utf8(response_body(first_media_response).await.to_vec()).expect("media playlist utf8");
        let segment_uri = first_media_body
            .lines()
            .find(|line| line.starts_with("/hls/shared/live/") && path_has_extension(line, "ts"))
            .expect("lease-bound segment URI")
            .to_string();
        let lease_path = format!("/{}/{}/", proxy_session_id.0, access_lease_id.0);
        assert!(segment_uri.contains(&lease_path));

        let reloaded_media_response = get_response(Arc::clone(app_state), &media_playlist_uri, None).await;
        assert_eq!(reloaded_media_response.status(), StatusCode::OK);
        assert!(!reloaded_media_response.headers().contains_key(header::LOCATION));
        let reloaded_media_body = String::from_utf8(response_body(reloaded_media_response).await.to_vec())
            .expect("reloaded media playlist utf8");
        assert_eq!(manifest_media_sequence(&reloaded_media_body), manifest_media_sequence(&first_media_body));
        assert!(reloaded_media_body.lines().any(|line| line == segment_uri));
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 1);

        let segment_response = get_response(Arc::clone(app_state), &segment_uri, None).await;
        assert_eq!(segment_response.status(), StatusCode::OK);
        assert!(!response_body(segment_response).await.is_empty());
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .await
            .expect("request-flow access lease");
        assert_eq!(lease.state, HlsAccessLeaseState::Activated);
        assert!(lease.last_manifest_snapshot.is_some());

        let second_entry_response =
            shared_request_flow_entry(&fixture, &test_fingerprint_with_addr(test_addr_with_port(55131))).await;
        assert_eq!(second_entry_response.status(), StatusCode::OK);
        let (_, second_media_playlist_uri) = single_variant_master_playlist(second_entry_response).await;
        let second_proxy_session_id =
            ProxySessionId(proxy_session_id_from_variant_uri(&second_media_playlist_uri).to_string());
        let second_access_lease_id =
            HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_media_playlist_uri).to_string());
        assert_eq!(second_proxy_session_id, proxy_session_id);
        assert_ne!(second_access_lease_id, access_lease_id);
        let second_pending_lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &second_access_lease_id,
                &second_proxy_session_id,
                super::current_time_millis(),
            )
            .await
            .expect("second pending request-flow lease");
        assert_eq!(second_pending_lease.state, HlsAccessLeaseState::Pending);
        assert!(second_pending_lease.last_manifest_snapshot.is_none());
        assert_ne!(second_pending_lease.user_session_token, lease.user_session_token);

        let second_media_response = get_response(Arc::clone(app_state), &second_media_playlist_uri, None).await;
        assert_eq!(second_media_response.status(), StatusCode::OK);
        assert!(!second_media_response.headers().contains_key(header::LOCATION));
        let second_media_body =
            String::from_utf8(response_body(second_media_response).await.to_vec()).expect("second media playlist utf8");
        assert_eq!(manifest_media_sequence(&second_media_body), manifest_media_sequence(&first_media_body));
        let second_published_lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &second_access_lease_id,
                &second_proxy_session_id,
                super::current_time_millis(),
            )
            .await
            .expect("second published request-flow lease");
        assert!(second_published_lease.last_manifest_snapshot.is_some());
        assert_eq!(second_published_lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert_eq!(app_state.hls_proxy.sessions().len().await, 1);
    }

    struct PublicationLateFixture {
        origin: TestSegmentOrigin,
        origin_phase: Arc<AtomicUsize>,
        app_state: Arc<AppState>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        access_lease_id: HlsAccessLeaseId,
        media_playlist_uri: String,
    }

    async fn publication_late_fixture() -> PublicationLateFixture {
        let initial_manifest = Arc::<[u8]>::from(regression_origin_manifest(123, 6));
        let progressed_manifest = Arc::<[u8]>::from(regression_origin_manifest(124, 6));
        let segment = Arc::<[u8]>::from(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .as_slice(),
        );
        let origin_phase = Arc::new(AtomicUsize::new(0));
        let origin_phase_for_handler = Arc::clone(&origin_phase);
        let origin = spawn_test_binary_origin(Arc::new(move |path| {
            if path_has_extension(path, "m3u8") {
                return match origin_phase_for_handler.load(Ordering::SeqCst) {
                    0 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&initial_manifest)),
                    1 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&progressed_manifest)),
                    _ => TestBinaryOriginResponse::new(
                        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                        Arc::<[u8]>::from(&b"retry"[..]),
                    ),
                };
            }
            TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&segment))
        }))
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("publication-late-request-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let mut target = test_m3u_hls_share_target();
        target.name = "default".to_string();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        enable_channel_unavailable_custom_response(&app_state);
        configure_default_test_server(&app_state);
        store_test_sources_with_target(&app_state, input.clone(), target.clone());
        let origin_manifest_url = format!("{}/channel/index.m3u8", origin.base_url);
        cache_test_m3u_hls_item(
            &app_state,
            &target,
            test_m3u_hls_item(&input, 12345, "channel-a", &origin_manifest_url),
        )
        .await;
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.password = "hls-pass".to_string();
        let entry_path = super::build_virtual_hls_entry_path(&target, &input, &user, 12345);
        let entry_response = super::handle_hls_stream_request(
            &test_fingerprint(),
            &app_state,
            &user,
            &target,
            None,
            None,
            &origin_manifest_url,
            None,
            test_hls_entry_stream_context(12345, "channel-a", None),
            &input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &entry_path,
        )
        .await
        .into_response();
        let (_, media_playlist_uri) = single_variant_master_playlist(entry_response).await;
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&media_playlist_uri).to_string());
        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&media_playlist_uri).to_string());
        let initial_media_response = get_response(Arc::clone(&app_state), &media_playlist_uri, None).await;
        assert_eq!(initial_media_response.status(), StatusCode::OK);
        let initial_media_body =
            String::from_utf8(response_body(initial_media_response).await.to_vec()).expect("initial media utf8");
        let last_segment_uri = initial_media_body
            .lines()
            .rfind(|line| line.starts_with("/hls/shared/live/") && path_has_extension(line, "ts"))
            .expect("initial manifest segment");
        assert_eq!(get_status(Arc::clone(&app_state), last_segment_uri).await, StatusCode::OK);
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("shared publication-late session");
        PublicationLateFixture {
            origin,
            origin_phase,
            app_state,
            session,
            proxy_session_id,
            access_lease_id,
            media_playlist_uri,
        }
    }

    async fn refresh_publication_late_fixture(fixture: &PublicationLateFixture) -> HlsAccessLease {
        let progress_generation_before = fixture.session.read().await.origin_control.progress_generation;
        {
            let mut session = fixture.session.write().await;
            session.origin_control.last_media_progress_at_ms = Some(0);
            session.origin_refresh.next_fetch_allowed_at_ms = 0;
        }
        let requests_before = fixture.origin.manifest_request_count();
        fixture.origin_phase.store(1, Ordering::SeqCst);
        let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("refreshed media utf8");
        assert!(!body.contains("#EXT-X-ENDLIST"));
        assert!(!body.contains("/terminal/"));
        assert!(fixture.origin.manifest_request_count() > requests_before);
        {
            let session = fixture.session.read().await;
            assert!(session.origin_control.progress_generation > progress_generation_before);
            assert_eq!(session.origin_seq_highwater, Some(129));
        }
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                super::current_time_millis(),
            )
            .await
            .expect("publication-late lease remains stored");
        assert_eq!(lease.state, HlsAccessLeaseState::Activated);
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert_eq!(fixture.app_state.hls_proxy.terminal_pending().owner_count(), 0);
        lease
    }

    async fn prepare_publication_late_terminal_pressure(
        fixture: &PublicationLateFixture,
        base_manifest: &HlsLeaseManifestSnapshot,
    ) {
        let terminal_response = fixture.app_state.app_config.custom_stream_response.load_full();
        let terminal_asset = snapshot_terminal_media_asset(
            terminal_response
                .as_ref()
                .and_then(|response| response.channel_unavailable.as_ref())
                .expect("configured terminal asset"),
        )
        .expect("compatible terminal asset");
        let bundle_key = prepared_terminal_bundle_key(
            &terminal_asset,
            base_manifest.target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = fixture.app_state.hls_proxy.start_prepared_terminal_bundle(
            terminal_asset,
            base_manifest.target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = match state {
            HlsPreparedTerminalBundleState::Preparing { .. } => fixture
                .app_state
                .hls_proxy
                .wait_for_prepared_terminal_bundle(bundle_key)
                .await
                .expect("terminal bundle completion"),
            state => state,
        };
        assert!(matches!(state, HlsPreparedTerminalBundleState::Ready { .. }));
        let mut session = fixture.session.write().await;
        for segment in session.segments.values_mut().filter(|segment| segment.proxy_seq > base_manifest.last_proxy_seq)
        {
            segment.duration_ms = 1;
        }
        session.origin_control.last_media_progress_at_ms = Some(0);
        session.origin_refresh.next_fetch_allowed_at_ms = 0;
    }

    async fn assert_publication_late_terminal_result(fixture: &PublicationLateFixture) {
        let requests_before = fixture.origin.manifest_request_count();
        fixture.origin_phase.store(2, Ordering::SeqCst);
        let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                super::current_time_millis(),
            )
            .await
            .expect("terminal lease remains stored");
        let recovery_plan = fixture.app_state.hls_proxy.manifest_recovery_burst().level.plan();
        assert!(
            fixture.origin.manifest_request_count().saturating_sub(requests_before) >= recovery_plan.total_candidates()
        );
        match lease.playback_mode {
            HlsLeasePlaybackMode::TerminalTail(_) => {
                assert_eq!(response.status(), StatusCode::OK);
                let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest utf8");
                assert!(body.contains("#EXT-X-ENDLIST"));
                assert!(body.contains("/terminal/"));
            }
            HlsLeasePlaybackMode::TerminalUnavailable { .. } => {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            }
            HlsLeasePlaybackMode::Live => {
                assert_eq!(response.status(), StatusCode::OK);
                let body = String::from_utf8(response_body(response).await.to_vec()).expect("live manifest utf8");
                assert!(body.starts_with("#EXTM3U"));
                assert!(!body.contains("#EXT-X-ENDLIST"));
            }
            HlsLeasePlaybackMode::Ended => {
                panic!("active hard-failed lease must not become ended without terminal publication")
            }
        }
    }

    #[tokio::test]
    async fn publication_late_live_manifest_request_refreshes_before_terminal_evaluation() {
        let fixture = publication_late_fixture().await;
        let lease = refresh_publication_late_fixture(&fixture).await;
        let base_manifest = lease.last_manifest_snapshot.as_ref().expect("live manifest snapshot");
        prepare_publication_late_terminal_pressure(&fixture, base_manifest).await;
        assert_publication_late_terminal_result(&fixture).await;
    }

    #[test]
    fn hls_entry_stream_context_keeps_only_positive_live_bitrate() {
        let input = ConfigInput { name: Arc::from("m3u-input"), ..ConfigInput::default() };
        let mut item = test_m3u_hls_item(&input, 1001, "channel-a", "http://origin.example.com/live.m3u8");
        item.additional_properties = Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
            bitrate: 2_500_000,
            ..shared::model::LiveStreamProperties::default()
        })));

        let context = super::HlsEntryStreamContext::from_playlist_item(&item).expect("entry stream context");
        assert_eq!(context.virtual_id(), 1001);
        assert_eq!(context.stream_ref(), "channel-a");
        assert_eq!(context.known_bitrate_bps(), Some(2_500_000));

        if let Some(StreamProperties::Live(properties)) = item.additional_properties.as_mut() {
            properties.bitrate = 0;
        }
        assert_eq!(
            super::HlsEntryStreamContext::from_playlist_item(&item)
                .expect("entry stream context without bitrate")
                .known_bitrate_bps(),
            None
        );
    }

    #[tokio::test]
    async fn hls_cache_entry_prefers_item_bitrate_then_loads_db_without_target_rebuild() {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut config = app_state.app_config.config.load().as_ref().clone();
        config.storage_dir = temp.path().to_string_lossy().into_owned();
        app_state.app_config.config.store(Arc::new(config));
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input =
            ConfigInput { id: 9, name: Arc::from("m3u-input"), input_type: InputType::M3u, ..ConfigInput::default() };
        let mut stored_item = test_m3u_hls_item(&input, 70001, "channel-a", "http://origin.example.com/live.m3u8");
        stored_item.additional_properties =
            Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
                bitrate: 2_500_000,
                ..shared::model::LiveStreamProperties::default()
            })));
        let input_storage = crate::repository::build_input_storage_path(&input.name, &temp.path().to_string_lossy());
        std::fs::create_dir_all(&input_storage).expect("input storage");
        let db_path = crate::repository::get_input_m3u_playlist_file_path(&input_storage, &input.name);
        let mut tree = crate::repository::BPlusTree::new();
        tree.insert(Arc::<str>::from("channel-a"), stored_item);
        tree.store(&db_path).expect("input M3U metadata");
        let request_url = "http://origin.example.com/live.m3u8";

        let db_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            super::build_hls_origin_source(&input, "channel-a"),
            70001,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let (db_bandwidth, db_variant_uri) = single_variant_master_playlist(db_response).await;
        assert_eq!(db_bandwidth, 3_000_000);
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&db_variant_uri).to_string());
        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&db_variant_uri).to_string());
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .await
            .expect("DB-backed access lease");
        assert_eq!(lease.known_bitrate_bps, Some(2_500_000));

        std::fs::write(&db_path, b"invalid metadata tree").expect("corrupt test metadata");
        let item_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint_with_addr(test_addr_with_port(55127)),
            &user,
            super::build_hls_origin_source(&input, "channel-a"),
            70001,
            None,
            Some(3_000_000),
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let (item_bandwidth, _) = single_variant_master_playlist(item_response).await;
        assert_eq!(item_bandwidth, 3_600_000);

        let fallback_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint_with_addr(test_addr_with_port(55128)),
            &user,
            super::build_hls_origin_source(&input, "channel-a"),
            70001,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let (fallback_bandwidth, _) = single_variant_master_playlist(fallback_response).await;
        assert_eq!(fallback_bandwidth, 1_000_000);
    }

    #[tokio::test]
    async fn hls_cache_entry_denies_access_lease_for_grace_without_slot_and_exhausted() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        for (connection_permission, connection_kind) in [
            (UserConnectionPermission::GracePeriod, None),
            (UserConnectionPermission::Exhausted, Some(ConnectionKind::Normal)),
        ] {
            let response = super::create_hls_cache_entry_master_playlist_response(
                &app_state,
                &test_fingerprint(),
                &user,
                origin_source.clone(),
                12345,
                None,
                None,
                None,
                request_url,
                &input,
                connection_permission,
                connection_kind,
                Some("/iptv"),
            )
            .await;

            assert_eq!(response.status(), StatusCode::OK);
            let variant_uri = single_variant_uri(response).await;
            let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&variant_uri).to_string());
            let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&variant_uri).to_string());
            let now_ms = super::current_time_millis();
            let snapshot = app_state
                .hls_proxy
                .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms)
                .await
                .expect("denied lease should stay available for response rendering");
            assert_eq!(snapshot.state, HlsAccessLeaseState::Denied);
            assert!(app_state
                .hls_proxy
                .access_lease_session_snapshot(&proxy_session_id, now_ms)
                .await
                .effective_origin_policy
                .is_none());

            let err = super::validate_hls_proxy_access_context(
                &app_state,
                &test_fingerprint(),
                &proxy_session_id,
                &access_lease_id.0,
                now_ms,
                HlsAccessAdmissionMode::ManifestPrepare,
            )
            .await
            .expect_err("denied lease must surface as admission denied");
            assert!(matches!(err, HlsAccessLeaseValidationError::AdmissionDenied { runtime_tail: None, .. }));
        }
    }

    #[tokio::test]
    async fn hls_cache_entry_master_playlist_uses_cache_when_target_hls_share_enabled() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        configure_default_test_server(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.password = "hls-pass".to_string();
        let input = test_hls_input();
        let target = test_hls_share_target(true);
        let original_hls_entry_path = super::build_virtual_hls_entry_path(&target, &input, &user, 1001);

        let response = super::handle_hls_stream_request(
            &test_fingerprint(),
            &app_state,
            &user,
            &target,
            None,
            None,
            "http://origin.example.com/live/user/pass/1001.m3u8",
            None,
            test_hls_entry_stream_context(1001, "80510", Some(2_500_000)),
            &input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &original_hls_entry_path,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let (bandwidth, variant_uri) = single_variant_master_playlist(response).await;
        assert_eq!(bandwidth, 3_000_000);
        assert!(variant_uri.starts_with("/hls/shared/live/"));
        assert!(variant_uri.ends_with("/manifest.m3u8"));
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 1);

        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&variant_uri).to_string());
        let origin_key = HlsSessionKey::new(input.id, "80510");
        let virtual_id_key = HlsSessionKey::new(input.id, "1001");
        assert_eq!(origin_key.stable_value(), "input:1|hls|80510");
        assert_eq!(proxy_session_id, build_proxy_session_id(&origin_key, &app_state.get_encrypt_secret()));
        assert_ne!(proxy_session_id, build_proxy_session_id(&virtual_id_key, &app_state.get_encrypt_secret()));

        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&variant_uri).to_string());
        let lease = app_state
            .hls_proxy
            .access_leases()
            .write()
            .await
            .response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .expect("access lease");
        assert_eq!(lease.stream_ref, "80510");
        assert_eq!(lease.virtual_id, 1001);
        assert_eq!(lease.known_bitrate_bps, Some(2_500_000));
    }

    #[tokio::test]
    async fn hls_cache_entry_shares_content_session_across_targets_but_keeps_distinct_virtual_leases() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        configure_default_test_server(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.password = "hls-pass".to_string();
        let input = test_hls_input();
        let first_target = test_hls_share_target(true);
        let mut second_target = test_hls_share_target(true);
        second_target.id = 2;

        let first_response = super::handle_hls_stream_request(
            &test_fingerprint(),
            &app_state,
            &user,
            &first_target,
            None,
            None,
            "http://origin.example.com/live/user/pass/1001.m3u8",
            None,
            test_hls_entry_stream_context(1001, "80510", None),
            &input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &super::build_virtual_hls_entry_path(&first_target, &input, &user, 1001),
        )
        .await
        .into_response();
        let second_response = super::handle_hls_stream_request(
            &test_fingerprint_with_addr(test_addr_with_port(55124)),
            &app_state,
            &user,
            &second_target,
            None,
            None,
            "http://origin.example.com/live/user/pass/9007.m3u8",
            None,
            test_hls_entry_stream_context(9007, "80510", None),
            &input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &super::build_virtual_hls_entry_path(&second_target, &input, &user, 9007),
        )
        .await
        .into_response();

        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(second_response.status(), StatusCode::OK);
        let first_variant_uri = single_variant_uri(first_response).await;
        let second_variant_uri = single_variant_uri(second_response).await;
        assert_eq!(
            proxy_session_id_from_variant_uri(&first_variant_uri),
            proxy_session_id_from_variant_uri(&second_variant_uri)
        );
        assert_ne!(
            access_lease_id_from_variant_uri(&first_variant_uri),
            access_lease_id_from_variant_uri(&second_variant_uri)
        );

        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
        let first_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
        let second_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());
        let now_ms = super::current_time_millis();
        let mut leases = app_state.hls_proxy.access_leases().write().await;
        let first_lease =
            leases.response_snapshot(&first_lease_id, &proxy_session_id, now_ms).expect("first access lease");
        let second_lease =
            leases.response_snapshot(&second_lease_id, &proxy_session_id, now_ms).expect("second access lease");
        assert_eq!(first_lease.virtual_id, 1001);
        assert_eq!(second_lease.virtual_id, 9007);
        assert_eq!(first_lease.stream_ref, "80510");
        assert_eq!(second_lease.stream_ref, "80510");
    }

    #[tokio::test]
    async fn hls_virtual_source_resolver_rejects_missing_input_stream_id_with_service_unavailable() {
        let input = single_hls_provider_input("missing-origin-id-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let target = Arc::new(test_m3u_hls_share_target());
        let item = test_m3u_hls_item(
            &input,
            1001,
            "",
            "http://account.example.com/live/account-user/account-pass/channel.m3u8",
        );
        cache_test_m3u_hls_item(&app_state, &target, item).await;

        let status = super::resolve_hls_virtual_source_for_target(&app_state, &target, 1001)
            .await
            .expect_err("missing input stream identity must fail safely");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn hls_cache_entry_uses_legacy_path_when_target_hls_share_disabled() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        configure_default_test_server(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.password = "hls-pass".to_string();
        let input = test_hls_input();
        let target = test_hls_share_target(false);
        let original_hls_entry_path = super::build_virtual_hls_entry_path(&target, &input, &user, 12345);

        let response = super::handle_hls_stream_request(
            &test_fingerprint(),
            &app_state,
            &user,
            &target,
            None,
            None,
            "http://origin.example.com/live/user/pass/12345.m3u8",
            None,
            test_hls_entry_stream_context(12345, "80510", None),
            &input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &original_hls_entry_path,
        )
        .await
        .into_response();

        let location = response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).unwrap_or("");
        assert!(!location.contains("/hls/shared/live/"));
        assert!(app_state.hls_proxy.access_leases().read().await.is_empty());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
    }

    #[tokio::test]
    async fn legacy_hls_token_route_renders_channel_unavailable_inline_when_target_hls_share_enabled() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        app_state.app_config.config.store(Arc::new(Config {
            custom_stream_response_enabled: true,
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                hls_cache: Some(HlsCacheConfigDto::default()),
                ..Default::default()
            })),
            ..Default::default()
        }));
        configure_default_test_server(&app_state);
        enable_channel_unavailable_custom_response(&app_state);
        let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
        let input = test_hls_input();
        let target = test_hls_share_target(true);
        store_test_sources_with_target(&app_state, input.clone(), target.clone());
        let encrypt_secret = app_state.get_encrypt_secret();
        let legacy_manifest = rewrite_hls(
            &user,
            &RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: "",
                content: "#EXTM3U\n#EXTINF:4.0,\nseg.ts\n",
                hls_url: "http://origin.example.com/live/user/pass/12345.m3u8".to_string(),
                target_id: target.id,
                virtual_id: 12345,
                input_id: input.id,
                user_token: Some("legacy-session-token"),
            },
        );
        let token = legacy_manifest
            .lines()
            .find_map(|line| line.rsplit_once('/').map(|(_, token)| token.trim().to_string()))
            .expect("legacy hls segment token should be rendered");

        let response = super::hls_api_stream_resolved(
            test_fingerprint(),
            HeaderMap::new(),
            Arc::clone(&app_state),
            Arc::clone(&user),
            Arc::new(target),
            input.id,
            12345,
            token,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
        assert!(app_state.hls_proxy.access_leases().read().await.is_empty());
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn legacy_hls_token_route_with_invalid_token_returns_bad_request_when_target_hls_share_enabled() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
        let input = test_hls_input();
        let target = test_hls_share_target(true);
        store_test_sources_with_target(&app_state, input.clone(), target.clone());

        let response = super::hls_api_stream_resolved(
            test_fingerprint(),
            HeaderMap::new(),
            Arc::clone(&app_state),
            user,
            Arc::new(target),
            input.id,
            12345,
            "not-a-valid-legacy-token.ts".to_string(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(app_state.hls_proxy.access_leases().read().await.is_empty());
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[test]
    fn cache_enabled_legacy_hls_route_only_allows_existing_m3u_catchup_session() {
        assert!(super::legacy_hls_route_allowed_with_cache(
            true,
            Some("m3u-catchup|session"),
            Some("m3u-catchup|session")
        ));
        assert!(super::legacy_hls_route_allowed_with_cache(true, Some("catchup|session"), Some("catchup|session")));
        assert!(!super::legacy_hls_route_allowed_with_cache(
            true,
            Some("m3u-catchup|session"),
            Some("m3u-catchup|other")
        ));
        assert!(!super::legacy_hls_route_allowed_with_cache(true, Some("legacy-session"), Some("legacy-session")));
        assert!(!super::legacy_hls_route_allowed_with_cache(true, None, None));
        assert!(super::legacy_hls_route_allowed_with_cache(false, None, None));
    }

    #[tokio::test]
    async fn hls_cache_canonical_manifest_rejects_when_target_hls_share_disabled() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let input = test_hls_input();
        let target = test_hls_share_target(false);
        store_test_sources_with_target(&app_state, input, target);
        let access_context = HlsAccessContext {
            username: "hls-user".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id: ProxySessionId("proxy-session".to_string()),
            input_id: 1,
            stream_ref: "12345".to_string(),
            virtual_id: 12345,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let Err(err) =
            super::resolve_hls_playback_manifest_request_context(&app_state, &access_context, &HeaderMap::new()).await
        else {
            panic!("disabled target hls sharing should reject canonical cache path");
        };

        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hls_cache_entry_leases_update_effective_origin_acquire_policy_for_shared_session() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut soft_user = ProxyUserCredentials::default();
        soft_user.username = "soft-user".to_string();
        soft_user.soft_priority = 20;
        let mut normal_user = ProxyUserCredentials::default();
        normal_user.username = "normal-user".to_string();
        normal_user.priority = -5;
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());

        let soft_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &soft_user,
            origin_source.clone(),
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Soft),
            None,
        )
        .await;
        assert_eq!(soft_response.status(), StatusCode::OK);
        let soft_snapshot =
            app_state.hls_proxy.access_lease_session_snapshot(&proxy_session_id, super::current_time_millis()).await;
        let soft_policy = soft_snapshot.effective_origin_policy.expect("soft policy");
        assert_eq!(soft_policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(soft_policy.priority, soft_user.soft_priority);

        // Different user/family, same shared HLS session. Normal media admission must upgrade
        // the future origin-account acquire policy without changing the shared session identity.
        let normal_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &normal_user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        assert_eq!(normal_response.status(), StatusCode::OK);
        let normal_snapshot =
            app_state.hls_proxy.access_lease_session_snapshot(&proxy_session_id, super::current_time_millis()).await;
        let normal_policy = normal_snapshot.effective_origin_policy.expect("normal policy");
        assert_eq!(normal_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(normal_policy.priority, normal_user.priority);

        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key,
                super::build_hls_origin_source(&input, "12345"),
                &app_state.get_encrypt_secret(),
                super::current_time_millis(),
            )
            .await;
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                super::current_time_millis(),
            )
            .await;
        let session_policy = session.read().await.effective_origin_acquire_policy_or_default();
        assert_eq!(session_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(session_policy.priority, normal_user.priority);
    }

    #[tokio::test]
    async fn hls_entry_origin_reservation_requires_real_provider_handle() {
        let app_state = test_app_state();
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = single_hls_provider_input("missing-provider");

        let reservation = super::try_reserve_hls_entry_origin_account_for_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            &input,
            12345,
            "http://origin.example.com/live/source-user/source-pass/12345.m3u8",
            "hls-session-token",
            "hls-cache:test-session",
            super::hls_origin_account_reservation_ttl_secs_fallback(),
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
        )
        .await;

        assert!(reservation.is_none(), "provisioning redirect must not use an exhausted/counter-only check");
    }

    #[tokio::test]
    async fn hls_entry_origin_reservation_sets_owner_reservation_before_redirect() {
        let input = single_hls_provider_input("available-provider");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let session_owner = "hls-cache:test-session";

        let reservation = super::try_reserve_hls_entry_origin_account_for_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            &input,
            12345,
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            "hls-session-token",
            session_owner,
            super::hls_origin_account_reservation_ttl_secs_fallback(),
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
        )
        .await
        .expect("provider reservation should be acquired before provisioning redirect");

        assert_eq!(reservation.request_url, "http://account.example.com/live/account-user/account-pass/12345.m3u8");
        assert!(reservation.selected_provider_config.is_some());
        assert!(app_state.active_users.active_streams().await.is_empty());
        app_state.connection_manager.release_provider_handle(reservation.provider_handle).await;

        assert!(
            app_state
                .active_provider
                .acquire_connection_with_grace_for_session(
                    &input.name,
                    &test_addr_with_port(55251),
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some("other-owner"),
                )
                .await
                .is_none(),
            "reserved provider must stay blocked for other HLS sessions"
        );

        let same_owner_handle = app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &test_addr_with_port(55252),
                false,
                0,
                ConnectionKind::Normal,
                Some(session_owner),
            )
            .await;
        assert!(same_owner_handle.is_some(), "reserved provider must be reusable by the same HLS session owner");
        app_state.connection_manager.release_provider_handle(same_owner_handle).await;
    }

    #[tokio::test]
    async fn hls_virtual_entry_reservation_uses_input_stream_id_for_shared_session_owner() {
        let input = single_hls_provider_input("origin-id-reservation-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        let target = Arc::new(test_m3u_hls_share_target());
        let item = test_m3u_hls_item(
            &input,
            1001,
            "80510",
            "http://account.example.com/live/account-user/account-pass/80510.m3u8",
        );
        cache_test_m3u_hls_item(&app_state, &target, item).await;
        let stream_identity = super::HlsEntryStreamIdentity::new(1001, "80510").expect("input stream identity");
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();

        assert!(
            super::try_reserve_hls_virtual_entry_origin_account_for_redirect(
                &app_state,
                &test_fingerprint(),
                &user,
                &target,
                &input,
                &stream_identity,
            )
            .await
        );

        let expected_key = HlsSessionKey::new(input.id, "80510");
        let expected_proxy_session_id = build_proxy_session_id(&expected_key, &app_state.get_encrypt_secret());
        let expected_owner = crate::api::model::build_hls_origin_session_owner(&expected_proxy_session_id);
        let same_owner_handle = app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &test_addr_with_port(55253),
                false,
                0,
                ConnectionKind::Normal,
                Some(&expected_owner),
            )
            .await;
        assert!(same_owner_handle.is_some(), "reservation must be owned by input:1|hls|80510, not virtual_id=1001");
        app_state.connection_manager.release_provider_handle(same_owner_handle).await;
    }

    #[tokio::test]
    async fn hls_cache_entry_creates_new_lease_for_same_user_session_and_proxy_session() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        let first_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let first_variant_uri = single_variant_uri(first_response).await;
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
        let first_access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
        let first_session_token =
            access_lease_session_token(&app_state, &proxy_session_id, &first_access_lease_id).await;

        let second_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let second_variant_uri = single_variant_uri(second_response).await;
        assert_ne!(
            access_lease_id_from_variant_uri(&first_variant_uri),
            access_lease_id_from_variant_uri(&second_variant_uri)
        );
        let second_access_lease_id =
            HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());
        let second_session_token =
            access_lease_session_token(&app_state, &proxy_session_id, &second_access_lease_id).await;
        assert_ne!(first_session_token, second_session_token);
    }

    #[tokio::test]
    async fn hls_cache_entry_creates_new_lease_after_manifest_touch() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        let first_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let first_variant_uri = single_variant_uri(first_response).await;
        let first_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
        let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
        let now_ms = super::current_time_millis();

        assert!(matches!(
            app_state
                .hls_proxy
                .touch_manifest_access_lease(
                    &first_lease_id,
                    &proxy_session_id,
                    now_ms,
                    None,
                    Some(super::HlsAccessLeasePendingDeadline::Bootstrap {
                        deadline_ms: now_ms.saturating_add(super::hls_pending_bootstrap_window_ms(&app_state)),
                    }),
                    super::hls_access_lease_ttl_ms(&app_state),
                )
                .await,
            HlsAccessLeaseTouch::Touched { .. }
        ));

        let second_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let second_variant_uri = single_variant_uri(second_response).await;

        assert_ne!(
            access_lease_id_from_variant_uri(&first_variant_uri),
            access_lease_id_from_variant_uri(&second_variant_uri)
        );
        let second_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());
        let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &second_lease_id).await;
        assert_ne!(first_session_token, second_session_token);
    }

    #[tokio::test]
    async fn hls_cache_entry_does_not_reuse_activated_access_lease() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        let first_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let first_variant_uri = single_variant_uri(first_response).await;
        let first_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
        let now_ms = super::current_time_millis();
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &first_lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming {
                    active_window_ms: 5_000,
                    valid_window_ms: super::hls_access_lease_ttl_ms(&app_state),
                },
            )
            .await
            .is_activated());

        let second_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let second_variant_uri = single_variant_uri(second_response).await;
        let second_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());

        assert_ne!(first_lease_id, second_lease_id);
        let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
        let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &second_lease_id).await;
        assert_ne!(first_session_token, second_session_token);
        assert!(
            app_state
                .hls_proxy
                .touch_access_lease(
                    &first_lease_id,
                    super::current_time_millis(),
                    HlsAccessLeaseTiming {
                        active_window_ms: 5_000,
                        valid_window_ms: super::hls_access_lease_ttl_ms(&app_state),
                    },
                )
                .await
        );
    }

    #[tokio::test]
    async fn hls_cache_entry_ignores_existing_pending_lease_for_new_playback() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let proxy_session_id = build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());
        let old_lease_id = HlsAccessLeaseId("old-pending-lease".to_string());
        let old_session_token = "old-hls-session-token";
        let old_issued_at_ms = super::current_time_millis().saturating_sub(6_000);
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                old_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                old_session_token.to_string(),
                1,
                "12345".to_string(),
                12345,
                old_issued_at_ms,
                super::hls_access_lease_ttl_ms(&app_state),
            ))
            .await;

        let response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let variant_uri = single_variant_uri(response).await;
        let new_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&variant_uri).to_string());
        let new_session_token = access_lease_session_token(&app_state, &proxy_session_id, &new_lease_id).await;

        assert_ne!(old_lease_id, new_lease_id);
        assert_ne!(old_session_token, new_session_token);
    }

    #[tokio::test]
    async fn hls_cache_parallel_real_playbacks_same_virtual_id_register_distinct_streams() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;

        let first_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let first_variant_uri = single_variant_uri(first_response).await;
        let first_lease_id = access_lease_id_from_variant_uri(&first_variant_uri);
        assert_eq!(proxy_session_id_from_variant_uri(&first_variant_uri), proxy_session_id);
        let proxy_session = ProxySessionId(proxy_session_id.clone());
        let first_session_token =
            access_lease_session_token(&app_state, &proxy_session, &HlsAccessLeaseId(first_lease_id.to_string())).await;
        publish_ready_test_manifest_for_lease(
            &app_state,
            &proxy_session,
            &HlsAccessLeaseId(first_lease_id.to_string()),
            4_000,
        )
        .await;
        let first_segment_uri = format!("/hls/shared/live/{proxy_session_id}/{first_lease_id}/000123.ts");
        assert_eq!(get_status(Arc::clone(&app_state), &first_segment_uri).await, StatusCode::OK);

        let second_response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;
        let second_variant_uri = single_variant_uri(second_response).await;
        let second_lease_id = access_lease_id_from_variant_uri(&second_variant_uri);
        assert_ne!(first_lease_id, second_lease_id);
        assert_eq!(proxy_session_id_from_variant_uri(&second_variant_uri), proxy_session_id);
        let second_session_token =
            access_lease_session_token(&app_state, &proxy_session, &HlsAccessLeaseId(second_lease_id.to_string()))
                .await;
        assert_ne!(first_session_token, second_session_token);
        publish_ready_test_manifest_for_lease(
            &app_state,
            &proxy_session,
            &HlsAccessLeaseId(second_lease_id.to_string()),
            4_000,
        )
        .await;
        let second_segment_uri = format!("/hls/shared/live/{proxy_session_id}/{second_lease_id}/000123.ts");
        assert_eq!(get_status(Arc::clone(&app_state), &second_segment_uri).await, StatusCode::OK);

        let streams = app_state.active_users.active_streams().await;
        assert_eq!(streams.len(), 2);
        let first_stream = streams
            .iter()
            .find(|stream| stream.session_token.as_deref() == Some(first_session_token.as_str()))
            .expect("first stream should be registered");
        let second_stream = streams
            .iter()
            .find(|stream| stream.session_token.as_deref() == Some(second_session_token.as_str()))
            .expect("second stream should be registered");
        let shared_stream_id = super::hls_cache_shared_stream_id(&proxy_session);
        assert_ne!(first_stream.session_token, second_stream.session_token);
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(second_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
        assert_eq!(second_stream.channel.shared_joined_existing, Some(true));
    }

    #[tokio::test]
    async fn hls_cache_entry_master_playlist_for_xtream_uses_stream_ref_session_identity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput {
            id: 7,
            name: Arc::from("xtream-input"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let origin_source = super::build_hls_origin_source(&input, "80510");
        let expected_proxy_session_id =
            build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());

        let response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            80510,
            None,
            None,
            None,
            "http://origin.example.com/live/user/pass/80510.m3u8",
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let variant_uri = single_variant_uri(response).await;
        assert!(variant_uri.starts_with(&format!("/hls/shared/live/{}/", expected_proxy_session_id.0)));
        assert!(variant_uri.ends_with("/manifest.m3u8"));
        assert!(app_state.hls_proxy.sessions().get_by_key(&HlsSessionKey::new(7, "80510")).await.is_none());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
    }

    #[tokio::test]
    async fn hls_cache_entry_master_playlist_for_m3u_uses_stream_ref_session_identity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input =
            ConfigInput { id: 9, name: Arc::from("m3u-input"), input_type: InputType::M3u, ..ConfigInput::default() };
        let origin_source = super::build_hls_origin_source(&input, "70001");
        let expected_proxy_session_id =
            build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());

        let response = super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            70001,
            None,
            None,
            None,
            "http://media.example.com/channel/playlist.m3u8",
            &input,
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            Some("/iptv"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let variant_uri = single_variant_uri(response).await;
        assert!(variant_uri.starts_with(&format!("/iptv/hls/shared/live/{}/", expected_proxy_session_id.0)));
        assert!(variant_uri.ends_with("/manifest.m3u8"));
        assert!(app_state.hls_proxy.sessions().get_by_key(&HlsSessionKey::new(9, "70001")).await.is_none());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
    }

    #[tokio::test]
    async fn hls_proxy_manifest_invalid_token_starts_no_origin_work() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);

        let response = get_response(
            Arc::clone(&app_state),
            "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/not-a-valid-token/manifest.m3u8",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
        assert!(app_state.hls_proxy.sessions().is_empty().await);
    }

    async fn prepare_server_path_manifest_session(app_state: &Arc<AppState>) -> (HlsSessionHandle, ProxySessionId) {
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.origin_refresh.next_fetch_allowed_at_ms = u64::MAX;
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            let map_id = ProxyMapId(0);
            let mut map = MapEntry::new(
                &session.proxy_session_id,
                map_id,
                OriginMapKey {
                    origin_epoch: 0,
                    resolved_origin_uri: "http://origin.example.com/init.mp4".to_string(),
                    byte_range: None,
                },
                "mp4".to_string(),
            );
            map.status = MapCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms };
            session.maps.insert(map_id, map);
            let mut segment = test_segment_entry(
                &session.proxy_session_id,
                123,
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
            );
            segment.map_ref = Some(map_id);
            session.segments.insert(123, segment);
            session.advance_media_readiness_generation();
            session.last_rendered_manifest = Some(RenderedManifest {
                body: format!(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXT-X-MAP:URI=\"/hls/shared/live/{proxy_session_id}/{}/map/000000.mp4\"\n#EXTINF:4.0,\n/hls/shared/live/{proxy_session_id}/{}/000123.ts\n",
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                ),
                first_proxy_seq: 123,
                last_proxy_seq: 123,
                playlist_duration_ms: 4_000,
                valid_until_ms: rendered_at_ms.saturating_add(4_000),
                render_gap_segments: 0,
                rendered_at_ms,
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![123],
            });
            ProxySessionId(proxy_session_id)
        };
        (session, proxy_session_id)
    }

    #[tokio::test]
    async fn hls_cache_manifest_response_applies_current_users_server_path_without_mutating_session_body() {
        let input_name = Arc::<str>::from("test-input");
        let input = ConfigInput {
            id: 1,
            name: Arc::clone(&input_name),
            input_type: InputType::Xtream,
            url: "http://origin.example.com".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let (session, proxy_session_id) = prepare_server_path_manifest_session(&app_state).await;
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: "http://origin.example.com/live/user/pass/12345.m3u8",
                session_entry_url: super::HlsOriginEntryUrl::direct_http(
                    "http://origin.example.com/live/user/pass/12345.m3u8",
                ),
                input: &input,
                origin_source: super::build_hls_origin_source(&input, "12345"),
            },
            HeaderMap::new(),
            Some("/iptv"),
            "/live/hls-user/hls-pass/12345.m3u8",
            super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("hls cache should handle valid live hls entrypoint");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");

        assert!(body.contains(&format!("/iptv/hls/shared/live/{}/", proxy_session_id.0)));
        assert!(body.contains("/map/000000.mp4"));
        assert!(body.contains("/000123.ts"));
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        let stored_body = session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body.clone();
        assert!(stored_body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert!(stored_body.contains(&format!("/hls/shared/live/{}/", proxy_session_id.0)));
        assert!(!stored_body.contains("/iptv/hls/shared/live/"));
        assert!(!stored_body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    fn transient_manifest_body(proxy_session_id: &str) -> String {
        transient_manifest_body_from_sequence(proxy_session_id, 100, 6)
    }

    fn transient_manifest_body_from_sequence(proxy_session_id: &str, first_sequence: u64, count: usize) -> String {
        let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n");
        for index in 0..count {
            let sequence = first_sequence.saturating_add(u64::try_from(index).expect("test sequence index fits u64"));
            body.push_str("#EXTINF:10.0,\n");
            let _ = writeln!(
                body,
                "/hls/shared/live/{proxy_session_id}/{}/r/seg{sequence}.ts",
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
            );
        }
        body
    }

    fn media_uri_count(body: &str) -> usize {
        body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count()
    }

    fn normal_manifest_body(proxy_session_id: &str) -> String {
        normal_manifest_body_from_sequence(proxy_session_id, 0, 6)
    }

    fn normal_manifest_body_from_sequence(proxy_session_id: &str, first_sequence: u64, count: usize) -> String {
        let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n");
        for index in 0..count {
            let sequence = first_sequence.saturating_add(u64::try_from(index).expect("test sequence index fits u64"));
            body.push_str("#EXTINF:10.0,\n");
            let _ = writeln!(
                body,
                "/hls/shared/live/{proxy_session_id}/{}/{sequence:06}.ts",
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
            );
        }
        body
    }

    fn store_normal_manifest_body(session: &mut HlsSession, body: String, rendered_at_ms: u64) {
        store_normal_manifest_body_range(session, body, 0, 6, rendered_at_ms);
    }

    fn store_normal_manifest_body_range(
        session: &mut HlsSession,
        body: String,
        first_proxy_seq: u64,
        count: usize,
        rendered_at_ms: u64,
    ) {
        let last_proxy_seq =
            first_proxy_seq.saturating_add(u64::try_from(count.saturating_sub(1)).expect("test count fits u64"));
        let proxy_session_id = session.proxy_session_id.clone();
        for proxy_seq in first_proxy_seq..=last_proxy_seq {
            let mut entry = test_segment_entry(
                &proxy_session_id,
                proxy_seq,
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
            );
            entry.duration_ms = 10_000;
            session.segments.insert(proxy_seq, entry);
        }
        session.advance_media_readiness_generation();
        session.last_rendered_manifest = Some(RenderedManifest {
            body,
            first_proxy_seq,
            last_proxy_seq,
            playlist_duration_ms: 60_000,
            valid_until_ms: rendered_at_ms.saturating_add(60_000),
            render_gap_segments: 0,
            rendered_at_ms,
            discontinuity_sequence: 0,
            target_duration_ms: 10_000,
            segment_proxy_seqs: (first_proxy_seq..=last_proxy_seq).collect(),
        });
    }

    async fn try_test_hls_cached_manifest_response(
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        access_lease_id: &HlsAccessLeaseId,
        access_lease_state: HlsAccessLeaseState,
        strip: &StripConfig,
        server_path: Option<&str>,
        options: super::HlsCachedManifestOptions,
    ) -> Option<axum::response::Response> {
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let now_ms = super::current_time_millis();
        if app_state
            .hls_proxy
            .access_lease_response_snapshot(access_lease_id, &proxy_session_id, now_ms)
            .await
            .is_none()
        {
            app_state
                .hls_proxy
                .prepare_access_lease(HlsAccessLease::pending(
                    access_lease_id.clone(),
                    HlsPlaybackFamilyKey::new("test-user", "manifest-test-client"),
                    proxy_session_id,
                    "test-user".to_string(),
                    "manifest-test-session".to_string(),
                    1,
                    "12345".to_string(),
                    12345,
                    now_ms,
                    60_000,
                ))
                .await;
        }
        super::try_hls_cached_manifest_response(
            app_state,
            session,
            access_lease_id,
            access_lease_state,
            strip,
            server_path,
            options,
            super::HlsRuntimeBandwidthLearningContext::Disabled,
        )
        .await
    }

    #[test]
    fn repeated_speculative_strip_candidates_yield_one_committed_applied_diagnostic() {
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let body = normal_manifest_body("proxy-session");
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        let candidates = (0..8)
            .map(|_| {
                super::materialize_shared_hls_access_manifest(
                    &body,
                    &access_lease_id,
                    HlsAccessLeaseState::Pending,
                    &strip,
                    "normal",
                    None,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(candidates.len(), 8);
        let candidate_count = candidates.len();
        let diagnostics = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                super::hls_initial_strip_publication_diagnostic(
                    if index.saturating_add(1) == candidate_count {
                        super::HlsInitialStripPublicationStatus::Committed
                    } else {
                        super::HlsInitialStripPublicationStatus::NotCommitted
                    },
                    HlsAccessLeaseState::Pending,
                    candidate,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            diagnostics,
            vec![super::HlsInitialStripPublicationDiagnostic::Applied {
                mode: "normal",
                strip_mode: "segments",
                configured: 3,
                effective: 3,
                visible_segments: 3,
            }]
        );
    }

    #[test]
    fn committed_pending_strip_disabled_yields_one_skipped_diagnostic() {
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let materialized = super::materialize_shared_hls_access_manifest(
            &normal_manifest_body("proxy-session"),
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 0 },
            "normal",
            None,
        );

        let diagnostics = [super::hls_initial_strip_publication_diagnostic(
            super::HlsInitialStripPublicationStatus::Committed,
            HlsAccessLeaseState::Pending,
            &materialized,
        )
        .expect("committed strip diagnostic")];

        assert_eq!(
            diagnostics,
            [super::HlsInitialStripPublicationDiagnostic::Skipped {
                mode: "normal",
                reason: crate::api::model::hls_cache::initial_strip::HlsInitialStripSkipReason::StripDisabled,
                visible_segments: 6,
            }]
        );
        assert_eq!(media_uri_count(&materialized.body), 6);
    }

    #[test]
    fn committed_activated_manifest_yields_one_lease_state_skip_diagnostic() {
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let materialized = super::materialize_shared_hls_access_manifest(
            &normal_manifest_body("proxy-session"),
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            "normal",
            None,
        );

        let diagnostics = [super::hls_initial_strip_publication_diagnostic(
            super::HlsInitialStripPublicationStatus::Committed,
            HlsAccessLeaseState::Activated,
            &materialized,
        )
        .expect("committed strip diagnostic")];

        assert_eq!(
            diagnostics,
            [super::HlsInitialStripPublicationDiagnostic::SkippedForLeaseState {
                mode: "normal",
                reason: super::HlsInitialStripLeaseSkipReason::LeaseActivated,
            }]
        );
        assert_eq!(media_uri_count(&materialized.body), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_strip_admission_timeout_does_not_commit_speculative_candidate() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.clone();
            let rendered_at_ms = super::current_time_millis();
            store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id.0), rendered_at_ms);
            session.segments.get_mut(&1).expect("visible test segment").status = SegmentCacheStatus::Discovered;
            session.advance_media_readiness_generation();
            session.mark_authorized_media_access(rendered_at_ms);
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::from_millis(75)),
        )
        .await
        .expect("timeout response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::current_time_millis())
            .await
            .expect("pending lease remains available");
        assert!(lease.last_manifest_snapshot.is_none());
    }

    async fn publish_ready_test_manifest_for_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        access_lease_id: &HlsAccessLeaseId,
        target_duration_ms: u64,
    ) {
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(proxy_session_id)
            .await
            .expect("test session exists");
        let (proxy_seq, duration_ms) = {
            let session = session.read().await;
            session
                .segments
                .iter()
                .find_map(|(proxy_seq, entry)| {
                    matches!(entry.status, SegmentCacheStatus::Ready { .. }).then_some((*proxy_seq, entry.duration_ms))
                })
                .expect("READY test segment")
        };
        let now_ms = super::current_time_millis();
        let publication_guard = app_state
            .hls_proxy
            .prepare_access_lease_manifest_publication(access_lease_id, proxy_session_id, now_ms)
            .await
            .expect("test lease accepts publication");
        let snapshot = HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(now_ms),
            snapshot_generation: 0,
            delivered_at_ms: now_ms,
            first_proxy_seq: proxy_seq,
            last_proxy_seq: proxy_seq,
            visible_segments: Arc::from([HlsLeaseManifestSegment {
                proxy_seq,
                duration_ms,
                uri: format!("/hls/shared/live/{}/{}/{proxy_seq:06}.ts", proxy_session_id.0, access_lease_id.0),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            }]),
            discontinuity_sequence: 0,
            target_duration_ms: target_duration_ms.max(duration_ms),
            playlist_duration_ms: duration_ms,
            last_visible_media_end_ms: duration_ms,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        };
        assert!(app_state
            .hls_proxy
            .commit_access_lease_manifest_publication(
                access_lease_id,
                proxy_session_id,
                publication_guard,
                snapshot,
                now_ms,
            )
            .await
            .is_committed());
    }

    async fn prepare_user_exhausted_terminal_bundle(app_state: &Arc<AppState>, target_duration_ms: u64) {
        let response = app_state.app_config.custom_stream_response.load_full().expect("runtime custom responses");
        let asset = response
            .user_connections_exhausted
            .as_ref()
            .and_then(|buffer| snapshot_terminal_media_asset(buffer).ok())
            .expect("valid user-exhausted terminal asset");
        let key = prepared_terminal_bundle_key(&asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        let state = app_state.hls_proxy.start_prepared_terminal_bundle(
            asset,
            target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = match state {
            HlsPreparedTerminalBundleState::Preparing { .. } => app_state
                .hls_proxy
                .wait_for_prepared_terminal_bundle(key)
                .await
                .expect("user-exhausted terminal bundle completion"),
            state => state,
        };
        assert!(matches!(
            state,
            HlsPreparedTerminalBundleState::Ready { ref bundle } if bundle.key == key
        ));
    }

    struct RuntimePolicyEndpointFixture {
        _temp_dir: tempfile::TempDir,
        app_state: Arc<AppState>,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        manifest_uri: String,
        live_segment_uri: String,
    }

    async fn assert_runtime_policy_base_timing(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
        phase: &str,
    ) {
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(proxy_session_id)
            .await
            .expect("runtime policy session");
        let manifest = app_state
            .hls_proxy
            .access_lease_response_snapshot(lease_id, proxy_session_id, super::current_time_millis())
            .await
            .and_then(|lease| lease.last_manifest_snapshot)
            .expect("runtime policy manifest");
        let evidence = prepare_terminal_base_evidence(
            &session,
            app_state.hls_proxy.segment_cache(),
            &manifest,
            super::current_time_millis(),
        )
        .await;
        assert!(
            evidence.timing().is_some(),
            "runtime policy base timing {phase}: {}",
            evidence.track_evidence_reason_code()
        );
        evidence.release();
    }

    async fn serve_and_wait_runtime_policy_base_segment(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
        live_segment_uri: &str,
    ) {
        let initial_segment = get_response(Arc::clone(app_state), live_segment_uri, None).await;
        assert_eq!(initial_segment.status(), StatusCode::OK);
        assert!(!response_body(initial_segment).await.is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let completed = app_state
                    .hls_proxy
                    .access_lease_response_snapshot(lease_id, proxy_session_id, super::current_time_millis())
                    .await
                    .and_then(|lease| lease.playback_cursor.highest_contiguous_completed_proxy_seq);
                if completed == Some(123) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial live segment completion");
    }

    async fn runtime_policy_endpoint_fixture(publish_manifest: bool) -> RuntimePolicyEndpointFixture {
        const TARGET_DURATION_MS: u64 = 12_000;

        let temp_dir = tempfile::tempdir().expect("runtime policy cache tempdir");
        let app_state =
            test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
        enable_runtime_policy_custom_responses(&app_state);
        let live_bytes =
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
        let proxy_session_id = ProxySessionId(map_ready_segment_without_lease(&app_state, 123, "ts", live_bytes).await);
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("runtime policy session");
        session.write().await.segments.get_mut(&123).expect("runtime policy base segment").duration_ms =
            TARGET_DURATION_MS;
        let lease_id = HlsAccessLeaseId(grant_hls_proxy_lease(&app_state, &proxy_session_id.0).await);
        if publish_manifest {
            publish_ready_test_manifest_for_lease(&app_state, &proxy_session_id, &lease_id, TARGET_DURATION_MS).await;
            let target_duration_ms = app_state
                .hls_proxy
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, super::current_time_millis())
                .await
                .and_then(|lease| lease.last_manifest_snapshot)
                .map(|manifest| manifest.target_duration_ms)
                .expect("published runtime policy target duration");
            prepare_user_exhausted_terminal_bundle(&app_state, target_duration_ms).await;
        }
        let manifest_uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, lease_id.0);
        let live_segment_uri = format!("/hls/shared/live/{}/{}/000123.ts", proxy_session_id.0, lease_id.0);
        if publish_manifest {
            assert_runtime_policy_base_timing(&app_state, &proxy_session_id, &lease_id, "before serve").await;
        }
        serve_and_wait_runtime_policy_base_segment(&app_state, &proxy_session_id, &lease_id, &live_segment_uri).await;
        if publish_manifest {
            assert_runtime_policy_base_timing(&app_state, &proxy_session_id, &lease_id, "after serve").await;
        }

        RuntimePolicyEndpointFixture {
            _temp_dir: temp_dir,
            app_state,
            proxy_session_id,
            lease_id,
            manifest_uri,
            live_segment_uri,
        }
    }

    async fn wait_for_runtime_policy_terminal_plan(fixture: &RuntimePolicyEndpointFixture) -> Arc<HlsTerminalTailPlan> {
        let plan = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(HlsLeasePlaybackMode::TerminalTail(plan)) = fixture
                    .app_state
                    .hls_proxy
                    .access_lease_response_snapshot(
                        &fixture.lease_id,
                        &fixture.proxy_session_id,
                        super::current_time_millis(),
                    )
                    .await
                    .map(|lease| lease.playback_mode)
                {
                    return plan;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if let Ok(plan) = plan {
            return plan;
        }
        let lease = fixture
            .app_state
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, super::current_time_millis())
            .await;
        let state = lease.as_ref().map_or("missing", |lease| lease.state.as_log_value());
        let playback = lease.as_ref().map_or("missing", |lease| match lease.playback_mode {
            HlsLeasePlaybackMode::Live => "live",
            HlsLeasePlaybackMode::TerminalTail(_) => "terminal-tail",
            HlsLeasePlaybackMode::TerminalUnavailable { .. } => "terminal-unavailable",
            HlsLeasePlaybackMode::Ended => "ended",
        });
        panic!(
            "runtime policy terminal owner deadline: state={state} playback={playback} owners={}",
            fixture.app_state.hls_proxy.terminal_pending().owner_count()
        );
    }

    #[tokio::test]
    async fn hls_cache_pending_normal_manifest_applies_initial_strip_without_mutating_shared_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
            session.mark_authorized_media_access(rendered_at_ms);
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        let stored_before = session.read().await.last_rendered_manifest.as_ref().expect("normal manifest").body.clone();

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("normal manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(body.contains("/000000.ts"));
        assert!(body.contains("/000001.ts"));
        assert!(body.contains("/000002.ts"));
        assert!(!body.contains("/000003.ts"));
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert_eq!(session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body, stored_before);
        assert_eq!(media_uri_count(&stored_before), 6);
    }

    #[tokio::test]
    async fn hls_cache_idle_normal_manifest_applies_initial_strip_without_mutating_shared_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
            session.mark_authorized_media_access(rendered_at_ms);
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        let stored_before = session.read().await.last_rendered_manifest.as_ref().expect("normal manifest").body.clone();

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Idle,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("idle normal manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(body.contains("/000000.ts"));
        assert!(body.contains("/000001.ts"));
        assert!(body.contains("/000002.ts"));
        assert!(!body.contains("/000003.ts"));
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert_eq!(session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body, stored_before);
        assert_eq!(media_uri_count(&stored_before), 6);
    }

    #[tokio::test]
    async fn hls_cache_activated_normal_manifest_skips_initial_strip() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
            session.mark_authorized_media_access(rendered_at_ms);
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("normal manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 6);
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    }

    #[tokio::test]
    async fn hls_cache_fresh_required_normal_manifest_does_not_serve_stale_committed_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let rendered_at_ms = {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
            session.mark_authorized_media_access(rendered_at_ms);
            rendered_at_ms
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::ZERO).requiring_newer_manifest(rendered_at_ms),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn hls_cache_fresh_required_normal_manifest_waits_for_newer_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let (proxy_session_id, old_rendered_at_ms) = {
            let mut session = session.write().await;
            let proxy_session_id = session.proxy_session_id.clone();
            let old_rendered_at_ms = super::current_time_millis();
            store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id.0), old_rendered_at_ms);
            session.origin_refresh.in_flight = true;
            session.mark_authorized_media_access(old_rendered_at_ms);
            (proxy_session_id, old_rendered_at_ms)
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            let rendered_at_ms = super::current_time_millis();
            let mut entry = test_segment_entry(
                &session.proxy_session_id,
                100,
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
            );
            entry.duration_ms = 4_000;
            session.segments.insert(100, entry);
            session.advance_media_readiness_generation();
            session.last_rendered_manifest = Some(RenderedManifest {
                body: format!(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n/hls/shared/live/{proxy_session_for_body}/{}/000100.ts\n",
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                ),
                first_proxy_seq: 100,
                last_proxy_seq: 100,
                playlist_duration_ms: 4_000,
                valid_until_ms: rendered_at_ms.saturating_add(4_000),
                render_gap_segments: 0,
                rendered_at_ms,
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![100],
            });
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200))
                .requiring_newer_manifest(old_rendered_at_ms),
        )
        .await
        .expect("fresh manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert!(body.contains("000100.ts"));
        assert!(!body.contains("000000.ts"));
        assert!(body.contains(&access_lease_id.0));
    }

    #[tokio::test]
    async fn hls_cache_pending_transient_manifest_applies_initial_strip_without_mutating_shared_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis());
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        let stored_before = session.read().await.transient.last_manifest_body.clone().expect("transient manifest body");

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
        assert_eq!(
            session.read().await.transient.last_manifest_body.as_ref().expect("stored manifest"),
            &stored_before
        );
        assert_eq!(media_uri_count(&stored_before), 6);
    }

    fn assert_encrypted_transient_terminal_incompatibility(snapshot: HlsLeaseManifestSnapshot, now_ms: u64) {
        let asset = terminal_test_asset();
        let bundle_target_duration_ms = snapshot.target_duration_ms.max(asset.duration_ms());
        let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &snapshot));
        let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
        let terminal_splice_evidence = base_splice_evidence.clone();
        assert_eq!(
            build_terminal_tail_plan(HlsTerminalTailBuildInput {
                generation: HlsTerminalTailGeneration(1),
                created_at_ms: now_ms,
                base_availability: Arc::from([]),
                base_track_signature: Some(asset.track_signature().clone()),
                base_splice_evidence,
                terminal_splice_evidence,
                base_timing,
                base_key_bindings: Arc::from([]),
                expected_asset: HlsRuntimeCustomTailAssetIdentity::channel_unavailable(
                    HlsTerminalAssetIdentity::from_asset(&asset),
                ),
                anchored_bundle: HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, bundle_target_duration_ms,),
                base_manifest: snapshot,
                asset,
            }),
            Err(HlsTerminalTailCompatibility::TransientPassthroughUnsupported)
        );
    }

    #[tokio::test]
    async fn encrypted_transient_endpoint_stores_client_visible_key_and_typed_terminal_incompatibility() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let now_ms = super::current_time_millis();
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), now_ms)
            .await;
        let (proxy_session_id, access_lease_id) = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.clone();
            let access_lease_id = HlsAccessLeaseId("encrypted-access-lease".to_string());
            let body = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n\
                 #EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/{}/{}/r/key.bin\",IV=0x1,KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n\
                 #EXTINF:4.0,\n/hls/shared/live/{}/{}/r/20.ts\n\
                 #EXTINF:4.0,\n/hls/shared/live/{}/{}/r/21.ts\n\
                 #EXTINF:4.0,\n/hls/shared/live/{}/{}/r/22.ts\n",
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
            );
            session.transient.replace_manifest_with_validity(body, now_ms, 12_000);
            session.mark_authorized_media_access(now_ms);
            (proxy_session_id, access_lease_id)
        };
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                access_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "encrypted-client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "encrypted-session".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                60_000,
            ))
            .await;

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 0 },
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("encrypted transient response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms)
            .await
            .expect("lease snapshot");
        let snapshot = lease.last_manifest_snapshot.expect("manifest snapshot");
        let encryption = snapshot.active_encryption.as_ref().expect("active encryption");

        assert!(
            body.contains(&format!("URI=\"/hls/shared/live/{}/{}/r/key.bin\"", proxy_session_id.0, access_lease_id.0))
        );
        assert_eq!(snapshot.delivery_mode, HlsManifestDeliveryMode::TransientPassthrough);
        assert_eq!(encryption.method, "AES-128");
        assert_eq!(encryption.iv.as_deref(), Some("0x1"));
        assert_eq!(encryption.key_format.as_deref(), Some("identity"));
        assert_eq!(encryption.key_format_versions.as_deref(), Some("1"));
        assert!(encryption.can_reset_to_clear);

        assert_encrypted_transient_terminal_incompatibility(snapshot, now_ms);
    }

    #[tokio::test]
    async fn hls_cache_idle_transient_manifest_applies_initial_strip_without_mutating_shared_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis());
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        let stored_before = session.read().await.transient.last_manifest_body.clone().expect("transient manifest body");

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Idle,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("idle transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert_eq!(
            session.read().await.transient.last_manifest_body.as_ref().expect("stored manifest"),
            &stored_before
        );
        assert_eq!(media_uri_count(&stored_before), 6);
    }

    #[tokio::test]
    async fn hls_cache_activated_transient_manifest_skips_initial_strip() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis().saturating_sub(16_000));
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 6);
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    }

    #[tokio::test]
    async fn hls_cache_transient_manifest_without_media_activity_is_not_served_from_committed_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            session.transient.replace_manifest_with_validity(
                transient_manifest_body(&proxy_session_id),
                rendered_at_ms,
                60_000,
            );
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn hls_cache_no_media_yet_transient_manifest_is_served_for_initial_canonical_response() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            let rendered_at_ms = super::current_time_millis();
            session.transient.replace_manifest_with_validity(
                transient_manifest_body(&proxy_session_id),
                rendered_at_ms,
                60_000,
            );
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await
        .expect("initial transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_transient_manifest_outside_soft_window_is_not_served_from_committed_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis().saturating_sub(60_000));
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn hls_cache_expired_transient_manifest_with_active_binding_is_served_while_manifest_valid() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let now_ms = super::current_time_millis();
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.clone();
            session.transient.replace_manifest_with_validity(
                transient_manifest_body(&proxy_session_id.0),
                now_ms.saturating_sub(1_000),
                60_000,
            );
            session.mark_authorized_media_access(now_ms.saturating_sub(60_000));
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::from("test-input"),
                Arc::from("test-account"),
                &proxy_session_id,
                now_ms,
            ));
            session.origin_refresh.in_flight = true;
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("valid committed transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
    }

    #[tokio::test]
    async fn hls_cache_expired_transient_manifest_with_active_binding_is_not_served_after_manifest_validity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let now_ms = super::current_time_millis();
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.clone();
            session.transient.replace_manifest_with_validity(
                transient_manifest_body(&proxy_session_id.0),
                now_ms.saturating_sub(60_000),
                1_000,
            );
            session.mark_authorized_media_access(now_ms.saturating_sub(60_000));
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::from("test-input"),
                Arc::from("test-account"),
                &proxy_session_id,
                now_ms,
            ));
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn hls_cache_no_media_yet_waits_for_first_normal_manifest_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.origin_refresh.in_flight = true;
            session.proxy_session_id.clone()
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            let rendered_at_ms = super::current_time_millis();
            let mut entry = test_segment_entry(
                &session.proxy_session_id,
                100,
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
            );
            entry.duration_ms = 4_000;
            session.segments.insert(100, entry);
            session.advance_media_readiness_generation();
            session.last_rendered_manifest = Some(RenderedManifest {
                    body: format!(
                        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n/hls/shared/live/{proxy_session_for_body}/{}/000100.ts\n",
                        crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                    ),
                    first_proxy_seq: 100,
                    last_proxy_seq: 100,
                    playlist_duration_ms: 4_000,
                    valid_until_ms: rendered_at_ms.saturating_add(4_000),
                    render_gap_segments: 0,
                    rendered_at_ms,
                    discontinuity_sequence: 0,
                    target_duration_ms: 4_000,
                    segment_proxy_seqs: vec![100],
                });
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("initial normal manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_no_media_yet_waits_for_first_transient_manifest_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            session.origin_refresh.in_flight = true;
            session.proxy_session_id.clone()
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            let rendered_at_ms = super::current_time_millis();
            session.transient.replace_manifest_with_validity(
                transient_manifest_body(&proxy_session_for_body),
                rendered_at_ms,
                60_000,
            );
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("initial transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_expired_transient_manifest_waits_for_revalidation_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            session.mark_authorized_media_access(super::current_time_millis().saturating_sub(60_000));
            session.origin_refresh.in_flight = true;
            session.proxy_session_id.clone()
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            let rendered_at_ms = super::current_time_millis();
            session.transient.replace_manifest_with_validity(
                transient_manifest_body(&proxy_session_for_body),
                rendered_at_ms,
                60_000,
            );
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

        let response = try_test_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("revalidated transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    async fn grant_hls_proxy_lease(app_state: &Arc<AppState>, proxy_session_id: &str) -> String {
        create_active_hls_user_session(app_state).await;
        let now_ms = super::current_time_millis();
        let lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
        let family_key = HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key);
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                family_key,
                ProxySessionId(proxy_session_id.to_string()),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                super::hls_access_lease_ttl_ms(app_state),
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &ProxySessionId(proxy_session_id.to_string()),
                now_ms,
                HlsAccessLeaseTiming {
                    active_window_ms: 5_000,
                    valid_window_ms: super::hls_access_lease_ttl_ms(app_state),
                },
            )
            .await
            .is_activated());
        lease_id.0
    }

    async fn hls_proxy_uri(app_state: &Arc<AppState>, proxy_session_id: &str, suffix: &str) -> String {
        let access_lease_id = grant_hls_proxy_lease(app_state, proxy_session_id).await;
        format!("/hls/shared/live/{proxy_session_id}/{access_lease_id}/{suffix}")
    }

    fn regression_origin_manifest(first_sequence: u64, segment_count: usize) -> Vec<u8> {
        let mut manifest =
            format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n");
        for offset in 0..segment_count {
            let sequence = first_sequence.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
            let _ = writeln!(&mut manifest, "#EXTINF:12.0,\n{sequence}.ts");
        }
        manifest.into_bytes()
    }

    fn regression_origin_refresh_request(
        app_state: &Arc<AppState>,
        session: HlsSessionHandle,
        manifest_url: &str,
        access_lease_id: Option<HlsAccessLeaseId>,
    ) -> OriginRefreshRequest {
        OriginRefreshRequest {
            app_config: Arc::clone(&app_state.app_config),
            session,
            origin_entry: LiveHlsOriginEntry::parse(manifest_url).expect("regression origin entry"),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: app_state.http_client.load().as_ref().clone(),
            no_redirect_client: app_state.http_client_no_redirect.load().as_ref().clone(),
            use_manual_redirects: false,
            segment_cache: Arc::clone(app_state.hls_proxy.segment_cache()),
            hls_proxy: Arc::clone(&app_state.hls_proxy),
            segment_repair: Arc::clone(app_state.hls_proxy.segment_repair()),
            segment_worker_pool: Arc::clone(app_state.hls_proxy.segment_worker_pool()),
            map_worker_pool: Arc::clone(app_state.hls_proxy.map_worker_pool()),
            origin_manifest_timeout_ms: app_state.hls_proxy.origin_manifest_timeout_ms(),
            manifest_recovery_burst: app_state.hls_proxy.manifest_recovery_burst(),
            strip: app_state.hls_proxy.strip(),
            retry_policy: RetryPolicy { delays_ms: [0; 5], jitter_max_ms: 0 },
            reverse_proxy_rewrite_secret: app_state.get_encrypt_secret().to_vec(),
            transient_resource_ttl_ms: app_state.hls_proxy.transient_resource_ttl_ms(),
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            fresh_manifest_requirement_generation: None,
            acceptance_directive: HlsManifestAcceptanceDirective::none(),
            access_lease_id,
            disabled_headers: None,
            now_ms: super::current_time_millis(),
            origin_io: None,
            post_refresh_runtime: None,
        }
    }

    async fn wait_for_ready_timeline(session: &HlsSessionHandle, expected_ready: usize) {
        let wait = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let ready = session
                    .read()
                    .await
                    .segments
                    .values()
                    .filter(|segment| matches!(segment.status, SegmentCacheStatus::Ready { .. }))
                    .count();
                if ready >= expected_ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if wait.is_err() {
            let session = session.read().await;
            let statuses = session
                .segments
                .values()
                .map(|segment| (segment.proxy_seq, segment.status.clone()))
                .collect::<Vec<_>>();
            panic!("READY timeline deadline: expected={expected_ready} statuses={statuses:?}");
        }
    }

    async fn extend_ready_segment_as_sparse_file(
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        proxy_seq: u64,
        logical_size: u64,
    ) {
        let cache_key = session.read().await.segments.get(&proxy_seq).expect("mapped sparse segment").cache_key.clone();
        let metadata = app_state
            .hls_proxy
            .segment_cache()
            .metadata(&cache_key)
            .await
            .expect("sparse cache metadata read")
            .expect("READY sparse cache object");
        let file =
            tokio::fs::OpenOptions::new().write(true).open(&metadata.path).await.expect("sparse cache object opens");
        file.set_len(logical_size).await.expect("sparse cache object extends");
        let mut session = session.write().await;
        let segment = session.segments.get_mut(&proxy_seq).expect("sparse segment remains mapped");
        segment.status =
            SegmentCacheStatus::Ready { content_length: logical_size, ready_at_ms: super::current_time_millis() };
        session.advance_media_readiness_generation();
        session.render_and_store_manifest(super::current_time_millis()).expect("sparse timeline renders");
    }

    fn terminal_test_asset() -> Arc<HlsTerminalMediaAsset> {
        let bytes = bytes::Bytes::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/fixtures/hls/channel_unavailable.ts"
        )));
        let buffer = TransportStreamBuffer::new(bytes.to_vec());
        snapshot_terminal_media_asset(&buffer).expect("terminal test asset is valid")
    }

    async fn publish_test_manifest_and_exhaust_configured_acceptance(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
        snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) {
        let target_duration_ms = snapshot.target_duration_ms;
        let publication_guard = app_state
            .hls_proxy
            .prepare_access_lease_manifest_publication(lease_id, proxy_session_id, now_ms)
            .await
            .expect("live lease accepts publication preparation");
        assert!(app_state
            .hls_proxy
            .commit_access_lease_manifest_publication(lease_id, proxy_session_id, publication_guard, snapshot, now_ms,)
            .await
            .is_committed());

        let terminal_response = app_state.app_config.custom_stream_response.load_full();
        let terminal_asset = terminal_response
            .as_ref()
            .and_then(|responses| responses.channel_unavailable.as_ref())
            .and_then(|buffer| snapshot_terminal_media_asset(buffer).ok())
            .expect("configured terminal test asset");
        let terminal_key =
            prepared_terminal_bundle_key(&terminal_asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        let state = app_state.hls_proxy.start_prepared_terminal_bundle(
            terminal_asset,
            target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = match state {
            HlsPreparedTerminalBundleState::Preparing { .. } => app_state
                .hls_proxy
                .wait_for_prepared_terminal_bundle(terminal_key)
                .await
                .expect("terminal bundle completion"),
            state => state,
        };
        assert!(matches!(
            state,
            HlsPreparedTerminalBundleState::Ready { ref bundle } if bundle.key == terminal_key
        ));

        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(proxy_session_id)
            .await
            .expect("warm shared session");
        let mut session = session.write().await;
        session.origin_control.record_media_progress(now_ms, target_duration_ms);
        let burst_plan = app_state.hls_proxy.manifest_recovery_burst().level.plan();
        let operation_timeout = HlsOperationTimeoutMs::from_millis(app_state.hls_proxy.origin_manifest_timeout_ms());
        let expected_eta = HlsRecoveryEtaMs::from_millis(app_state.hls_proxy.origin_manifest_timeout_ms());
        let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms: now_ms,
            burst_plan,
            target_duration_ms,
            transition_margin: HlsTransitionMarginMs::from_millis(target_duration_ms),
            workload: HlsRecoveryWorkload::clear_fetch(),
            observed_latency: HlsObservedRecoveryLatency::default(),
            required_terminal_media_key: Some(terminal_key),
            terminal_media_preparation: HlsTerminalMediaPreparationState::Ready { key: terminal_key },
            policy: HlsRecoveryTimingPolicy::new(operation_timeout, operation_timeout, expected_eta, expected_eta),
        });
        session.origin_control.begin_acceptance_episode(
            now_ms,
            burst_plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &timing,
        );
        session.origin_control.path_condition = HlsOriginPathCondition::HardFetchFailure;
        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        episode.record_full_burst();
        episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
        episode.hold_after_uncommitted_burst(None, None);
    }

    async fn terminalize_existing_test_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &str,
        lease_id: &str,
        base_proxy_seq: u64,
    ) -> TransportStreamBuffer {
        let proxy_session_id = ProxySessionId(proxy_session_id.to_string());
        let lease_id = HlsAccessLeaseId(lease_id.to_string());
        let buffer = TransportStreamBuffer::new(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .to_vec(),
        );
        let asset = snapshot_terminal_media_asset(&buffer).expect("terminal test asset is valid");
        let base_manifest = HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(7),
            snapshot_generation: 7,
            delivered_at_ms: super::current_time_millis(),
            first_proxy_seq: base_proxy_seq,
            last_proxy_seq: base_proxy_seq,
            visible_segments: Arc::from([HlsLeaseManifestSegment {
                proxy_seq: base_proxy_seq,
                duration_ms: 4_000,
                uri: format!("/iptv/hls/shared/live/{}/{}/{base_proxy_seq:06}.ts", proxy_session_id.0, lease_id.0),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            }]),
            discontinuity_sequence: 3,
            target_duration_ms: asset.duration_ms().saturating_add(1_000),
            playlist_duration_ms: 4_000,
            last_visible_media_end_ms: 4_000,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        };
        let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &base_manifest));
        let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
        let terminal_splice_evidence = base_splice_evidence.clone();
        let plan = build_terminal_tail_plan(HlsTerminalTailBuildInput {
            generation: HlsTerminalTailGeneration(17),
            created_at_ms: super::current_time_millis(),
            base_availability: Arc::from([HlsTerminalBaseSegmentAvailability {
                proxy_seq: base_proxy_seq,
                media_state: HlsTerminalBaseMediaState::Ready,
                required_map_ready: true,
                required_key_ready: true,
                protection: HlsTerminalBaseProtection::Protectable,
            }]),
            base_track_signature: Some(asset.track_signature().clone()),
            base_splice_evidence,
            terminal_splice_evidence,
            base_timing,
            base_key_bindings: Arc::from([]),
            expected_asset: HlsRuntimeCustomTailAssetIdentity::channel_unavailable(
                HlsTerminalAssetIdentity::from_asset(&asset),
            ),
            base_manifest: base_manifest.clone(),
            anchored_bundle: HlsTerminalTailBuildInput::anchored_bundle_for_test(
                &asset,
                base_manifest.target_duration_ms,
            ),
            asset,
        })
        .expect("terminal test plan is compatible");
        let protection = HlsTerminalTailProtection {
            generation: plan.generation,
            base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
            key_bindings: plan.key_bindings(),
        };
        {
            let mut leases = app_state.hls_proxy.access_leases().write().await;
            let mut lease = leases.remove_access_lease(&lease_id).expect("test lease exists before terminal cutover");
            lease.last_manifest_snapshot = Some(base_manifest);
            lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::new(plan));
            leases.prepare_access_lease(lease);
        }
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("test session exists before terminal cutover");
        session.write().await.install_terminal_tail_protection(lease_id, protection);
        buffer
    }

    async fn terminal_test_plan_shape(app_state: &Arc<AppState>, proxy_session_id: &str, lease_id: &str) -> (u64, u16) {
        let snapshot = app_state
            .hls_proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId(lease_id.to_string()),
                &ProxySessionId(proxy_session_id.to_string()),
                super::current_time_millis(),
            )
            .await
            .expect("terminal test lease snapshot exists");
        let HlsLeasePlaybackMode::TerminalTail(plan) = snapshot.playback_mode else {
            panic!("terminal test lease keeps terminal playback mode");
        };
        (plan.generation.0, plan.segment_count)
    }

    async fn wait_for_provider_connection_count(app_state: &Arc<AppState>, expected: usize) {
        for _ in 0..50 {
            let actual = app_state.active_provider.get_provider_connections_count().await;
            if actual == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(app_state.active_provider.get_provider_connections_count().await, expected);
    }

    fn normal_manifest(body: &str) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, "http://origin.example.com/live/final/index.m3u8") {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    async fn map_segment(app_state: &Arc<AppState>, proxy_seq: u64, extension: &str) -> String {
        map_segment_with_origin_url(app_state, proxy_seq, extension, &format!("{proxy_seq}.{extension}")).await
    }

    async fn map_segment_with_origin_url(
        app_state: &Arc<AppState>,
        proxy_seq: u64,
        _extension: &str,
        origin_url: &str,
    ) -> String {
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let manifest =
            normal_manifest(&format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{proxy_seq}\n#EXTINF:4.0,\n{origin_url}\n"));
        let mut session = session.write().await;
        session.proxy_next_seq = Some(proxy_seq);
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.proxy_session_id.0.clone()
    }

    async fn map_ready_segment(app_state: &Arc<AppState>, proxy_seq: u64, extension: &str, body: &[u8]) -> String {
        let proxy_session_id = map_ready_segment_without_lease(app_state, proxy_seq, extension, body).await;
        grant_hls_proxy_lease(app_state, &proxy_session_id).await;
        proxy_session_id
    }

    async fn map_ready_segment_without_lease(
        app_state: &Arc<AppState>,
        proxy_seq: u64,
        extension: &str,
        body: &[u8],
    ) -> String {
        let proxy_session_id = map_segment(app_state, proxy_seq, extension).await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        let cache_key = {
            let session = session.read().await;
            session.segments.get(&proxy_seq).expect("segment should be mapped").cache_key.clone()
        };
        let metadata = app_state
            .hls_proxy
            .segment_cache()
            .write_bytes_and_commit(&cache_key, body)
            .await
            .expect("cache commit should succeed");
        {
            let mut session = session.write().await;
            session.segments.get_mut(&proxy_seq).expect("segment should be mapped").status =
                SegmentCacheStatus::Ready { content_length: metadata.size, ready_at_ms: 200 };
        }
        proxy_session_id
    }

    async fn map_hls_map(app_state: &Arc<AppState>, body: &[u8], grant_lease: bool) -> String {
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n000123.m4s\n");
        let proxy_session_id = {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            session.proxy_session_id.0.clone()
        };
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        let cache_key = {
            let session = session.read().await;
            session.maps.get(&ProxyMapId(0)).expect("map should be mapped").cache_key.clone()
        };
        let metadata = app_state
            .hls_proxy
            .segment_cache()
            .write_bytes_and_commit(&cache_key, body)
            .await
            .expect("map cache commit should succeed");
        {
            let mut session = session.write().await;
            session.maps.get_mut(&ProxyMapId(0)).expect("map should be mapped").status =
                MapCacheStatus::Ready { content_length: metadata.size, ready_at_ms: 200 };
        }
        if grant_lease {
            grant_hls_proxy_lease(app_state, &proxy_session_id).await;
        }
        proxy_session_id
    }

    async fn map_transient_resource(
        app_state: &Arc<AppState>,
        origin_url: &str,
        extension: &str,
        grant_lease: bool,
    ) -> (String, String) {
        map_transient_resource_with_kind(app_state, origin_url, extension, grant_lease, TransientResourceKind::Segment)
            .await
    }

    async fn map_transient_resource_with_kind(
        app_state: &Arc<AppState>,
        origin_url: &str,
        extension: &str,
        grant_lease: bool,
        kind: TransientResourceKind,
    ) -> (String, String) {
        let secret = b"rewrite-secret";
        let now_ms = super::current_time_millis();
        let session = app_state.hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), secret, now_ms).await;
        let resource_id = build_transient_resource_id(origin_url, secret);
        let proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            session.transient.upsert_resources([TransientResourceRef::new(
                kind,
                origin_url,
                secret,
                now_ms,
                300_000,
                Some(extension.to_string()),
            )]);
            session.proxy_session_id.0.clone()
        };
        if grant_lease {
            grant_hls_proxy_lease(app_state, &proxy_session_id).await;
        }
        (proxy_session_id, resource_id.0)
    }

    async fn get_response(app_state: Arc<AppState>, uri: &str, range: Option<&str>) -> Response<Body> {
        request_response(app_state, Method::GET, uri, range).await
    }

    async fn request_response(
        app_state: Arc<AppState>,
        method: Method,
        uri: &str,
        range: Option<&str>,
    ) -> Response<Body> {
        let router = hls_api_register().with_state(app_state);
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let mut request = request.body(Body::empty()).expect("request should build");
        request.extensions_mut().insert(ConnectInfo(test_addr()));
        router.oneshot(request).await.expect("response")
    }

    async fn get_status(app_state: Arc<AppState>, uri: &str) -> StatusCode {
        get_response(app_state, uri, None).await.status()
    }

    async fn hls_session_last_media_at_ms(app_state: &Arc<AppState>, proxy_session_id: &str) -> Option<u64> {
        app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.to_string()))
            .await
            .expect("session should exist")
            .read()
            .await
            .activity
            .last_authorized_media_at_ms
    }

    async fn assert_no_hls_cache_stream_registered(app_state: &Arc<AppState>) {
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    async fn response_body(response: Response<Body>) -> bytes::Bytes {
        response.into_body().collect().await.expect("body should collect").to_bytes()
    }

    async fn single_variant_master_playlist(response: Response<Body>) -> (u32, String) {
        let body = response_body(response).await;
        let body = std::str::from_utf8(&body).expect("master playlist should be UTF-8");
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("#EXTM3U"));
        let bandwidth = lines
            .next()
            .and_then(|line| line.strip_prefix("#EXT-X-STREAM-INF:BANDWIDTH="))
            .and_then(|value| value.parse::<u32>().ok())
            .expect("positive master playlist bandwidth");
        let uri = lines.next().expect("single variant URI").to_string();
        assert!(lines.next().is_none(), "master playlist must contain exactly one variant");
        (bandwidth, uri)
    }

    async fn single_variant_uri(response: Response<Body>) -> String { single_variant_master_playlist(response).await.1 }

    fn access_lease_id_from_variant_uri(uri: &str) -> &str {
        uri.trim_end_matches("/manifest.m3u8").rsplit('/').next().expect("access lease id in variant URI")
    }

    fn proxy_session_id_from_variant_uri(uri: &str) -> &str {
        let mut parts = uri.trim_end_matches("/manifest.m3u8").rsplit('/');
        let _access_lease_id = parts.next().expect("access lease id in variant URI");
        parts.next().expect("proxy session id in variant URI")
    }

    fn manifest_media_sequence(body: &str) -> u64 {
        body.lines()
            .find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .and_then(|value| value.parse().ok())
            .expect("media sequence")
    }

    async fn access_lease_session_token(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        access_lease_id: &HlsAccessLeaseId,
    ) -> String {
        app_state
            .hls_proxy
            .access_lease(access_lease_id, proxy_session_id, super::current_time_millis())
            .await
            .expect("access lease should exist")
            .user_session_token
    }

    #[test]
    fn transient_full_object_cacheable_request_accepts_open_zero_range() {
        use crate::api::model::is_hls_transient_full_object_cacheable_request;

        assert!(is_hls_transient_full_object_cacheable_request(None));
        assert!(is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=0-"))));
        assert!(!is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=4-"))));
        assert!(!is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=-4"))));
        assert!(!is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=0-1,4-5"))));
    }

    async fn assert_hls_cache_stream_registered(app_state: &Arc<AppState>, proxy_session_id: &str) {
        let streams = app_state.active_users.active_streams().await;
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.username, "hls-user");
        assert_eq!(stream.session_token.as_deref(), Some("hls-session-token"));
        assert_eq!(stream.provider.as_ref(), "origin-provider");
        assert_eq!(stream.channel.item_type, PlaylistItemType::LiveHls);
        assert!(stream.channel.shared);
        assert_eq!(
            stream.channel.shared_stream_id,
            Some(super::hls_cache_shared_stream_id(&ProxySessionId(proxy_session_id.to_string())))
        );
        assert_eq!(stream.channel.shared_joined_existing, Some(false));
        assert_eq!(stream.channel.url.as_ref(), format!("/hls/shared/live/{proxy_session_id}/manifest.m3u8"));
        assert!(!stream.channel.url.contains("test-access-lease"));
        assert!(!stream.channel.url.contains("hls-session-token"));
        assert!(!stream.channel.url.contains("origin.example.com"));
        assert!(!stream.channel.url.contains("/hls/hls-user/"));
    }

    fn stats_provider_test_user_session(provider: &str) -> UserSession {
        UserSession {
            token: "stats-session-token".to_string(),
            transition_version: 0,
            virtual_id: 12345,
            provider: Arc::from(provider),
            stream_url: Arc::from("http://origin.example.com/live/12345.m3u8"),
            provider_session_headers: HashMap::new(),
            addr: test_addr(),
            socket_bound: false,
            active_addrs: Vec::new(),
            ts: 100,
            started_at: 100,
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            lifecycle: PlaybackLifecycle::Active,
        }
    }

    #[test]
    fn hls_cache_stats_provider_prefers_active_origin_account_binding() {
        let origin_source = HlsOriginSource::new(1, Arc::from("cdn-dev"), "12345", HlsOriginSourceKind::XtreamLive);
        let proxy_session_id = ProxySessionId("stats-session".to_string());
        let binding = HlsOriginAccountBinding::new(
            Arc::clone(&origin_source.input_name),
            Arc::from("cdn-dev-alias"),
            &proxy_session_id,
            100,
        );
        let user_session = stats_provider_test_user_session("cdn-dev");

        let provider = super::hls_cache_stats_provider(&origin_source, Some(&binding), &user_session);

        assert_eq!(provider.as_ref(), "cdn-dev-alias");
    }

    #[test]
    fn hls_cache_stats_provider_falls_back_when_origin_account_binding_is_not_active() {
        let origin_source = HlsOriginSource::new(1, Arc::from("cdn-dev"), "12345", HlsOriginSourceKind::XtreamLive);
        let proxy_session_id = ProxySessionId("stats-session".to_string());
        let mut binding = HlsOriginAccountBinding::new(
            Arc::clone(&origin_source.input_name),
            Arc::from("cdn-dev-alias"),
            &proxy_session_id,
            100,
        );
        binding.detach(HlsOriginAccountDetachedReason::Cleanup, 200);
        let user_session = stats_provider_test_user_session("session-provider");

        let provider = super::hls_cache_stats_provider(&origin_source, Some(&binding), &user_session);

        assert_eq!(provider.as_ref(), "session-provider");
    }

    #[test]
    fn hls_cache_stats_provider_falls_back_to_input_name_without_session_provider() {
        let origin_source = HlsOriginSource::new(1, Arc::from("cdn-dev"), "12345", HlsOriginSourceKind::XtreamLive);
        let user_session = stats_provider_test_user_session("");

        let provider = super::hls_cache_stats_provider(&origin_source, None, &user_session);

        assert_eq!(provider.as_ref(), "cdn-dev");
    }

    async fn register_hls_cache_stream_for_stats_test(
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        session_token: &str,
        fingerprint: &Fingerprint,
        lease_id: &str,
    ) {
        create_active_hls_user_session_with(
            app_state,
            session_token,
            "origin-provider",
            "http://origin.example.com/live/12345.m3u8",
            fingerprint.addr,
        )
        .await;
        let context = test_hls_access_context_with(
            proxy_session_id.clone(),
            HlsAccessLeaseId(lease_id.to_string()),
            session_token,
            fingerprint.key.clone(),
        );
        super::ensure_hls_cache_stream_registered(app_state, fingerprint, &HeaderMap::new(), &context, session)
            .await
            .expect("HLS stream registers");
    }

    fn find_stream_by_session_token(
        streams: &[shared::model::StreamInfo],
        session_token: &str,
    ) -> shared::model::StreamInfo {
        streams
            .iter()
            .find(|stream| stream.session_token.as_deref() == Some(session_token))
            .unwrap_or_else(|| panic!("{session_token} stream should exist"))
            .clone()
    }

    #[tokio::test]
    async fn hls_cache_stream_stats_mark_additional_viewers_as_joined_existing() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;
        let proxy_session_id = ProxySessionId(proxy_session_id);
        let shared_stream_id = super::hls_cache_shared_stream_id(&proxy_session_id);
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("session should exist");
        let first_fingerprint = test_fingerprint();
        let second_fingerprint = test_fingerprint_with_addr(test_addr_with_port(55124));
        register_hls_cache_stream_for_stats_test(
            &app_state,
            &session,
            &proxy_session_id,
            "hls-session-token",
            &first_fingerprint,
            "first-access-lease",
        )
        .await;

        let streams = app_state.active_users.active_streams().await;
        let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
        let first_meter_uid = first_stream.meter_uid;
        assert!(first_stream.channel.shared);
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));

        register_hls_cache_stream_for_stats_test(
            &app_state,
            &session,
            &proxy_session_id,
            "hls-second-session-token",
            &second_fingerprint,
            "second-access-lease",
        )
        .await;

        let streams = app_state.active_users.active_streams().await;
        let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
        let second_stream = find_stream_by_session_token(&streams, "hls-second-session-token");
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
        assert_eq!(second_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(second_stream.channel.shared_joined_existing, Some(true));

        register_hls_cache_stream_for_stats_test(
            &app_state,
            &session,
            &proxy_session_id,
            "hls-session-token",
            &first_fingerprint,
            "first-access-lease",
        )
        .await;

        let streams = app_state.active_users.active_streams().await;
        let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
        assert_eq!(first_stream.meter_uid, first_meter_uid);
    }

    struct TestSegmentOrigin {
        base_url: String,
        key_requests: Arc<AtomicUsize>,
        manifest_requests: Arc<AtomicUsize>,
        segment_requests: Arc<AtomicUsize>,
        key_bytes: Option<Arc<RwLock<Arc<[u8]>>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestSegmentOrigin {
        fn drop(&mut self) { self.task.abort(); }
    }

    impl TestSegmentOrigin {
        fn key_request_count(&self) -> usize { self.key_requests.load(Ordering::SeqCst) }

        fn manifest_request_count(&self) -> usize { self.manifest_requests.load(Ordering::SeqCst) }

        fn segment_request_count(&self) -> usize { self.segment_requests.load(Ordering::SeqCst) }

        async fn set_key_bytes(&self, bytes: Arc<[u8]>) {
            if let Some(key_bytes) = &self.key_bytes {
                *key_bytes.write().await = bytes;
            }
        }
    }

    async fn spawn_test_segment_origin(body: &'static [u8]) -> TestSegmentOrigin {
        spawn_test_status_origin(StatusCode::OK, body).await
    }

    async fn spawn_test_encrypted_hls_origin(
        manifest: &'static [u8],
        key_bytes: Arc<[u8]>,
        plaintext_segment: Arc<[u8]>,
    ) -> TestSegmentOrigin {
        let manifest = Arc::<[u8]>::from(manifest);
        let key_bytes = Arc::new(RwLock::new(key_bytes));
        let key_bytes_for_task = Arc::clone(&key_bytes);
        let key_requests = Arc::new(AtomicUsize::new(0));
        let key_requests_for_task = Arc::clone(&key_requests);
        let manifest_requests = Arc::new(AtomicUsize::new(0));
        let manifest_requests_for_task = Arc::clone(&manifest_requests);
        let segment_requests = Arc::new(AtomicUsize::new(0));
        let segment_requests_for_task = Arc::clone(&segment_requests);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let manifest = Arc::clone(&manifest);
                let key_bytes = Arc::clone(&key_bytes_for_task);
                let key_requests = Arc::clone(&key_requests_for_task);
                let manifest_requests = Arc::clone(&manifest_requests_for_task);
                let segment_requests = Arc::clone(&segment_requests_for_task);
                let plaintext_segment = Arc::clone(&plaintext_segment);
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 2048];
                    let Ok(read) = socket.read(&mut request).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let path = String::from_utf8_lossy(&request[..read])
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .map_or_else(|| "/".to_string(), str::to_owned);
                    let current_key_bytes = Arc::clone(&*key_bytes.read().await);
                    let body = if path_has_extension(&path, "m3u8") {
                        manifest_requests.fetch_add(1, Ordering::SeqCst);
                        manifest
                    } else if path.ends_with("key.bin") {
                        key_requests.fetch_add(1, Ordering::SeqCst);
                        current_key_bytes
                    } else if let Some(origin_sequence) = path
                        .rsplit('/')
                        .next()
                        .and_then(|file| file.strip_suffix(".ts"))
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        segment_requests.fetch_add(1, Ordering::SeqCst);
                        Arc::from(encrypt_test_aes128_cbc_pkcs7(
                            &plaintext_segment,
                            &current_key_bytes,
                            test_hls_sequence_iv(origin_sequence),
                        ))
                    } else {
                        Arc::<[u8]>::from([])
                    };
                    let response =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    if socket.write_all(response.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });
        TestSegmentOrigin {
            base_url: format!("http://{addr}"),
            key_requests,
            manifest_requests,
            segment_requests,
            key_bytes: Some(key_bytes),
            task,
        }
    }

    fn encrypt_test_aes128_cbc_pkcs7(plaintext: &[u8], key: &[u8], iv: [u8; 16]) -> Vec<u8> {
        let padding_len = 16 - (plaintext.len() % 16);
        let mut ciphertext = plaintext.to_vec();
        ciphertext.resize(
            plaintext.len().saturating_add(padding_len),
            u8::try_from(padding_len).expect("PKCS#7 AES-128 padding fits in u8"),
        );
        let cipher = Aes128::new_from_slice(key).expect("test key has AES-128 length");
        let mut previous = iv;
        for block in ciphertext.as_chunks_mut::<16>().0 {
            for (byte, previous) in block.iter_mut().zip(previous) {
                *byte ^= previous;
            }
            let mut encrypted = Block::<Aes128>::default();
            encrypted.copy_from_slice(block);
            cipher.encrypt_block(&mut encrypted);
            block.copy_from_slice(&encrypted);
            previous.copy_from_slice(block);
        }
        ciphertext
    }

    fn test_hls_sequence_iv(sequence: u64) -> [u8; 16] {
        let mut iv = [0_u8; 16];
        iv[8..].copy_from_slice(&sequence.to_be_bytes());
        iv
    }

    struct TestBinaryOriginResponse {
        status: StatusCode,
        location: Option<String>,
        body: Arc<[u8]>,
    }

    impl TestBinaryOriginResponse {
        fn new(status: StatusCode, body: Arc<[u8]>) -> Self { Self { status, location: None, body } }

        fn redirect(location: String) -> Self {
            Self { status: StatusCode::FOUND, location: Some(location), body: Arc::from(&b""[..]) }
        }
    }

    type TestBinaryOriginHandler = Arc<dyn Fn(&str) -> TestBinaryOriginResponse + Send + Sync>;

    async fn spawn_test_binary_origin(handler: TestBinaryOriginHandler) -> TestSegmentOrigin {
        let key_requests = Arc::new(AtomicUsize::new(0));
        let manifest_requests = Arc::new(AtomicUsize::new(0));
        let segment_requests = Arc::new(AtomicUsize::new(0));
        let manifest_requests_for_task = Arc::clone(&manifest_requests);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                let manifest_requests = Arc::clone(&manifest_requests_for_task);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 2048];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..read]);
                    let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                    if path_has_extension(path, "m3u8") {
                        manifest_requests.fetch_add(1, Ordering::SeqCst);
                    }
                    let TestBinaryOriginResponse { status, location, body } = handler(path);
                    let reason = status.canonical_reason().unwrap_or("Status");
                    let location_header =
                        location.map_or_else(String::new, |location| format!("Location: {location}\r\n"));
                    let response = format!(
                        "HTTP/1.1 {} {reason}\r\n{location_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        status.as_u16(),
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        TestSegmentOrigin {
            base_url: format!("http://{addr}"),
            key_requests,
            manifest_requests,
            segment_requests,
            key_bytes: None,
            task,
        }
    }

    async fn spawn_test_status_origin(status: StatusCode, body: &'static [u8]) -> TestSegmentOrigin {
        let body = Arc::<[u8]>::from(body);
        spawn_test_binary_origin(Arc::new(move |_path| TestBinaryOriginResponse::new(status, Arc::clone(&body)))).await
    }

    struct TestEncodedManifestOrigin {
        base_url: String,
        requests: Arc<tokio::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestEncodedManifestOrigin {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn spawn_test_encoded_manifest_origin(
        content_encoding: Option<&'static str>,
        body: Vec<u8>,
        body_delay: Duration,
    ) -> TestEncodedManifestOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let body = Arc::new(body);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 2048];
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                    }
                    requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len());
                    if let Some(content_encoding) = content_encoding {
                        let _ = writeln!(&mut response, "Content-Encoding: {content_encoding}\r");
                    }
                    response.push_str("Connection: close\r\n\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    if body_delay.is_zero() {
                        let _ = socket.write_all(body.as_slice()).await;
                    } else {
                        let split_at = body.len().min(4);
                        let _ = socket.write_all(&body[..split_at]).await;
                        tokio::time::sleep(body_delay).await;
                        let _ = socket.write_all(&body[split_at..]).await;
                    }
                });
            }
        });
        TestEncodedManifestOrigin { base_url: format!("http://{addr}"), requests, task }
    }

    fn legacy_manifest_test_input(origin: &TestEncodedManifestOrigin) -> crate::model::InputSource {
        crate::model::InputSource {
            name: Arc::from("legacy-content-coding-test"),
            url: format!("{}/manifest.m3u8", origin.base_url),
            provider: None,
            username: None,
            password: None,
            method: shared::model::InputFetchMethod::GET,
            headers: HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]),
        }
    }

    fn legacy_manifest_test_client_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        headers
    }

    async fn encode_test_manifest(content_encoding: &str, body: &[u8]) -> Vec<u8> {
        match content_encoding {
            "gzip" => {
                let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
                encoder.write_all(body).await.expect("gzip test body encodes");
                encoder.shutdown().await.expect("gzip test encoder finishes");
                encoder.into_inner()
            }
            "deflate" => {
                let mut encoder = async_compression::tokio::write::DeflateEncoder::new(Vec::new());
                encoder.write_all(body).await.expect("deflate test body encodes");
                encoder.shutdown().await.expect("deflate test encoder finishes");
                encoder.into_inner()
            }
            "br" => {
                let mut encoder = async_compression::tokio::write::BrotliEncoder::new(Vec::new());
                encoder.write_all(body).await.expect("brotli test body encodes");
                encoder.shutdown().await.expect("brotli test encoder finishes");
                encoder.into_inner()
            }
            "zstd" => {
                let mut encoder = async_compression::tokio::write::ZstdEncoder::new(Vec::new());
                encoder.write_all(body).await.expect("zstd test body encodes");
                encoder.shutdown().await.expect("zstd test encoder finishes");
                encoder.into_inner()
            }
            _ => panic!("unsupported test Content-Encoding: {content_encoding}"),
        }
    }

    async fn wait_for_hls_test_session(app_state: &Arc<AppState>, session_key: &HlsSessionKey) -> HlsSessionHandle {
        for _ in 0..50 {
            if let Some(session) = app_state.hls_proxy.sessions().get_by_key(session_key).await {
                return session;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shared hls test session should be created");
    }

    async fn wait_for_hls_refresh_in_flight(session: &HlsSessionHandle) {
        for _ in 0..50 {
            if session.read().await.origin_refresh.in_flight {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shared hls refresh should be in flight");
    }

    struct TestTransientOrigin {
        base_url: String,
        requests: Arc<tokio::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestTransientOrigin {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn spawn_test_transient_origin() -> TestTransientOrigin {
        spawn_test_transient_origin_with_response(
            "206 Partial Content",
            &[
                ("Content-Type", "video/mp2t"),
                ("Content-Range", "bytes 2-15/16"),
                ("Accept-Ranges", "bytes"),
                ("Cache-Control", "no-store"),
                ("ETag", "\"abc\""),
                ("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
            ],
            "transient-body",
        )
        .await
    }

    async fn spawn_test_transient_origin_with_response(
        status_line: &'static str,
        response_headers: &'static [(&'static str, &'static str)],
        body: &'static str,
    ) -> TestTransientOrigin {
        spawn_test_transient_origin_with_delayed_binary_response(
            status_line,
            response_headers,
            body.as_bytes().to_vec(),
            Duration::ZERO,
        )
        .await
    }

    async fn spawn_test_transient_origin_with_delayed_response(
        status_line: &'static str,
        response_headers: &'static [(&'static str, &'static str)],
        body: &'static str,
        response_delay: Duration,
    ) -> TestTransientOrigin {
        spawn_test_transient_origin_with_delayed_binary_response(
            status_line,
            response_headers,
            body.as_bytes().to_vec(),
            response_delay,
        )
        .await
    }

    async fn spawn_test_transient_origin_with_delayed_binary_response(
        status_line: &'static str,
        response_headers: &'static [(&'static str, &'static str)],
        body: Vec<u8>,
        response_delay: Duration,
    ) -> TestTransientOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let body = Arc::new(body);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    requests.lock().await.push(request);
                    if !response_delay.is_zero() {
                        tokio::time::sleep(response_delay).await;
                    }
                    let mut response_head = format!("HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n", body.len());
                    for (name, value) in response_headers {
                        let _ = writeln!(&mut response_head, "{name}: {value}\r");
                    }
                    response_head.push_str("Connection: close\r\n\r\n");
                    let _ = socket.write_all(response_head.as_bytes()).await;
                    let _ = socket.write_all(body.as_slice()).await;
                });
            }
        });
        TestTransientOrigin { base_url: format!("http://{addr}"), requests, task }
    }

    #[tokio::test]
    async fn valid_hls_proxy_segment_without_session_returns_not_found() {
        let status = get_status(test_app_state(), "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/000123.ts").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn valid_hls_proxy_segment_with_not_ready_session_returns_not_found() {
        let app_state = test_app_state();
        let proxy_session_id = map_segment(&app_state, 123, "ts").await;

        let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/000123.ts")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn valid_hls_proxy_segment_with_not_ready_and_valid_lease_returns_service_unavailable() {
        let app_state = test_app_state();
        let proxy_session_id = map_segment(&app_state, 123, "ts").await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        session.write().await.segments.get_mut(&123).expect("segment should exist").origin_fetch_ref = None;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn not_ready_hls_proxy_segment_with_fetch_ref_demand_fetches_and_returns_ok() {
        let origin = spawn_test_segment_origin(b"0123456789").await;
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id =
            map_segment_with_origin_url(&app_state, 123, "ts", &format!("{}/seg.ts", origin.base_url)).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    }

    #[tokio::test]
    async fn not_ready_hls_proxy_segment_with_range_waits_for_demand_fetch_then_returns_partial() {
        let origin = spawn_test_segment_origin(b"0123456789").await;
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id =
            map_segment_with_origin_url(&app_state, 123, "ts", &format!("{}/seg.ts", origin.base_url)).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
    }

    #[tokio::test]
    async fn not_ready_hls_proxy_segment_without_fetch_ref_returns_service_unavailable() {
        let app_state = test_app_state();
        let proxy_session_id = map_segment(&app_state, 123, "ts").await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        session.write().await.segments.get_mut(&123).expect("segment should exist").origin_fetch_ref = None;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_without_lease_returns_not_found() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;

        let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/000123.ts")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn invalid_normal_segment_uri_never_redirects_or_serves_terminal_media() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "broken.ts").await;

        let response = get_response(app_state, &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::LOCATION));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_marked_for_gc_returns_not_found_without_redirect() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        session.write().await.mark_for_gc_removal();

        let response = get_response(app_state, &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::LOCATION));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_without_range_returns_ok() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=300, immutable");
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        {
            let session = session.read().await;
            let segment = session.segments.get(&123).expect("segment should exist");
            assert_eq!(segment.access.active_readers(), 1);
            assert!(matches!(segment.status, SegmentCacheStatus::Ready { content_length: 10, .. }));
        }

        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));

        {
            let session = session.read().await;
            let segment = session.segments.get(&123).expect("segment should exist");
            assert_eq!(segment.access.active_readers(), 0);
            assert!(segment.access.last_accessed_at_ms() > 0);
            assert!(session.activity.last_authorized_media_at_ms.is_some());
        }
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_range_zero_open_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "m4s", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.m4s").await;

        let response = get_response(app_state, &uri, Some("bytes=0-")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-9/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_range_start_open_returns_partial_content_from_offset() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "m4v", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.m4v").await;

        let response = get_response(app_state, &uri, Some("bytes=4-")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 4-9/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "6");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"456789"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_range_start_end_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_suffix_range_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("bytes=-3")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 7-9/10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"789"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_unsatisfiable_range_returns_416() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=99-")).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_multi_range_returns_416() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-1,4-5")).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_without_lease_returns_not_found() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"map-body", false).await;

        let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/map/000000.mp4")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_without_range_returns_ok() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
        assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_range_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_multi_range_returns_416_with_zero_content_length() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-1,4-5")).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn hls_proxy_map_not_ready_with_valid_lease_returns_service_unavailable() {
        let app_state = test_app_state();
        let session =
            app_state.hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"rewrite-secret", 100).await;
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n000123.m4s\n");
        let proxy_session_id = {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            session.proxy_session_id.0.clone()
        };
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_without_lease_returns_not_found() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", false).await;

        let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/r/{resource_id}.ts")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn transient_resource_with_valid_lease_streams_origin_response_and_headers() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        {
            let mut session = session.write().await;
            session.origin_request_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
            session.origin_request_headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
            session
                .origin_request_headers
                .insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
            session.origin_request_headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
            session
                .origin_request_headers
                .insert(HeaderName::from_static("x-tuliprox-main-revision"), HeaderValue::from_static("secret"));
            session.origin_request_headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));
        }

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-15/16");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::ETAG], "\"abc\"");
        assert_eq!(response.headers()[header::LAST_MODIFIED], "Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"transient-body"));
        assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
        let origin_requests = origin.requests.lock().await;
        let origin_request = origin_requests.first().expect("origin request").to_ascii_lowercase();
        assert!(origin_request.contains("range: bytes=2-15"));
        assert!(origin_request.contains("accept-language: de"));
        assert!(!origin_request.contains("authorization: bearer secret"));
        assert!(!origin_request.contains("cookie: sid=secret"));
        assert!(!origin_request.contains("proxy-authorization: basic secret"));
        assert!(!origin_request.contains("host: proxy.example.com"));
        assert!(!origin_request.contains("x-tuliprox-main-revision"));
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    }

    #[tokio::test]
    async fn transient_resource_without_range_is_cached_after_first_fetch() {
        let app_state = test_app_state();
        let origin =
            spawn_test_transient_origin_with_response("200 OK", &[("Content-Type", "video/mp2t")], "0123456789").await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first = get_response(Arc::clone(&app_state), &uri, None).await;
        let second = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn transient_resource_range_from_zero_is_cached_as_full_object() {
        let app_state = test_app_state();
        let origin =
            spawn_test_transient_origin_with_response("200 OK", &[("Content-Type", "video/mp2t")], "0123456789").await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-")).await;
        let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=4-")).await;

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"456789"));
        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity"));
        assert!(!request.contains("\r\nrange:"));
    }

    #[tokio::test]
    async fn transient_cache_fill_rejects_identity_partial_without_ready_object_or_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(Arc::clone(&hls_proxy));
        disable_custom_stream_response(&app_state);
        let origin = spawn_test_transient_origin_with_response(
            "206 Partial Content",
            &[("Content-Type", "video/mp2t"), ("Content-Range", "bytes 0-3/10")],
            "part",
        )
        .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let proxy_session_id = ProxySessionId(proxy_session_id);
        let cache_key = TransientObjectCacheKey::new(proxy_session_id.clone(), TransientResourceId(resource_id), "ts");
        let session =
            hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.expect("session should exist");
        let status =
            session.read().await.transient.object_cache.get(&cache_key).expect("transient cache entry").status.clone();
        assert!(matches!(status, TransientObjectCacheStatus::FailedPermanent { status: None, .. }));
        assert!(hls_proxy.segment_cache().metadata(&cache_key).await.expect("cache metadata reads").is_none());
        assert!(!hls_proxy.segment_cache().has_active_temp_files());
        assert_eq!(std::fs::read_dir(temp_dir.path()).expect("cache root reads").count(), 0);

        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity"));
        assert!(!request.contains("\r\nrange:"));
    }

    #[tokio::test]
    async fn transient_resource_range_from_zero_waits_for_inflight_object_cache_fetch() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin_with_delayed_response(
            "200 OK",
            &[("Content-Type", "video/mp2t")],
            "0123456789",
            Duration::from_millis(150),
        )
        .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first_app_state = Arc::clone(&app_state);
        let first_uri = uri.clone();
        let first = tokio::spawn(async move { get_response(first_app_state, &first_uri, Some("bytes=0-")).await });
        for _ in 0..50 {
            if origin.requests.lock().await.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(origin.requests.lock().await.len(), 1);

        let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-")).await;
        let first = first.await.expect("first request joins");

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn transient_resource_range_from_offset_without_ready_object_is_not_cached() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;
        let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(origin.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn transient_key_resource_is_not_cached() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin_with_response(
            "200 OK",
            &[("Content-Type", "application/octet-stream")],
            "key-bytes",
        )
        .await;
        let (proxy_session_id, resource_id) = map_transient_resource_with_kind(
            &app_state,
            &format!("{}/key.bin", origin.base_url),
            "key",
            true,
            TransientResourceKind::Key,
        )
        .await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.key")).await;

        let first = get_response(Arc::clone(&app_state), &uri, None).await;
        let second = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"key-bytes"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"key-bytes"));
        assert_eq!(origin.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn transient_resource_origin_error_does_not_mark_media_activity() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin_with_response(
            "500 Internal Server Error",
            &[("Content-Type", "text/plain")],
            "origin-error",
        )
        .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()), Some("1"));
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_permanent_origin_error_never_redirects_to_manifest() {
        let app_state = test_app_state();
        let origin =
            spawn_test_transient_origin_with_response("404 Not Found", &[("Content-Type", "text/plain")], "missing")
                .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_permanent_origin_error_returns_not_found_when_custom_response_disabled() {
        let app_state = test_app_state();
        disable_custom_stream_response(&app_state);
        let origin =
            spawn_test_transient_origin_with_response("404 Not Found", &[("Content-Type", "text/plain")], "missing")
                .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_holds_provider_handle_until_origin_body_is_finished() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let proxy_session_id_value = ProxySessionId(proxy_session_id.clone());
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id_value)
            .await
            .expect("session should exist");
        {
            let mut session = session.write().await;
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &proxy_session_id_value,
                super::current_time_millis(),
            ));
        }
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"transient-body"));
        wait_for_provider_connection_count(&app_state, 0).await;
    }

    #[tokio::test]
    async fn transient_decoder_failure_releases_origin_and_access_guards_once() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let mut truncated = encode_test_manifest("gzip", b"transient decoder failure").await;
        truncated.truncate(truncated.len().saturating_sub(8));
        let origin = spawn_test_transient_origin_with_delayed_binary_response(
            "200 OK",
            &[("Content-Type", "video/mp2t"), ("Content-Encoding", "gzip")],
            truncated,
            Duration::ZERO,
        )
        .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let proxy_session_id_value = ProxySessionId(proxy_session_id.clone());
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id_value)
            .await
            .expect("session should exist");
        {
            let mut session = session.write().await;
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &proxy_session_id_value,
                super::current_time_millis(),
            ));
        }
        let resource = session
            .read()
            .await
            .transient
            .resources
            .get(&TransientResourceId(resource_id.clone()))
            .cloned()
            .expect("transient resource exists");
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-")).await;

        assert_eq!(response.status(), StatusCode::OK);
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(resource.active_readers(), 1);
        assert!(response.into_body().collect().await.is_err());
        wait_for_provider_connection_count(&app_state, 0).await;
        assert_eq!(resource.active_readers(), 0);
        tokio::task::yield_now().await;
        assert_eq!(app_state.active_provider.get_provider_connections_count().await, 0);
        assert_eq!(origin.requests.lock().await.len(), 1, "body failure must not start another origin request");
    }

    #[tokio::test]
    async fn transient_unknown_resource_never_redirects_to_manifest() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, _) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "r/unknown.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::LOCATION));
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_unknown_resource_returns_not_found_when_custom_response_disabled() {
        let app_state = test_app_state();
        disable_custom_stream_response(&app_state);
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, _) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "r/unknown.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[test]
    fn transient_cross_origin_redirect_strips_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("session=secret"));
        headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        headers.insert(header::HOST, HeaderValue::from_static("origin.example.com"));
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1"));

        crate::api::model::scrub_hls_origin_headers(&mut headers, None);

        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key(header::HOST));
        assert_eq!(headers[header::RANGE], "bytes=0-1");
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_unknown_range_unit_is_ignored() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("items=0-1")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    }

    #[tokio::test]
    async fn invalid_hls_proxy_file_names_return_not_found() {
        let app_state = test_app_state();

        assert_eq!(
            get_status(Arc::clone(&app_state), "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/123.ts").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_status(app_state, "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/map/000123.exe").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn legacy_hls_route_remains_registered() {
        let status = get_status(test_app_state(), "/hls/user/pass/1/2/3/not-a-token").await;

        assert_ne!(status, StatusCode::NOT_FOUND);
    }
}
