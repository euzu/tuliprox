use super::{
    safe_hls_access_lease_id, safe_proxy_session_id, safe_user_session_token, HlsAccessLease, HlsAccessLeaseId,
    HlsPlaybackFamilyKey, ProxySessionId,
};
use crate::{
    api::{
        api_utils::{connection_priority_for_kind, resolve_playback_request_admission, EvictionReentryGuard},
        model::AppState,
    },
    auth::Fingerprint,
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
    pub lease_id: HlsAccessLeaseId,
    pub family_key: HlsPlaybackFamilyKey,
    pub epg_reference_ts: Option<i64>,
    pub archive_origin_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsAccessLeaseValidationError {
    InvalidLease,
    SessionMismatch,
    UserSessionMissing,
    AdmissionDenied,
    Expired,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsAccessAdmissionMode {
    ManifestPrepare,
    ResourceAccess,
}

pub async fn validate_hls_access_lease(
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
    if lease.state == super::HlsAccessLeaseState::Denied {
        return Err(HlsAccessLeaseValidationError::AdmissionDenied);
    }
    validate_hls_access_lease_admission(app_state, fingerprint, lease, admission_mode).await
}

async fn validate_hls_access_lease_admission(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    lease: HlsAccessLease,
    admission_mode: HlsAccessAdmissionMode,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    let Some(user) = app_state.app_config.get_user_credentials(&lease.username) else {
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} session={} reason=user_missing",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::UserSessionMissing);
    };
    let Some(user_session) =
        app_state.active_users.get_and_update_user_session(&lease.username, &lease.user_session_token).await
    else {
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} session={} reason=session_missing",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::UserSessionMissing);
    };

    let (admission, _, _) = resolve_playback_request_admission(
        app_state,
        &user,
        fingerprint,
        PlaylistItemType::LiveHls,
        Some(&user_session),
        &lease.user_session_token,
        true,
        EvictionReentryGuard::Session(&lease.user_session_token),
        admission_mode == HlsAccessAdmissionMode::ManifestPrepare,
        false,
    )
    .await;
    if admission.permission == UserConnectionPermission::Exhausted
        || (admission.permission == UserConnectionPermission::GracePeriod && admission.kind.is_none())
    {
        app_state.hls_proxy.deny_access_lease(&lease.lease_id).await;
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} session={} reason=admission_denied",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::AdmissionDenied);
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
        app_state.hls_proxy.deny_access_lease(&lease.lease_id).await;
        warn!(
            "HLS access lease rejected: lease={} proxy_session={} session={} reason=origin_policy_missing",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&lease.proxy_session_id),
            safe_user_session_token(&lease.user_session_token)
        );
        return Err(HlsAccessLeaseValidationError::AdmissionDenied);
    };
    let priority = connection_priority_for_kind(&user, connection_kind);
    let _ =
        app_state.hls_proxy.update_access_lease_origin_acquire_policy(&lease.lease_id, connection_kind, priority).await;

    app_state.active_users.touch_http_activity(&lease.username, &lease.user_session_token, &fingerprint.addr).await;

    Ok(HlsAccessContext {
        username: lease.username,
        user_session_token: lease.user_session_token,
        proxy_session_id: lease.proxy_session_id,
        input_id: lease.input_id,
        stream_ref: lease.stream_ref,
        virtual_id: lease.virtual_id,
        lease_id: lease.lease_id,
        family_key: lease.family_key,
        epg_reference_ts: lease.epg_reference_ts,
        archive_origin_url: lease.archive_origin_url,
    })
}
