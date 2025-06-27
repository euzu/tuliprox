use shared::model::ReverseProxyConfigDto;
use crate::model::config::cache::CacheConfig;
use crate::model::{macros, RateLimitConfig, StreamConfig};

#[derive(Debug, Clone)]
pub struct ReverseProxyConfig {
    pub resource_rewrite_disabled: bool,
    pub disable_referer_header: bool,
    pub stream: Option<StreamConfig>,
    pub cache: Option<CacheConfig>,
    pub rate_limit: Option<RateLimitConfig>,
}

macros::from_impl!(ReverseProxyConfig);

impl From<&ReverseProxyConfigDto> for ReverseProxyConfig {
    fn from(dto: &ReverseProxyConfigDto) -> Self {
        Self {
            resource_rewrite_disabled: dto.resource_rewrite_disabled,
            disable_referer_header: dto.disable_referer_header,
            stream: dto.stream.as_ref().map(Into::into),
            cache: dto.cache.as_ref().map(Into::into),
            rate_limit: dto.rate_limit.as_ref().map(Into::into),
        }
    }
}

impl From<&ReverseProxyConfig> for ReverseProxyConfigDto {
    fn from(instance: &ReverseProxyConfig) -> Self {
        Self {
            resource_rewrite_disabled: instance.resource_rewrite_disabled,
            disable_referer_header: instance.disable_referer_header,
            stream: instance.stream.as_ref().map(Into::into),
            cache: instance.cache.as_ref().map(Into::into),
            rate_limit: instance.rate_limit.as_ref().map(Into::into),
        }
    }
}
