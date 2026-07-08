//! Generic networking defaults: resolve/probe delays, grace periods, retry,
//! interner GC, port, user-agent, background flags.

default_eq_fns!(
    default_resolve_delay_secs, is_default_resolve_delay_secs, u16, 2;
    default_probe_delay_secs, is_default_probe_delay_secs, u16, 2;
    default_grace_period_millis, is_default_grace_period_millis, u64, 2000;
    default_shared_burst_buffer_mb, is_default_shared_burst_buffer_mb, u64, 12;
    default_grace_period_timeout_secs, is_default_grace_period_timeout_secs, u64, 4;
    default_catchup_session_ttl_secs, is_default_catchup_session_ttl_secs, u64, 45;
    default_connect_timeout_secs, is_default_connect_timeout_secs, u32, 6;
    default_resource_retry_attempts, is_default_resource_retry_attempts, u32, 3;
    default_resource_retry_backoff_ms, is_default_resource_retry_backoff_ms, u64, 250;
    default_interner_gc_interval_secs, is_default_interner_gc_interval_secs, u32, 180;
    default_interner_gc_min_pool_size, is_default_interner_gc_min_pool_size, u32, 100;
    default_custom_stream_response_error_status, is_default_custom_stream_response_error_status, u16, 502;
);

pub const fn default_resource_retry_backoff_multiplier() -> f64 { 1.0 }
pub const F64_DEFAULT_EPSILON: f64 = 1e-9;
pub const fn is_default_resource_retry_backoff_multiplier(v: &f64) -> bool {
    (*v - default_resource_retry_backoff_multiplier()).abs() < F64_DEFAULT_EPSILON
}

pub const fn default_resolve_background() -> bool { true }
pub const fn default_xtream_live_stream_use_prefix() -> bool { true }

pub const DEFAULT_PORT: u16 = 8901;
pub const DEFAULT_USER_AGENT: &str = "VLC/3.0.16 LibVLC/3.0.16";
pub fn default_default_user_agent() -> Option<String> { Some(DEFAULT_USER_AGENT.to_string()) }
