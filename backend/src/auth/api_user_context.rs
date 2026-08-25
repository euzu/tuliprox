use crate::model::AppConfig;
use crate::repository::GeoIp;
use arc_swap::ArcSwapOption;
use crate::model::ProxyUserPermissionDenyReason;
use crate::repository::{
    evaluate_network_access, log_network_access_allowed_geoip_unavailable, log_network_access_denied,
    NetworkAccessDecision, NetworkAccessDenyReason,
};
use axum::response::IntoResponse;
use log::debug;
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum PermissionDenyReason {
    Expired,
    Disabled,
    Banned,
    Inactive,
}

impl From<ProxyUserPermissionDenyReason> for PermissionDenyReason {
    fn from(reason: ProxyUserPermissionDenyReason) -> Self {
        match reason {
            ProxyUserPermissionDenyReason::Expired | ProxyUserPermissionDenyReason::ExpiredStatus => {
                PermissionDenyReason::Expired
            }
            ProxyUserPermissionDenyReason::Disabled => PermissionDenyReason::Disabled,
            ProxyUserPermissionDenyReason::Banned => PermissionDenyReason::Banned,
            ProxyUserPermissionDenyReason::Inactive
            | ProxyUserPermissionDenyReason::UnresolvedPlan
            | ProxyUserPermissionDenyReason::InvalidFilter => {
                PermissionDenyReason::Inactive
            }
        }
    }
}

#[derive(Debug)]
pub enum ApiUserAuthError {
    AuthFailed,
    PermissionDenied(PermissionDenyReason),
    NetworkDenied(NetworkAccessDenyReason),
}

impl std::fmt::Display for ApiUserAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiUserAuthError::AuthFailed => write!(f, "Authentication failed"),
            ApiUserAuthError::PermissionDenied(reason) => match reason {
                PermissionDenyReason::Expired => write!(f, "User access denied, expired"),
                PermissionDenyReason::Disabled => write!(f, "User access denied, status disabled"),
                PermissionDenyReason::Banned => write!(f, "User access denied, status banned"),
                PermissionDenyReason::Inactive => write!(f, "User access denied, status inactive"),
            },
            ApiUserAuthError::NetworkDenied(reason) => match reason {
                NetworkAccessDenyReason::NoCidrMatch => write!(f, "Network access denied, no CIDR match"),
                NetworkAccessDenyReason::NoCountryMatch => write!(f, "Network access denied, no country match"),
                NetworkAccessDenyReason::GeoIpUnavailable => write!(f, "Network access denied, GeoIP unavailable"),
                NetworkAccessDenyReason::CountryUnknown => write!(f, "Network access denied, country unknown"),
                NetworkAccessDenyReason::MalformedClientIp => write!(f, "Network access denied, malformed client IP"),
            },
        }
    }
}

impl ApiUserAuthError {
    /// Returns a player-endpoint compatible response with configured auth error status and empty body.
    /// This is used by proxy player endpoints (m3u, xtream, xmltv, etc.) where external players
    /// expect empty responses with HTTP status codes only.
    pub fn into_player_response(self, auth_error_status: axum::http::StatusCode) -> axum::response::Response {
        let status = match &self {
            ApiUserAuthError::AuthFailed => auth_error_status,
            ApiUserAuthError::PermissionDenied(_) | ApiUserAuthError::NetworkDenied(_) => {
                axum::http::StatusCode::FORBIDDEN
            }
        };
        status.into_response()
    }
}

#[derive(Debug)]
pub struct ApiUserContext {
    pub user: Arc<crate::model::ProxyUserCredentials>,
    pub target: Arc<crate::model::ConfigTarget>,
    pub fingerprint: crate::auth::Fingerprint,
}

