use serde::{Deserialize, Serialize};
use std::fmt;

/// Alert severity for a disk-space notification.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskAlertLevel {
    Warn,
    Critical,
}

impl fmt::Display for DiskAlertLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DiskAlertLevel::Warn => "warning",
            DiskAlertLevel::Critical => "critical",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiskAlert {
    pub level: DiskAlertLevel,
    /// Total size of the filesystem in bytes.
    pub total_bytes: u64,
    /// Bytes free to the current user.
    pub free_bytes: u64,
    /// Used bytes (`total - free`).
    pub used_bytes: u64,
    /// Used percent in the range `[0.0, 100.0]`.
    pub percent: f64,
}
