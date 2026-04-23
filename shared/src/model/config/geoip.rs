use crate::error::TuliproxError;
use enum_iterator::Sequence;
use std::fmt;

pub fn default_geoip_url() -> String {
    String::from(
        "https://raw.githubusercontent.com/sapics/ip-location-db/refs/heads/main/asn-country/asn-country-ipv4.csv",
    )
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, Sequence)]
#[serde(rename_all = "lowercase")]
pub enum GeoIpUnavailablePolicy {
    #[default]
    Deny,
    Allow,
}

impl fmt::Display for GeoIpUnavailablePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeoIpUnavailablePolicy::Deny => write!(f, "deny"),
            GeoIpUnavailablePolicy::Allow => write!(f, "allow"),
        }
    }
}

impl std::str::FromStr for GeoIpUnavailablePolicy {
    type Err = TuliproxError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "deny" => Ok(GeoIpUnavailablePolicy::Deny),
            "allow" => Ok(GeoIpUnavailablePolicy::Allow),
            _ => Err(TuliproxError::Config(format!("Unknown GeoIpUnavailablePolicy {s}"))),
        }
    }
}

pub const fn is_default_unavailable_policy(policy: &GeoIpUnavailablePolicy) -> bool {
    matches!(policy, GeoIpUnavailablePolicy::Deny)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GeoIpConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_geoip_url")]
    pub url: String,
    #[serde(default, skip_serializing_if = "is_default_unavailable_policy")]
    pub unavailable_policy: GeoIpUnavailablePolicy,
}

impl GeoIpConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.url.trim().is_empty() && self.unavailable_policy == GeoIpUnavailablePolicy::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geoip_unavailable_policy_defaults_to_deny() {
        let dto: GeoIpConfigDto = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert_eq!(dto.unavailable_policy, GeoIpUnavailablePolicy::Deny);
    }

    #[test]
    fn geoip_unavailable_policy_accepts_allow() {
        let dto: GeoIpConfigDto = serde_json::from_str(r#"{"enabled": false, "unavailable_policy": "allow"}"#).unwrap();
        assert_eq!(dto.unavailable_policy, GeoIpUnavailablePolicy::Allow);
    }

    #[test]
    fn geoip_unavailable_policy_accepts_deny() {
        let dto: GeoIpConfigDto = serde_json::from_str(r#"{"enabled": true, "unavailable_policy": "deny"}"#).unwrap();
        assert_eq!(dto.unavailable_policy, GeoIpUnavailablePolicy::Deny);
    }

    #[test]
    fn geoip_unavailable_policy_unknown_fails() {
        let result: Result<GeoIpConfigDto, _> =
            serde_json::from_str(r#"{"enabled": false, "unavailable_policy": "invalid"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn geoip_config_default_has_unavailable_policy_deny() {
        let dto = GeoIpConfigDto::default();
        assert_eq!(dto.unavailable_policy, GeoIpUnavailablePolicy::Deny);
    }

    #[test]
    fn geoip_config_with_allow_is_not_empty() {
        let dto: GeoIpConfigDto = serde_json::from_str(r#"{"enabled": false, "unavailable_policy": "allow"}"#).unwrap();
        assert!(!dto.is_empty(), "is_empty() should return false for explicit allow policy");
    }

    #[test]
    fn geoip_config_is_empty_respects_policy() {
        // URL default means we need an explicit empty string to test is_empty with deny policy
        let dto: GeoIpConfigDto =
            serde_json::from_str(r#"{"enabled": false, "url": "", "unavailable_policy": "deny"}"#).unwrap();
        assert!(dto.is_empty(), "disabled config with deny policy and empty url should be empty");
    }
}
