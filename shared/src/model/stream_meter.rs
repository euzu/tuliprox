use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamMeterEntry {
    pub uids: Vec<u32>,
    pub rate_kbps: u32,
    pub total_kb: u32,
}
