use crate::{
    defaults::{
        default_as_true, default_mcp_max_response_bytes, is_default_mcp_max_response_bytes, is_false, is_true,
        MIN_MCP_MAX_RESPONSE_BYTES,
    },
    error::TuliproxError,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpConfigDto {
    /// Mount the MCP endpoint (`POST {web_ui_path}/mcp`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,

    /// Hide and reject tools that change state.
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub read_only: bool,

    /// Extra `Origin` values accepted besides the request `Host`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,

    /// Tool results larger than this are truncated.
    #[serde(default = "default_mcp_max_response_bytes", skip_serializing_if = "is_default_mcp_max_response_bytes")]
    pub max_response_bytes: usize,
}

impl Default for McpConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            read_only: true,
            allowed_origins: Vec::new(),
            max_response_bytes: default_mcp_max_response_bytes(),
        }
    }
}

impl McpConfigDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.allowed_origins = self
            .allowed_origins
            .iter()
            .map(|origin| origin.trim().trim_end_matches('/'))
            .filter(|origin| !origin.is_empty())
            .map(str::to_owned)
            .collect();
        if self.max_response_bytes < MIN_MCP_MAX_RESPONSE_BYTES {
            return Err(TuliproxError::ConfigBase(format!(
                "`mcp.max_response_bytes` must be >= {MIN_MCP_MAX_RESPONSE_BYTES}, got {}",
                self.max_response_bytes
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disabled_read_only() {
        let dto: McpConfigDto = serde_json::from_str("{}").unwrap();
        assert_eq!(dto, McpConfigDto::default());
        assert!(!dto.enabled);
        assert!(dto.read_only);
        assert_eq!(dto.allowed_origins, Vec::<String>::new());
        assert_eq!(dto.max_response_bytes, 262_144);
    }

    #[test]
    fn default_block_serializes_empty() {
        assert_eq!(serde_json::to_string(&McpConfigDto::default()).unwrap(), "{}");
    }

    #[test]
    fn round_trips_non_default_values() {
        let dto = McpConfigDto {
            enabled: true,
            read_only: false,
            allowed_origins: vec!["https://claude.ai".to_string()],
            max_response_bytes: 4096,
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert_eq!(serde_json::from_str::<McpConfigDto>(&json).unwrap(), dto);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(serde_json::from_str::<McpConfigDto>(r#"{"enabld": true}"#).is_err());
    }

    #[test]
    fn prepare_normalizes_origins_and_rejects_tiny_limit() {
        let mut dto = McpConfigDto {
            allowed_origins: vec![" https://a.example/ ".to_string(), "  ".to_string()],
            ..McpConfigDto::default()
        };
        dto.prepare().unwrap();
        assert_eq!(dto.allowed_origins, vec!["https://a.example".to_string()]);

        dto.max_response_bytes = 10;
        assert!(dto.prepare().is_err());
    }
}
