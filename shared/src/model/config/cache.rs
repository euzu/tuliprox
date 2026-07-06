use crate::{
    error::TuliproxError,
    model::ByteSize,
    utils::{is_blank_optional_str, is_blank_or_default_cache_dir, DEFAULT_CACHE_DIR},
};

fn is_blank_optional_byte_size(value: &Option<ByteSize>) -> bool {
    value.as_ref().is_none_or(|v| v.as_str().trim().is_empty())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_byte_size")]
    pub size: Option<ByteSize>,
    #[serde(default, alias = "dir", skip_serializing_if = "is_blank_or_default_cache_dir")]
    pub directory: Option<String>,
}

impl CacheConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled && is_blank_optional_byte_size(&self.size) && is_blank_optional_str(self.directory.as_deref())
    }

    pub(crate) fn prepare(&mut self, _storage_dir: &str) -> Result<(), TuliproxError> {
        if self.enabled {
            if is_blank_or_default_cache_dir(&self.directory) {
                self.directory = Some(DEFAULT_CACHE_DIR.to_string());
            } else if let Some(dir) = self.directory.as_ref() {
                self.directory = Some(dir.trim().to_string());
            }

            if let Some(val) = self.size.as_ref() {
                val.parse_bytes()
                    .map_err(|err| TuliproxError::ConfigCache(format!("Failed to read cache size: {err}")))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_sets_default_cache_dir_when_enabled_and_missing() {
        let mut cache = CacheConfigDto { enabled: true, directory: None, size: None };
        cache.prepare("storage").expect("prepare should succeed");
        assert_eq!(cache.directory.as_deref(), Some(DEFAULT_CACHE_DIR));
    }

    #[test]
    fn prepare_keeps_custom_cache_dir_when_enabled() {
        let mut cache = CacheConfigDto { enabled: true, directory: Some("custom-cache".to_string()), size: None };
        cache.prepare("storage").expect("prepare should succeed");
        assert_eq!(cache.directory.as_deref(), Some("custom-cache"));
    }

    #[test]
    fn serializing_skips_default_cache_dir() {
        let cache = CacheConfigDto { enabled: true, directory: Some(DEFAULT_CACHE_DIR.to_string()), size: None };
        let serialized = serde_json::to_string(&cache).expect("cache serialization should succeed");
        assert!(!serialized.contains("\"directory\""), "expected no dir field for default value, got: {serialized}");
    }

    #[test]
    fn serializing_keeps_non_default_cache_dir() {
        let cache = CacheConfigDto { enabled: true, directory: Some("custom-cache".to_string()), size: None };
        let serialized = serde_json::to_string(&cache).expect("cache serialization should succeed");
        assert!(serialized.contains("\"directory\""), "expected dir field for custom value, got: {serialized}");
    }
}
