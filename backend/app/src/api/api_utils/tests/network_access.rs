use super::{
    evaluate_network_access, mock_geoip, user_with_network_access, NetworkAccessDecision, NetworkAccessDenyReason,
};
use crate::{model::NetworkAccess, repository::GeoIp};
use arc_swap::ArcSwapOption;
use shared::model::GeoIpUnavailablePolicy;
use std::sync::Arc;

// =========================================================================================
// evaluate_network_access tests
// =========================================================================================

/// Run `evaluate_network_access` for a synthetic user built from
/// `network_access` and assert the decision matches `expected`. Centralizes
/// the boilerplate (`user_with_network_access` + geoip setup + call +
/// assert) shared by every `evaluate_network_access` test below.
pub(in crate::api::api_utils::tests) fn assert_network_decision(
    network_access: Option<NetworkAccess>,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
    ip: &str,
    expected: NetworkAccessDecision,
) {
    let user = user_with_network_access(network_access);
    assert_eq!(evaluate_network_access(&user, ip, geoip, GeoIpUnavailablePolicy::Deny), expected);
}

/// `Arc<ArcSwapOption<GeoIp>>` with no `GeoIP` database loaded.
pub(in crate::api::api_utils::tests) fn empty_geoip() -> Arc<ArcSwapOption<GeoIp>> {
    Arc::new(ArcSwapOption::<GeoIp>::default())
}

#[test]
fn no_config_allows_all() {
    assert_network_decision(None, &empty_geoip(), "192.168.1.1", NetworkAccessDecision::Allowed);
}

#[test]
fn empty_config_allows_all() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec![] }),
        &empty_geoip(),
        "192.168.1.1",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn cidr_match_allows() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["192.168.1.0/24".parse().unwrap()] }),
        &empty_geoip(),
        "192.168.1.42",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn cidr_miss_denies() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["192.168.1.0/24".parse().unwrap()] }),
        &empty_geoip(),
        "10.0.0.1",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
    );
}

#[test]
fn country_match_allows() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
        &mock_geoip("DE"),
        "8.8.8.8",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn country_miss_denies() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
        &mock_geoip("US"),
        "8.8.8.8",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch),
    );
}

#[test]
fn no_geoip_denies_on_country_restriction() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
        &empty_geoip(),
        "8.8.8.8",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable),
    );
}

#[test]
fn ipv4_vs_ipv6_denies_gracefully() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["2001:db8::/32".parse().unwrap()] }),
        &empty_geoip(),
        "192.168.1.1",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
    );
}

#[test]
fn ipv6_vs_ipv4_denies_gracefully() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["192.168.1.0/24".parse().unwrap()] }),
        &empty_geoip(),
        "2001:db8::1",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
    );
}

#[test]
fn either_cidr_or_country_match_allows() {
    assert_network_decision(
        Some(NetworkAccess {
            allowed_countries: vec!["US".to_string()],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }),
        &mock_geoip("DE"),
        "192.168.1.42",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn single_ip_cidr() {
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec![],
        allowed_networks: vec!["192.168.1.1/32".parse().unwrap()],
    }));
    let geoip = empty_geoip();
    assert_eq!(
        evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Allowed
    );
    assert_eq!(
        evaluate_network_access(&user, "192.168.1.2", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
    );
}

// =========================================================================================
// network denied reason tests
// =========================================================================================

// The three `network_denied_reason_*` cases remain because they cover the
// `NetworkAccessDenyReason`-focused API surface directly, while
// `cidr_miss_denies`, `country_miss_denies`, and
// `no_geoip_denies_on_country_restriction` above assert the same deny
// reasons through broader `evaluate_network_access(...)` behavior. The
// overlap is intentional so both the general decision path and the
// reason-reporting-focused path stay pinned by tests.

#[test]
fn network_denied_reason_country_unknown_when_geoip_loaded_but_unknown_ip() {
    // GeoIP is loaded (not None), but lookup returns None for this IP (private/unknown).
    // We need a GeoIP that only covers a private range, so public IPs get None.
    // Use a CIDR-only restriction (no country rules) so we can verify
    // that when countries ARE checked, lookup None gives "country_unknown".
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec!["192.168.1.0/24".parse().unwrap()], // miss CIDR first
    }));
    // Use the real GeoIp::new() which only seeds private ranges.
    // For 8.8.8.8 (public), lookup returns None.
    let geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::new()))));
    // CIDR miss -> country check -> geoip loaded but lookup returns None
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown)
    );
}

#[test]
fn network_denied_reason_none_when_no_config() {
    let user = user_with_network_access(None);
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    assert_eq!(
        evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Allowed
    );
}

// =========================================================================================
// GeoIP unavailable policy tests
// =========================================================================================

#[test]
fn geoip_unavailable_default_deny_denies() {
    // Country rule exists but GeoIP is unavailable — default policy is Deny
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable));
}

#[test]
fn geoip_unavailable_explicit_allow_allows() {
    // Country rule exists, GeoIP unavailable, but policy is Allow — allows
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::AllowedGeoIpUnavailable);
}

#[test]
fn geoip_unavailable_cidr_only_still_denies() {
    // CIDR only rules, no match — should deny even with Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec![],
        allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch));
}

#[test]
fn geoip_unavailable_cidr_match_allows() {
    // CIDR match always allows, regardless of policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec!["10.0.0.0/8".parse().unwrap()],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Deny);
    assert_eq!(decision, NetworkAccessDecision::Allowed);
}

#[test]
fn geoip_loaded_country_mismatch_still_denies() {
    // Loaded GeoIP but country doesn't match — should deny under Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("US")))));
    let decision = evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch));
}

#[test]
fn geoip_loaded_unknown_country_still_denies() {
    // Loaded GeoIP but lookup returns None — should deny under Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
    }));
    let geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::new()))));
    let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown));
}

#[test]
fn malformed_ip_denies_even_when_geoip_unavailable_policy_is_allow() {
    let user = user_with_network_access(Some(NetworkAccess::from(&shared::model::NetworkAccessDto {
        allowed_countries: Some(vec!["DE".to_string()]),
        allowed_networks: None,
    })));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());

    let decision = evaluate_network_access(&user, "not-an-ip", &geoip, GeoIpUnavailablePolicy::Allow);

    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::MalformedClientIp));
}

#[test]
fn evaluate_network_access_respects_allow_policy() {
    // verify evaluate_network_access returns AllowedGeoIpUnavailable with Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    // Default deny policy should return Denied
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable)
    );
    // Allow policy should return AllowedGeoIpUnavailable
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow),
        NetworkAccessDecision::AllowedGeoIpUnavailable
    );
}
