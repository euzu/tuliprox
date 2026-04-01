use crate::{
    error::Error,
    services::{get_base_href, request_get, request_post, Encoding},
};
use serde::Serialize;
use shared::{
    model::{DownloadActionResponse, DownloadsResponse, FileDownloadDto},
    utils::concat_path_leading_slash,
};

#[derive(Clone, Debug, Serialize)]
pub struct QueueDownloadRequest {
    pub url: String,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueueRecordingRequest {
    pub url: String,
    pub filename: String,
    pub start_at: i64,
    pub duration_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DownloadActionRequest {
    pub uuid: String,
}

pub struct DownloadsService {
    downloads_api_path: String,
    downloads_info_api_path: String,
}

impl DownloadsService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self {
            downloads_api_path: concat_path_leading_slash(&base_href, "api/v1/file/download"),
            downloads_info_api_path: concat_path_leading_slash(&base_href, "api/v1/file/download/info"),
        }
    }

    pub async fn queue_download(
        &self,
        url: String,
        filename: String,
        input_name: Option<String>,
    ) -> Result<FileDownloadDto, Error> {
        let request = QueueDownloadRequest { url, filename, input_name, priority: None };
        request_post::<&QueueDownloadRequest, FileDownloadDto>(
            &self.downloads_api_path,
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }

    pub async fn get_downloads(&self) -> Result<DownloadsResponse, Error> {
        request_get::<DownloadsResponse>(&self.downloads_info_api_path, None, Some(Encoding::Json))
            .await?
            .ok_or(Error::RequestError)
    }

    pub async fn queue_recording(
        &self,
        url: String,
        filename: String,
        start_at: i64,
        duration_secs: u64,
        input_name: Option<String>,
        priority: Option<i8>,
    ) -> Result<FileDownloadDto, Error> {
        let request = QueueRecordingRequest { url, filename, start_at, duration_secs, input_name, priority };
        request_post::<&QueueRecordingRequest, FileDownloadDto>(
            &concat_path_leading_slash(&get_base_href(), "api/v1/file/record"),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }

    pub async fn pause_download(&self, uuid: String) -> Result<DownloadActionResponse, Error> {
        let request = DownloadActionRequest { uuid };
        request_post::<&DownloadActionRequest, DownloadActionResponse>(
            &format!("{}/pause", self.downloads_api_path),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }

    pub async fn resume_download(&self, uuid: String) -> Result<DownloadActionResponse, Error> {
        let request = DownloadActionRequest { uuid };
        request_post::<&DownloadActionRequest, DownloadActionResponse>(
            &format!("{}/resume", self.downloads_api_path),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }

    pub async fn cancel_download(&self, uuid: String) -> Result<DownloadActionResponse, Error> {
        let request = DownloadActionRequest { uuid };
        request_post::<&DownloadActionRequest, DownloadActionResponse>(
            &format!("{}/cancel", self.downloads_api_path),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }

    pub async fn remove_download(&self, uuid: String) -> Result<DownloadActionResponse, Error> {
        let request = DownloadActionRequest { uuid };
        request_post::<&DownloadActionRequest, DownloadActionResponse>(
            &format!("{}/remove", self.downloads_api_path),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }

    pub async fn retry_download(&self, uuid: String) -> Result<DownloadActionResponse, Error> {
        let request = DownloadActionRequest { uuid };
        request_post::<&DownloadActionRequest, DownloadActionResponse>(
            &format!("{}/retry", self.downloads_api_path),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::RequestError)
    }
}

impl Default for DownloadsService {
    fn default() -> Self { Self::new() }
}
