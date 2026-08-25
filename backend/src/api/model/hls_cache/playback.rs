use super::{
    lease::{
        HlsAccessLeaseDenialMode, HlsRuntimePolicyRevocationOutcome,
    },
    runtime_custom_tail::HlsRuntimeCustomTailReason,
    safe_hls_access_lease_id, safe_proxy_session_id, safe_user_session_token, HlsAccessLease, HlsAccessLeaseId,
    HlsPlaybackFamilyKey, ProxySessionId,
};
use crate::{
    api::{
        api_utils::{connection_priority_for_kind, resolve_playback_request_admission, EvictionReentryGuard},
        model::{AppState, UserSession},
    },
    auth::Fingerprint,
    model::ProxyUserCredentials,
};
use log::warn;
use shared::model::{PlaylistItemType, UserConnectionPermission};
use std::sync::Arc;

/// Placeholder stored in shared HLS manifests before per-user access lease IDs are inserted.
pub const HLS_ACCESS_LEASE_ID_PLACEHOLDER: &str = "__hls_access_lease_id__";

/// Validated user context restored from a server-side HLS access lease.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessContext {
    pub username: String,
    pub user_session_token: String,
    pub proxy_session_id: ProxySessionId,
    pub input_id: u16,
    pub stream_ref: String,
    pub virtual_id: u32,
    pub(crate) known_bitrate_bps: Option<u32>,
    pub lease_id: HlsAccessLeaseId,
    pub family_key: HlsPlaybackFamilyKey,
    pub epg_reference_ts: Option<i64>,
    pub archive_origin_url: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum HlsAccessLeaseValidationError {
    UserSessionMissing {
        runtime_tail: Option<HlsRuntimePolicyRevocationOutcome>,
    },
    UserAccountExpired {
        runtime_tail: Option<HlsRuntimePolicyRevocationOutcome>,
    },
    AdmissionDenied {
        runtime_tail: Option<HlsRuntimePolicyRevocationOutcome>,
        reason: Option<HlsRuntimeCustomTailReason>,
    },
    AvailabilityPending,
    Expired,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsAccessAdmissionMode {
    ManifestPrepare,
    ResourceAccess,
}

impl HlsAccessAdmissionMode {
    const fn prepares_manifest(self) -> bool { matches!(self, Self::ManifestPrepare) }

    /// Manifest denials retain the exact live lease long enough to commit an
    /// anchored finite tail; media-resource denials revoke origin access now.
    const fn revokes_lease_on_denial(self) -> bool { matches!(self, Self::ResourceAccess) }
}

pub(crate) async fn validate_hls_access_lease(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    path_proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    now_ms: u64,
    admission_mode: HlsAccessAdmissionMode,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    let Some(lease) = app_state.hls_proxy.access_lease(lease_id, path_proxy_session_id, now_ms).await else {
        return Err(HlsAccessLeaseValidationError::Expired);
    };
    if matches!(
        lease.state,
        super::HlsAccessLeaseState::PolicyRevoking | super::HlsAccessLeaseState::Denied
    ) {
        return Err(HlsAccessLeaseValidationError::AdmissionDenied {
            runtime_tail: lease.runtime_policy_revocation_outcome(),
            reason: lease.runtime_policy_denial_reason(),
        });
    }
    validate_hls_access_lease_admission(app_state, fingerprint, lease, admission_mode, now_ms).await
}

async fn validate_hls_access_lease_admission(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    lease: HlsAccessLease,
    admission_mode: HlsAccessAdmissionMode,
    now_ms: u64,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    let (user, user_session) =
        resolve_hls_access_lease_identity(app_state, &lease, admission_mode, now_ms).await?;

    let (admission, _, _) = resolve_playback_request_admission(
        app_state,
        &user,
        fingerprint,
        PlaylistItemType::LiveHls,
        Some(&user_session),
        &lease.user_session_token,
        true,
        EvictionReentryGuard::Session(&lease.user_session_token),
        admission_mode.prepares_manifest(),
        false,
    )
    .await;
    if admission.permission == UserConnectionPermission::Exhausted
        || (admission.permission == UserConnectionPermission::GracePeriod && admission.kind.is_none())
    {
        let runtime_tail = begin_runtime_policy_denial(
            app_state,
            &lease,
            admission_mode,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            now_ms,
        )
        .await;
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} user_session={} reason=admission_denied",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::AdmissionDenied {
            runtime_tail,
            reason: Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted),
        });
    }

    let Some(connection_kind) = app_state
        .active_users
        .refresh_session_connection_kind_for_origin_policy(
            &lease.username,
            user.max_connections,
            user.soft_connections,
            &lease.user_session_token,
        )
        .await
        .or(admission.kind)
    else {
        let runtime_tail = begin_runtime_policy_denial(
            app_state,
            &lease,
            admission_mode,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            now_ms,
        )
        .await;
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} user_session={} reason=origin_policy_missing",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::AdmissionDenied {
            runtime_tail,
            reason: Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted),
        });
    };
    let priority = connection_priority_for_kind(&user, connection_kind);
    if app_state
        .hls_proxy
        .update_access_lease_origin_acquire_policy(&lease.lease_id, connection_kind, priority)
        .await
        .is_none()
    {
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} user_session={} reason=expired_during_policy_update",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::Expired);
    }

    app_state.active_users.touch_http_activity(&lease.username, &lease.user_session_token, &fingerprint.addr).await;

    Ok(hls_access_context_from_lease(lease))
}

