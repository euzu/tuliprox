//! MCP server defaults.

use crate::model::McpPlayerTools;

default_eq_fns!(
    default_mcp_max_response_bytes, is_default_mcp_max_response_bytes, usize, 262_144;
);

pub const fn default_mcp_player_tools() -> McpPlayerTools { McpPlayerTools::Off }

pub fn is_default_mcp_player_tools(value: &McpPlayerTools) -> bool { *value == default_mcp_player_tools() }

/// Smallest accepted `mcp.max_response_bytes`; anything below cannot hold the truncation marker and a useful payload.
pub const MIN_MCP_MAX_RESPONSE_BYTES: usize = 1024;
