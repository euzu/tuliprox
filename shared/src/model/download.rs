use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FileDownloadDto {
    pub uuid: String,
    pub filename: String,
    #[serde(default)]
    pub kind: String,
    pub filesize: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
    pub finished: bool,
    pub paused: bool,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DownloadsResponse {
    pub completed: bool,
    pub queue: Vec<FileDownloadDto>,
    pub downloads: Vec<FileDownloadDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<FileDownloadDto>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DownloadActionResponse {
    pub success: bool,
}
