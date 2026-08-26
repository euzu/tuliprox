//! Persisted shape of one Stalker EPG programme.
//!
//! Written and read by the repository, produced by the Stalker client. Plain
//! data with no behaviour, so it sits below both.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalkerProgramRecord {
    pub channel_id: Option<String>,
    pub title: String,
    pub start_epoch: Option<i64>,
    pub stop_epoch: Option<i64>,
    pub description: Option<String>,
    pub category: Option<String>,
}
