use serde::{Deserialize, Serialize};

// Request to trigger a Library scan
#[derive(Debug, Serialize, Deserialize)]
pub struct LibraryScanRequest {
    // Force rescan of all files, ignoring modification timestamps
    #[serde(default)]
    pub force_rescan: bool,
}

// Scan result with statistics
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LibraryScanResult {
    pub files_scanned: usize,
    pub groups_scanned: usize,
    pub files_added: usize,
    pub files_updated: usize,
    pub files_removed: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum LibraryScanSummaryStatus {
    Success,
    Error,
}

// Response for Library scan
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LibraryScanSummary {
    pub status: LibraryScanSummaryStatus,
    pub message: String,
    pub result: Option<LibraryScanResult>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LibraryStatus {
    pub enabled: bool,
    pub total_items: usize,
    pub movies: usize,
    pub series: usize,
    /// Stored episode records; `total_items` still counts movie/series catalog entries.
    #[serde(default)]
    pub episodes: usize,
    pub path: Option<String>,
}
