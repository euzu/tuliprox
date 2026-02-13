use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::info_err_res;
use crate::model::{CacheConfigDto, GeoIpConfigDto, RateLimitConfigDto, StreamConfigDto};
use crate::utils::{
    default_resource_retry_attempts, default_resource_retry_backoff_ms,
    default_resource_retry_backoff_multiplier, hex_to_u8_16, is_default_resource_retry_attempts,
    is_default_resource_retry_backoff_ms, is_default_resource_retry_backoff_multiplier,
    is_empty_optional_vec, is_false,
};
use log::warn;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReverseProxyDisabledHeaderConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub referer_header: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub x_header: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cloudflare_header: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_header: Vec<String>,
}

impl ReverseProxyDisabledHeaderConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.referer_header
            && !self.x_header
            && !self.cloudflare_header
            && self.custom_header.iter().all(|h| h.trim().is_empty())
    }

    pub fn clean(&mut self) {
        self.custom_header.retain(|h| !h.trim().is_empty());
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReverseProxyConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub resource_rewrite_disabled: bool,
    pub rewrite_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_retry: Option<ResourceRetryConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_header: Option<ReverseProxyDisabledHeaderConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoip: Option<GeoIpConfigDto>,
}

impl ReverseProxyConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.resource_rewrite_disabled
            && self.disabled_header.as_ref().is_none_or(|d| d.is_empty())
            && self
                .resource_retry
                .as_ref()
                .is_none_or(ResourceRetryConfigDto::is_default)
            && (self.stream.is_none() || self.stream.as_ref().is_some_and(|s| s.is_empty()))
            && (self.cache.is_none() || self.cache.as_ref().is_some_and(|c| c.is_empty()))
            && (self.rate_limit.is_none() || self.rate_limit.as_ref().is_some_and(|r| r.is_empty()))
            && (self.geoip.is_none() || self.geoip.as_ref().is_some_and(|g| g.is_empty()))
    }

    pub fn clean(&mut self) {
        if let Some(disabled) = self.disabled_header.as_mut() {
            disabled.clean();
            if disabled.is_empty() {
                self.disabled_header = None;
            }
        }
        if self
            .resource_retry
            .as_ref()
            .is_some_and(ResourceRetryConfigDto::is_default)
        {
            self.resource_retry = None;
        }
        if self.stream.as_ref().is_some_and(StreamConfigDto::is_empty) {
            self.stream = None;
        }
        if self.cache.as_ref().is_some_and(CacheConfigDto::is_empty) {
            self.cache = None;
        }
        if self
            .rate_limit
            .as_ref()
            .is_some_and(RateLimitConfigDto::is_empty)
        {
            self.rate_limit = None;
        }
        if self.geoip.as_ref().is_some_and(GeoIpConfigDto::is_empty) {
            self.geoip = None;
        }
    }

    pub(crate) fn prepare(&mut self, working_dir: &str) -> Result<(), TuliproxError> {
        hex_to_u8_16(&self.rewrite_secret)
            .map_err(|e| TuliproxError::new(TuliproxErrorKind::Info, e))?;

        if let Some(stream) = self.stream.as_mut() {
            stream.prepare()?;
        }
        if let Some(cache) = self.cache.as_mut() {
            if cache.enabled && self.resource_rewrite_disabled {
                warn!("The cache is disabled because resource rewrite is disabled");
                cache.enabled = false;
            }
            cache.prepare(working_dir)?;
        }

        if let Some(rate_limit) = self.rate_limit.as_mut() {
            if rate_limit.enabled {
                rate_limit.prepare()?;
            }
        }

        if let Some(resource_retry) = self.resource_retry.as_mut() {
            resource_retry.prepare()?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRetryConfigDto {
    #[serde(
        default = "default_resource_retry_attempts",
        skip_serializing_if = "is_default_resource_retry_attempts"
    )]
    pub max_attempts: u32,
    #[serde(
        default = "default_resource_retry_backoff_ms",
        skip_serializing_if = "is_default_resource_retry_backoff_ms"
    )]
    pub backoff_millis: u64,
    #[serde(
        default = "default_resource_retry_backoff_multiplier",
        skip_serializing_if = "is_default_resource_retry_backoff_multiplier"
    )]
    pub backoff_multiplier: f64,
    #[serde(default, skip_serializing_if = "is_empty_optional_vec")]
    pub failover_redirect_patterns: Option<Vec<String>>,
}

impl Default for ResourceRetryConfigDto {
    fn default() -> Self {
        Self {
            max_attempts: default_resource_retry_attempts(),
            backoff_millis: default_resource_retry_backoff_ms(),
            backoff_multiplier: default_resource_retry_backoff_multiplier(),
            failover_redirect_patterns: None,
        }
    }
}

impl ResourceRetryConfigDto {
    pub fn is_default(&self) -> bool {
        self.max_attempts == default_resource_retry_attempts()
            && self.backoff_millis == default_resource_retry_backoff_ms()
            && (self.backoff_multiplier - default_resource_retry_backoff_multiplier()).abs()
                < f64::EPSILON
            && is_empty_optional_vec(&self.failover_redirect_patterns)
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(failover_redirect_patterns) = self.failover_redirect_patterns.as_mut() {
            for pattern in failover_redirect_patterns {
                if let Err(err) = crate::model::REGEX_CACHE.get_or_compile(pattern) {
                    return info_err_res!("Can't parse regex: {pattern} {err}");
                }
            }
        }
        Ok(())
    }
}
