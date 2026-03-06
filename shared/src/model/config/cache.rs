use crate::{
    error::TuliproxError,
    info_err_res,
    utils::{
        is_blank_optional_str, is_blank_optional_string, is_blank_or_default_cache_dir, parse_size_base_2,
        DEFAULT_CACHE_DIR,
    },
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub size: Option<String>,
    #[serde(default, alias = "dir", skip_serializing_if = "is_blank_or_default_cache_dir")]
    pub directory: Option<String>,
}

impl CacheConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled && is_blank_optional_str(self.size.as_deref()) && is_blank_optional_str(self.directory.as_deref())
    }

    pub(crate) fn prepare(&mut self, _storage_dir: &str) -> Result<(), TuliproxError> {
        if self.enabled {
            if is_blank_or_default_cache_dir(&self.directory) {
                self.directory = Some(DEFAULT_CACHE_DIR.to_string());
            } else if let Some(dir) = self.directory.as_ref() {
                self.directory = Some(dir.trim().to_string());
            }

            if let Some(val) = self.size.as_ref() {
                match parse_size_base_2(val) {
                    Ok(size) => {
                        if let Err(err) = usize::try_from(size) {
                            return info_err_res!("Cache size could not be determined: {err}");
                        }
                    }
                    Err(err) => return info_err_res!("Failed to read cache size: {err}"),
                }
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
        assert!(!serialized.contains("\"dir\""), "expected no dir field for default value, got: {serialized}");
    }

    #[test]
    fn serializing_keeps_non_default_cache_dir() {
        let cache = CacheConfigDto { enabled: true, directory: Some("custom-cache".to_string()), size: None };
        let serialized = serde_json::to_string(&cache).expect("cache serialization should succeed");
        assert!(serialized.contains("\"dir\""), "expected dir field for custom value, got: {serialized}");
    }
}
