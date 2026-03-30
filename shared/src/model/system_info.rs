use serde::{Deserialize, Serialize};

/// System resource usage information for the current process.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemInfo {
    /// CPU usage as a percentage (0.0 - 100.0+)
    pub cpu_usage: f32,
    /// Memory used by the process in bytes
    pub memory_usage: u64,
    /// Total system memory in bytes
    pub memory_total: u64,
    /// Network receive bytes per second (system-wide)
    pub net_rx_bytes_per_sec: f64,
    /// Network transmit bytes per second (system-wide)
    pub net_tx_bytes_per_sec: f64,
}
