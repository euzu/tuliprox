use crate::model::LibraryScanSummary;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationRunAccepted {
    pub run_id: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaylistUpdateProgressEvent {
    pub run_id: u64,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LibraryScanProgressEvent {
    pub run_id: u64,
    pub summary: LibraryScanSummary,
}
