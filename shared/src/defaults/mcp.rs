//! MCP server defaults.

default_eq_fns!(
    default_mcp_max_response_bytes, is_default_mcp_max_response_bytes, usize, 262_144;
);

/// Smallest accepted `mcp.max_response_bytes`; anything below cannot hold the truncation marker and a useful payload.
pub const MIN_MCP_MAX_RESPONSE_BYTES: usize = 1024;
