use shared::model::CacheConfigDto;
use shared::utils::parse_size_base_2;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    pub dir: String,
    pub size: usize,
    pub size_str: Option<String>,
}

macros::from_impl!(CacheConfig);
impl From<&CacheConfigDto> for CacheConfig {
    fn from(dto: &CacheConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            // Dto prepare should have set the right path
            dir: dto.dir.as_ref().map_or_else(Default::default, std::string::ToString::to_string),
            size_str: dto.size.clone(),
            size: get_size(dto)
        }
    }
}

impl From<&CacheConfig> for CacheConfigDto {
    fn from(instance: &CacheConfig) -> Self {
        Self {
            enabled: instance.enabled,
            // Dto prepare should have set the right path
            dir: Some(instance.dir.to_string()),
            size: instance.size_str.clone(),
        }
    }
}

fn get_size(dto: &CacheConfigDto) -> usize {
    // we assume that the previous dto check discarded all problems
    match dto.size.as_ref() {
        None => return 1024,
        Some(val) => {
            if let Ok(size) = parse_size_base_2(val) {
                if let Ok(value) = usize::try_from(size) {
                    return value;
                }
            }
        }
    }
    0
}
