use crate::model::macros;
use shared::model::{ByteSize, CacheConfigDto};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    pub directory: String,
    pub size: usize,
    pub size_str: Option<ByteSize>,
}

macros::from_impl!(CacheConfig);
impl From<&CacheConfigDto> for CacheConfig {
    fn from(dto: &CacheConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            // Dto prepare should have set the right path
            directory: dto.directory.as_ref().map_or_else(Default::default, std::string::ToString::to_string),
            size_str: dto.size.clone(),
            size: get_size(dto),
        }
    }
}

impl From<&CacheConfig> for CacheConfigDto {
    fn from(instance: &CacheConfig) -> Self {
        Self {
            enabled: instance.enabled,
            // Dto prepare should have set the right path
            directory: Some(instance.directory.clone()),
            size: instance.size_str.clone(),
        }
    }
}

fn get_size(dto: &CacheConfigDto) -> usize {
    // we assume that the previous dto check discarded all problems
    match dto.size.as_ref() {
        None => return 1024,
        Some(val) => {
            if let Ok(size) = val.parse_bytes() {
                if let Ok(value) = usize::try_from(size.get()) {
                    return value;
                }
            }
        }
    }
    0
}
