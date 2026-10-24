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
        let mut allowed_origins = Vec::with_capacity(self.allowed_origins.len());
        for origin in &self.allowed_origins {
            let origin = origin.trim().trim_end_matches('/');
            if origin.is_empty() {
                continue;
            }
            let Some(canonical) = canonical_origin(origin) else {
                return Err(TuliproxError::ConfigBase(format!(
                    "`mcp.allowed_origins` entry `{origin}` is not an origin; expected `scheme://host[:port]` without \
                     path, query or fragment"
                )));
            };
            allowed_origins.push(canonical);
        }
        self.allowed_origins = allowed_origins;
        if self.max_response_bytes < MIN_MCP_MAX_RESPONSE_BYTES {
            return Err(TuliproxError::ConfigBase(format!(
                "`mcp.max_response_bytes` must be >= {MIN_MCP_MAX_RESPONSE_BYTES}, got {}",
                self.max_response_bytes
            )));
        }
        Ok(())
    }
}

/// The origin as a browser sends it in the `Origin` header, or `None` when `value` is not a `scheme://host[:port]`
/// origin. The default port is omitted and the host lowercased for `http(s)`/`ws(s)`; a custom scheme is kept as given.
/// A path, query, fragment or userinfo is not part of an origin.
fn canonical_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let is_origin = url.host_str().is_some()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if !is_origin {
        return None;
    }
    let origin = url.origin();
    Some(if origin.is_tuple() { origin.ascii_serialization() } else { value.to_owned() })
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

    fn prepare_origins(origins: &[&str]) -> Result<McpConfigDto, TuliproxError> {
        let mut dto = McpConfigDto {
            allowed_origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            ..McpConfigDto::default()
        };
        dto.prepare().map(|()| dto)
    }

    #[test]
    fn prepare_rejects_origins_without_scheme_or_with_a_path_query_or_fragment() {
        for invalid in
            ["claude.ai", "localhost:3000", "https://a.example/app", "https://a.example?x=1", "https://a.example#top"]
        {
            let err = prepare_origins(&["https://ok.example", invalid]).unwrap_err().to_string();
            assert!(err.contains(invalid), "{invalid}: {err}");
        }
    }

    #[test]
    fn prepare_accepts_custom_scheme_and_port_origins() {
        let dto = prepare_origins(&["vscode-webview://abc123", "http://localhost:3000/", "https://claude.ai"]).unwrap();
        assert_eq!(dto.allowed_origins, ["vscode-webview://abc123", "http://localhost:3000", "https://claude.ai"]);
    }

    #[test]
    fn prepare_canonicalizes_tuple_origins_to_the_browser_origin_header() {
        let dto = prepare_origins(&[
            "https://claude.ai:443/",
            "http://LOCALHOST:80",
            "https://claude.ai:8443",
            "wss://Host.example:443/",
        ])
        .unwrap();
        assert_eq!(
            dto.allowed_origins,
            ["https://claude.ai", "http://localhost", "https://claude.ai:8443", "wss://host.example"]
        );
    }

    #[test]
    fn prepare_rejects_origins_with_userinfo() {
        assert!(prepare_origins(&["https://user@a.example"]).is_err());
    }

    #[test]
    fn prepare_keeps_the_origins_when_validation_fails() {
        let mut dto = McpConfigDto {
            allowed_origins: vec![" https://a.example/ ".to_string(), "bad".to_string()],
            ..McpConfigDto::default()
        };
        assert!(dto.prepare().is_err());
        assert_eq!(dto.allowed_origins, [" https://a.example/ ", "bad"]);
    }
}
