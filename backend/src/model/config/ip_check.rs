use std::sync::Arc;
use regex::Regex;
use shared::model::{IpCheckConfigDto};
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct IpCheckConfig {
    pub url: Option<String>,
    pub url_ipv4: Option<String>,
    pub url_ipv6: Option<String>,
    pub pattern_ipv4: Option<Arc<Regex>>,
    pub pattern_ipv6: Option<Arc<Regex>>,
}

macros::from_impl!(IpCheckConfig);
impl From<&IpCheckConfigDto> for IpCheckConfig {
    fn from(dto: &IpCheckConfigDto) -> Self {
        Self {
            url: dto.url.clone(),
            url_ipv4: dto.url_ipv4.clone(),
            url_ipv6: dto.url_ipv6.clone(),
            pattern_ipv4: dto.pattern_ipv4.as_ref().and_then(|s| {
                shared::model::REGEX_CACHE.get_or_compile(s)
                    .map_err(|e| log::warn!("Invalid pattern_ipv4 regex '{s}': {e}"))
                    .ok()
            }),
            pattern_ipv6: dto.pattern_ipv6.as_ref().and_then(|s| {
                shared::model::REGEX_CACHE.get_or_compile(s)
                    .map_err(|e| log::warn!("Invalid pattern_ipv6 regex '{s}': {e}"))
                    .ok()
            }),
        }
    }
}
impl From<&IpCheckConfig> for IpCheckConfigDto {
    fn from(dto: &IpCheckConfig) -> Self {
        Self {
            url: dto.url.clone(),
            url_ipv4: dto.url_ipv4.clone(),
            url_ipv6: dto.url_ipv6.clone(),
            pattern_ipv4: dto.pattern_ipv4.as_ref().map(std::string::ToString::to_string),
            pattern_ipv6:  dto.pattern_ipv6.as_ref().map(std::string::ToString::to_string),
        }
    }
}