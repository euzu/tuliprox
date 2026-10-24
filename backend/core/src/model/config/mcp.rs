use crate::model::macros;
use shared::model::McpConfigDto;

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub enabled: bool,
    pub read_only: bool,
    pub allowed_origins: Vec<String>,
    pub max_response_bytes: usize,
}

macros::from_impl!(McpConfig);

impl From<&McpConfigDto> for McpConfig {
    fn from(dto: &McpConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            read_only: dto.read_only,
            allowed_origins: dto.allowed_origins.clone(),
            max_response_bytes: dto.max_response_bytes,
        }
    }
}

impl From<&McpConfig> for McpConfigDto {
    fn from(config: &McpConfig) -> Self {
        Self {
            enabled: config.enabled,
            read_only: config.read_only,
            allowed_origins: config.allowed_origins.clone(),
            max_response_bytes: config.max_response_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_round_trips_through_runtime_config() {
        let dto = McpConfigDto {
            enabled: true,
            read_only: false,
            allowed_origins: vec!["https://claude.ai".to_string()],
            max_response_bytes: 4096,
        };
        let config = McpConfig::from(&dto);
        assert_eq!(McpConfigDto::from(&config), dto);
    }

    #[test]
    fn default_dto_maps_to_disabled_read_only() {
        let config = McpConfig::from(&McpConfigDto::default());
        assert!(!config.enabled);
        assert!(config.read_only);
        assert_eq!(config.max_response_bytes, 262_144);
    }
}
