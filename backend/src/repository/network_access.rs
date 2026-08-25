//! Policy-aware network access evaluation.
//!
//! Whether a user may reach the service from a given address is a policy
//! decision over their configured CIDRs and countries - not an HTTP concern.
//! It lived in `api::api_utils`, which is what made `auth` reach up into `api`.
//! It sits here rather than in `model` because it queries the `GeoIp` store, and
//! `model` must not depend on this layer.

use super::GeoIp;
use crate::model::ProxyUserCredentials;
use arc_swap::ArcSwapOption;
use log::warn;
use shared::model::GeoIpUnavailablePolicy;
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;

/// Result of a policy-aware network access evaluation.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NetworkAccessDecision {
    /// Request is allowed (matched CIDR or country with `GeoIP` available).
    Allowed,
    /// Request is allowed because `GeoIP` is unavailable and policy is Allow.
    AllowedGeoIpUnavailable,
    /// Request is denied with a typed reason.
    Denied(NetworkAccessDenyReason),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NetworkAccessDenyReason {
    NoCidrMatch,
    NoCountryMatch,
    GeoIpUnavailable,
    CountryUnknown,
    MalformedClientIp,
}

impl NetworkAccessDenyReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCidrMatch => "no_cidr_match",
            Self::NoCountryMatch => "no_country_match",
            Self::GeoIpUnavailable => "geoip_unavailable",
            Self::CountryUnknown => "country_unknown",
            Self::MalformedClientIp => "malformed_client_ip",
        }
    }
}

/// Logs a network access denial with structured context for operator debugging.
/// Do NOT log passwords or secrets.
#[allow(clippy::uninlined_format_args)]
pub fn log_network_access_denied(username: &str, client_ip: &str, reason: &str) {
    let sanitized_username = sanitize_sensitive_info(username);
    let sanitized_client_ip = sanitize_sensitive_info(client_ip);
    warn!(
        target: "network_access",
        "Network access denied: user=\"{}\" client_ip=\"{}\" reason={}",
        sanitized_username,
        sanitized_client_ip,
        reason
    );
}

/// Logs a network access allowed-without-GeoIP event for explicit-risk observability.
#[allow(clippy::uninlined_format_args)]
pub fn log_network_access_allowed_geoip_unavailable(username: &str, client_ip: &str) {
    warn!(
        target: "network_access",
        "Network access allowed because GeoIP is unavailable and reverse_proxy.geoip.unavailable_policy=allow; user=\"{}\" client_ip=\"{}\"",
        sanitize_sensitive_info(username),
        sanitize_sensitive_info(client_ip)
    );
}

/// Evaluates network access with the configured GeoIP-unavailable policy.
/// Returns a structured decision for logging and HTTP response mapping.
pub fn evaluate_network_access(
    user: &ProxyUserCredentials,
    client_ip: &str,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
    geoip_unavailable_policy: GeoIpUnavailablePolicy,
) -> NetworkAccessDecision {
    let Some(access) = user.network_access.as_ref() else {
        return NetworkAccessDecision::Allowed;
    };
    if access.is_empty() {
        return NetworkAccessDecision::Allowed;
    }

    let Ok(parsed_ip) = client_ip.parse::<std::net::IpAddr>() else {
        return NetworkAccessDecision::Denied(NetworkAccessDenyReason::MalformedClientIp);
    };

    // CIDR check
    for net in &access.allowed_networks {
        if net.contains(&parsed_ip) {
            return NetworkAccessDecision::Allowed;
        }
    }

    // No CIDR match — check country rules
    if access.allowed_countries.is_empty() {
        return NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch);
    }

    // Country rules exist — check if GeoIP is available
    let geoip_guard = geoip.load();
    let Some(geoip_db) = geoip_guard.as_ref() else {
        // GeoIP unavailable — apply policy
        return match geoip_unavailable_policy {
            GeoIpUnavailablePolicy::Allow => NetworkAccessDecision::AllowedGeoIpUnavailable,
            GeoIpUnavailablePolicy::Deny => NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable),
        };
    };

    // GeoIP is loaded — do country lookup
    match geoip_db.lookup(client_ip) {
        Some(country) => {
            if access.allowed_countries.iter().any(|c| c == &country) {
                NetworkAccessDecision::Allowed
            } else {
                NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch)
            }
        }
        None => NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown),
    }
}
