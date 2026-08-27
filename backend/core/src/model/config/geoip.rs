use crate::model::macros;
use shared::model::{GeoIpConfigDto, GeoIpUnavailablePolicy};

#[derive(Debug, Clone)]
pub struct GeoIpConfig {
    pub enabled: bool,
    pub url: String,
    pub unavailable_policy: GeoIpUnavailablePolicy,
}

macros::from_impl!(GeoIpConfig);

impl From<&GeoIpConfigDto> for GeoIpConfig {
    fn from(dto: &GeoIpConfigDto) -> Self {
        Self { enabled: dto.enabled, url: dto.url.clone(), unavailable_policy: dto.unavailable_policy }
    }
}

impl From<&GeoIpConfig> for GeoIpConfigDto {
    fn from(instance: &GeoIpConfig) -> Self {
        Self { enabled: instance.enabled, url: instance.url.clone(), unavailable_policy: instance.unavailable_policy }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geoip_unavailable_policy_dto_default_converts_to_deny() {
        let dto =
            GeoIpConfigDto { enabled: false, url: String::new(), unavailable_policy: GeoIpUnavailablePolicy::Deny };
        let config: GeoIpConfig = (&dto).into();
        assert_eq!(config.unavailable_policy, GeoIpUnavailablePolicy::Deny);
    }

    #[test]
    fn geoip_unavailable_policy_dto_allow_converts_to_allow() {
        let dto = GeoIpConfigDto {
            enabled: true,
            url: "https://example.com/db.csv".to_string(),
            unavailable_policy: GeoIpUnavailablePolicy::Allow,
        };
        let config: GeoIpConfig = (&dto).into();
        assert_eq!(config.unavailable_policy, GeoIpUnavailablePolicy::Allow);
    }

    #[test]
    fn geoip_unavailable_policy_domain_allow_converts_to_dto_allow() {
        let config = GeoIpConfig {
            enabled: true,
            url: "https://example.com/db.csv".to_string(),
            unavailable_policy: GeoIpUnavailablePolicy::Allow,
        };
        let dto: GeoIpConfigDto = (&config).into();
        assert_eq!(dto.unavailable_policy, GeoIpUnavailablePolicy::Allow);
    }
}
