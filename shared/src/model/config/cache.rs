use std::path::PathBuf;
use crate::error::{TuliproxError};
use crate::{info_err_res};
use crate::utils::{is_blank_optional_str, is_blank_optional_string, parse_size_base_2};
use path_clean::PathClean;


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub dir: Option<String>,
}

impl CacheConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled && is_blank_optional_str(self.size.as_deref()) && is_blank_optional_str(self.dir.as_deref())
    }

    pub(crate) fn prepare(&mut self, working_dir: &str) -> Result<(), TuliproxError> {
        if self.enabled {
            let work_path = PathBuf::from(working_dir);
            match self.dir.as_ref() {
                None => self.dir = Some(work_path.join("cache").to_string_lossy().to_string()),
                Some(work_dir) => {
                    let mut cache_dir = work_dir.to_string();
                    if PathBuf::from(&cache_dir).is_relative() {
                        cache_dir = work_path.join(&cache_dir).clean().to_string_lossy().to_string();
                    }
                    self.dir = Some(cache_dir.to_string());                }
            }

            if let Some(val) = self.size.as_ref() {
                match parse_size_base_2(val) {
                    Ok(size) => {
                        if let Err(err) = usize::try_from(size) {
                            return info_err_res!("Cache size could not be determined: {err}");
                        }
                    }
                    Err(err) => {
                        return info_err_res!("Failed to read cache size: {err}")
                    }
                }
            }
        }
        Ok(())
    }
}
