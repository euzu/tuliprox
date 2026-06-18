use crate::model::LibraryScanSummary;
use serde::{Deserialize, Serialize};

/// Marker response for accepted long-running operations. Carries no payload:
/// progress is delivered over the event channel, not through this response.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct OperationRunAccepted {}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaylistUpdateProgressEvent {
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LibraryScanProgressEvent {
    pub summary: LibraryScanSummary,
}
