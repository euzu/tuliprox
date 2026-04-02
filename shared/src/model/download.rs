use serde::{Deserialize, Serialize};

pub type FileDownloadDto = crate::model::TransferTaskDto;
pub type DownloadsResponse = crate::model::TransfersResponse;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DownloadActionResponse {
    pub success: bool,
}