fn hls_access_context_from_lease(lease: HlsAccessLease) -> HlsAccessContext {
    HlsAccessContext {
        username: lease.username,
        user_session_token: lease.user_session_token,
        proxy_session_id: lease.proxy_session_id,
        input_id: lease.input_id,
        stream_ref: lease.stream_ref,
        virtual_id: lease.virtual_id,
        known_bitrate_bps: lease.known_bitrate_bps,
        lease_id: lease.lease_id,
        family_key: lease.family_key,
        epg_reference_ts: lease.epg_reference_ts,
        archive_origin_url: lease.archive_origin_url,
    }
}

async fn resolve_hls_access_lease_identity(
    app_state: &Arc<AppState>,
    lease: &HlsAccessLease,
    admission_mode: HlsAccessAdmissionMode,
    now_ms: u64,
) -> Result<(Arc<ProxyUserCredentials>, UserSession), HlsAccessLeaseValidationError> {
    let Some(user) = app_state.app_config.get_user_credentials(&lease.username) else {
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} user_session={} reason=user_missing",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        let runtime_tail = begin_runtime_policy_denial(
            app_state,
            lease,
            admission_mode,
            HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
            now_ms,
        )
        .await;
        return Err(HlsAccessLeaseValidationError::UserSessionMissing { runtime_tail });
    };
    if user.permission_denied(&app_state.app_config) {
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} user_session={} reason=user_account_expired",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        let runtime_tail = begin_runtime_policy_denial(
            app_state,
            lease,
            admission_mode,
            HlsRuntimeCustomTailReason::UserAccountExpired,
            now_ms,
        )
        .await;
        return Err(HlsAccessLeaseValidationError::UserAccountExpired { runtime_tail });
    }
    let Some(user_session) =
        app_state.active_users.get_and_update_user_session(&lease.username, &lease.user_session_token).await
    else {
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} user_session={} reason=session_missing",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        let runtime_tail = begin_runtime_policy_denial(
            app_state,
            lease,
            admission_mode,
            HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
            now_ms,
        )
        .await;
        return Err(HlsAccessLeaseValidationError::UserSessionMissing { runtime_tail });
    };
    Ok((user, user_session))
}

async fn begin_runtime_policy_denial(
    app_state: &Arc<AppState>,
    lease: &HlsAccessLease,
    admission_mode: HlsAccessAdmissionMode,
    reason: HlsRuntimeCustomTailReason,
    now_ms: u64,
) -> Option<HlsRuntimePolicyRevocationOutcome> {
    if !admission_mode.revokes_lease_on_denial() {
        return None;
    }
    let outcome = app_state
        .hls_proxy
        .begin_runtime_policy_revocation(
            &lease.lease_id,
            &lease.proxy_session_id,
            reason,
            now_ms,
        )
        .await;
    let denial_mode = match outcome {
        HlsRuntimePolicyRevocationOutcome::Started { .. }
        | HlsRuntimePolicyRevocationOutcome::AlreadyPending { .. } => None,
        HlsRuntimePolicyRevocationOutcome::AlreadyCommitted { .. } => {
            Some(HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail)
        }
        HlsRuntimePolicyRevocationOutcome::NoPublishedManifest
        | HlsRuntimePolicyRevocationOutcome::NoLongerEligible => {
            Some(HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd { reason })
        }
    };
    if let Some(denial_mode) = denial_mode {
        let _ = app_state.hls_proxy.deny_access_lease(&lease.lease_id, denial_mode).await;
    }
    Some(outcome)
}
