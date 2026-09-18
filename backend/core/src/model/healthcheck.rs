use serde::{Deserialize, Serialize};
use shared::utils::is_blank_optional_string;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Healthcheck {
    pub status: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub build_time: Option<String>,
    pub server_time: String,
    /// Runtime liveness as observed by the scheduler heartbeat. Absent before
    /// the watchdog is installed (for example in setup mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeHealth>,
}

/// Liveness snapshot produced by the scheduler heartbeat.
///
/// `status` is `alive` while the heartbeat is within its stall threshold and
/// `stalled` otherwise. This field is informational: the enclosing
/// [`Healthcheck::status`] stays `ok` for as long as the HTTP server answers,
/// so an orchestrator does not restart a process that still serves requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHealth {
    pub status: String,
    pub heartbeat_age_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub stall_threshold_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_for_ms: Option<u64>,
    pub ticks: u64,
    pub stall_episodes: u64,
    pub max_schedule_delay_ms: u64,
    pub uptime_ms: u64,
}
