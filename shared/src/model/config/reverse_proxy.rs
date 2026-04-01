use crate::{
    error::{TuliproxError, TuliproxErrorKind},
    info_err_res,
    model::{CacheConfigDto, GeoIpConfigDto, RateLimitConfigDto, StreamConfigDto, StreamHistoryConfigDto},
    utils::{
        default_resource_retry_attempts, default_resource_retry_backoff_ms, default_resource_retry_backoff_multiplier,
        hex_to_u8_16, is_default_resource_retry_attempts, is_default_resource_retry_backoff_ms,
        is_default_resource_retry_backoff_multiplier, is_empty_optional_vec, is_false,
    },
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

    pub fn clean(&mut self) { self.custom_header.retain(|h| !h.trim().is_empty()); }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_history: Option<StreamHistoryConfigDto>,
}

impl ReverseProxyConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.resource_rewrite_disabled
            && self.disabled_header.as_ref().is_none_or(|d| d.is_empty())
            && self.resource_retry.as_ref().is_none_or(ResourceRetryConfigDto::is_default)
            && (self.stream.is_none() || self.stream.as_ref().is_some_and(|s| s.is_empty()))
            && (self.cache.is_none() || self.cache.as_ref().is_some_and(|c| c.is_empty()))
            && (self.rate_limit.is_none() || self.rate_limit.as_ref().is_some_and(|r| r.is_empty()))
            && (self.geoip.is_none() || self.geoip.as_ref().is_some_and(|g| g.is_empty()))
            && (self.stream_history.is_none() || self.stream_history.as_ref().is_some_and(|s| s.is_empty()))
    }

    pub fn clean(&mut self) {
        if let Some(disabled) = self.disabled_header.as_mut() {
            disabled.clean();
            if disabled.is_empty() {
                self.disabled_header = None;
            }
        }
        if self.resource_retry.as_ref().is_some_and(ResourceRetryConfigDto::is_default) {
            self.resource_retry = None;
        }
        if self.stream.as_ref().is_some_and(StreamConfigDto::is_empty) {
            self.stream = None;
        }
        if self.cache.as_ref().is_some_and(CacheConfigDto::is_empty) {
            self.cache = None;
        }
        if self.rate_limit.as_ref().is_some_and(RateLimitConfigDto::is_empty) {
            self.rate_limit = None;
        }
        if self.geoip.as_ref().is_some_and(GeoIpConfigDto::is_empty) {
            self.geoip = None;
        }
        if self.stream_history.as_ref().is_some_and(StreamHistoryConfigDto::is_empty) {
            self.stream_history = None;
        }
    }

    pub(crate) fn prepare(&mut self, storage_dir: &str) -> Result<(), TuliproxError> {
        self.rewrite_secret = self.rewrite_secret.trim().to_string();
        if !self.resource_rewrite_disabled {
            if self.rewrite_secret.is_empty() {
                return info_err_res!("rewrite_secret is required when resource rewrite is enabled");
            }
            hex_to_u8_16(&self.rewrite_secret).map_err(|e| TuliproxError::new(TuliproxErrorKind::Info, e))?;
        }

        if let Some(stream) = self.stream.as_mut() {
            stream.prepare()?;
        }
        if let Some(cache) = self.cache.as_mut() {
            if cache.enabled && self.resource_rewrite_disabled {
                warn!("The cache is disabled because resource rewrite is disabled");
                cache.enabled = false;
            }
            cache.prepare(storage_dir)?;
        }

        if let Some(rate_limit) = self.rate_limit.as_mut() {
            if rate_limit.enabled {
                rate_limit.prepare()?;
            }
        }

        if let Some(stream_history) = self.stream_history.as_mut() {
            stream_history.prepare(storage_dir)?;
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
    #[serde(default = "default_resource_retry_attempts", skip_serializing_if = "is_default_resource_retry_attempts")]
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
            && (self.backoff_multiplier - default_resource_retry_backoff_multiplier()).abs() < f64::EPSILON
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

#[cfg(test)]
mod tests {
    use super::ReverseProxyConfigDto;
    use crate::model::StreamHistoryConfigDto;

    #[test]
    fn serializing_stream_history_under_reverse_proxy_uses_nested_yaml_shape() {
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 14,
                stream_history_directory: "/var/lib/tuliprox/history".to_string(),
            }),
            ..Default::default()
        };

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(serialized.contains("stream_history:"), "expected nested stream_history block, got: {serialized}");
    }

    #[test]
    fn prepare_uses_default_directory_when_stream_history_directory_is_blank() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 14,
                stream_history_directory: String::new(),
            }),
            ..Default::default()
        };

        cfg.prepare("storage").expect("prepare should succeed with blank directory");
        let sh = cfg.stream_history.as_ref().unwrap();
        // Blank directory must resolve to an absolute path ending with the default subdir name.
        assert!(
            sh.stream_history_directory.ends_with("stream_history"),
            "expected default subdir 'stream_history', got: {}",
            sh.stream_history_directory
        );
        assert!(
            std::path::Path::new(&sh.stream_history_directory).is_absolute(),
            "expected absolute path, got: {}",
            sh.stream_history_directory
        );
    }

    #[test]
    fn prepare_normalizes_relative_stream_history_directory_against_storage_dir() {
        use std::path::{Path, PathBuf};

        let storage_dir = if cfg!(windows) { r"C:\data\tuliprox" } else { "/var/lib/tuliprox" };
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 14,
                stream_history_directory: "history".to_string(),
            }),
            ..Default::default()
        };

        cfg.prepare(storage_dir).expect("prepare should succeed");

        let stream_history = cfg.stream_history.expect("stream history should exist");
        let expected = PathBuf::from(storage_dir).join("history");
        let actual = Path::new(&stream_history.stream_history_directory);
        assert_eq!(actual.file_name(), expected.file_name());
        assert!(actual.ends_with("history"), "directory should end with 'history', got: {actual:?}");
    }
}
