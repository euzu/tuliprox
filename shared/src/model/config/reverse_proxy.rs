use log::warn;
use crate::error::TuliproxError;
use crate::model::{CacheConfigDto, RateLimitConfigDto, StreamConfigDto};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReverseProxyConfigDto {
    #[serde(default)]
    pub resource_rewrite_disabled: bool,
    #[serde(default)]
    pub disable_referer_header: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
}

impl ReverseProxyConfigDto {
    pub(crate) fn prepare(&mut self, working_dir: &str) -> Result<(), TuliproxError> {
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
        Ok(())
    }
}