/// Checks only network access (no permission check). Used by stream endpoints
/// where permission check must happen later with full stream info for `admission_failure_response`.
pub fn check_network_access_only(
    user: &Arc<crate::model::ProxyUserCredentials>,
    fingerprint: &crate::auth::Fingerprint,
    app_config: &Arc<AppConfig>,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
) -> Result<(), ApiUserAuthError> {
    let geoip_unavailable_policy = app_config.get_geoip_unavailable_policy();
    match evaluate_network_access(user, &fingerprint.client_ip, geoip, geoip_unavailable_policy) {
        NetworkAccessDecision::Allowed => Ok(()),
        NetworkAccessDecision::AllowedGeoIpUnavailable => {
            log_network_access_allowed_geoip_unavailable(&user.username, &fingerprint.client_ip);
            Ok(())
        }
        NetworkAccessDecision::Denied(reason) => {
            log_network_access_denied(&user.username, &fingerprint.client_ip, reason.as_str());
            Err(ApiUserAuthError::NetworkDenied(reason))
        }
    }
}

/// Checks network access without logging. Used for high-frequency polling endpoints
/// (e.g., `HDHomeRun` `lineup_status`) where repeated log output would be noisy.
pub fn try_check_network_access_only(
    user: &Arc<crate::model::ProxyUserCredentials>,
    fingerprint: &crate::auth::Fingerprint,
    app_config: &Arc<AppConfig>,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
) -> Result<(), ApiUserAuthError> {
    let geoip_unavailable_policy = app_config.get_geoip_unavailable_policy();
    match evaluate_network_access(user, &fingerprint.client_ip, geoip, geoip_unavailable_policy) {
        NetworkAccessDecision::Allowed | NetworkAccessDecision::AllowedGeoIpUnavailable => Ok(()),
        NetworkAccessDecision::Denied(reason) => Err(ApiUserAuthError::NetworkDenied(reason)),
    }
}

pub fn resolve_api_user_context(
    user: Arc<crate::model::ProxyUserCredentials>,
    target: Arc<crate::model::ConfigTarget>,
    fingerprint: crate::auth::Fingerprint,
    app_config: &Arc<AppConfig>,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
) -> Result<ApiUserContext, ApiUserAuthError> {
    // Permission check
    if let Some(reason) = user.permission_denied_reason(app_config) {
        debug!("User access denied for {}: {:?}", sanitize_sensitive_info(&user.username), reason);
        return Err(ApiUserAuthError::PermissionDenied(reason.into()));
    }

    // Network access check with policy
    let geoip_unavailable_policy = app_config.get_geoip_unavailable_policy();
    match evaluate_network_access(&user, &fingerprint.client_ip, geoip, geoip_unavailable_policy) {
        NetworkAccessDecision::Allowed => Ok(ApiUserContext { user, target, fingerprint }),
        NetworkAccessDecision::AllowedGeoIpUnavailable => {
            log_network_access_allowed_geoip_unavailable(&user.username, &fingerprint.client_ip);
            Ok(ApiUserContext { user, target, fingerprint })
        }
        NetworkAccessDecision::Denied(reason) => {
            log_network_access_denied(&user.username, &fingerprint.client_ip, reason.as_str());
            Err(ApiUserAuthError::NetworkDenied(reason))
        }
    }
}

/// Checks permission and network access without taking ownership. Used when
/// user/target are needed after the auth check (avoids unnecessary Arc clones).
pub fn check_permission_and_network_access_only(
    user: &Arc<crate::model::ProxyUserCredentials>,
    fingerprint: &crate::auth::Fingerprint,
    app_config: &Arc<AppConfig>,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
) -> Result<(), ApiUserAuthError> {
    // Permission check
    if let Some(reason) = user.permission_denied_reason(app_config) {
        debug!("User access denied for {}: {:?}", sanitize_sensitive_info(&user.username), reason);
        return Err(ApiUserAuthError::PermissionDenied(reason.into()));
    }

    // Network access check
    let geoip_unavailable_policy = app_config.get_geoip_unavailable_policy();
    match evaluate_network_access(user, &fingerprint.client_ip, geoip, geoip_unavailable_policy) {
        NetworkAccessDecision::Allowed => Ok(()),
        NetworkAccessDecision::AllowedGeoIpUnavailable => {
            log_network_access_allowed_geoip_unavailable(&user.username, &fingerprint.client_ip);
            Ok(())
        }
        NetworkAccessDecision::Denied(reason) => {
            log_network_access_denied(&user.username, &fingerprint.client_ip, reason.as_str());
            Err(ApiUserAuthError::NetworkDenied(reason))
        }
    }
}
