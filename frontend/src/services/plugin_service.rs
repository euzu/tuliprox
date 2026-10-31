use crate::{
    error::{Error, ErrorInfo},
    services::{get_base_href, get_token, request_get, request_post_form, RawResponse},
};
use gloo_net::http::{Request, RequestBuilder};
use js_sys::encode_uri_component;
use serde::de::DeserializeOwned;
use shared::{
    model::{
        PluginApproveDto, PluginLogEntryDto, PluginLogLevelDto, PluginLogQueryDto, PluginSettingsDto,
        PluginSettingsErrorDto, PluginSettingsSaveDto, PluginStatsDto, PluginSummaryDto, PluginTaskDto,
        PluginUploadErrorDto, PluginUploadReviewDto,
    },
    utils::{concat_path, concat_path_leading_slash, CONTENT_TYPE_JSON},
};
use web_sys::{File, FormData};

/// Why a settings save failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSaveError {
    /// The server rejected the settings with a `422` and a field-level body.
    Invalid(PluginSettingsErrorDto),
    /// Any other failure, with a message for display.
    Other(String),
}

/// Why an upload, approve, enable, disable or uninstall failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginActionError {
    /// The HTTP status, `0` when the request never got an answer.
    pub status: u16,
    pub error: String,
    /// Every problem a rejected upload found (`422`); empty otherwise.
    pub problems: Vec<String>,
}

impl PluginActionError {
    /// The error, then each problem on its own line.
    pub fn message(&self) -> String {
        self.problems.iter().fold(self.error.clone(), |mut message, problem| {
            message.push_str("\n- ");
            message.push_str(problem);
            message
        })
    }

    fn transport(err: &Error) -> Self { Self { status: 0, error: err.to_string(), problems: Vec::new() } }
}

pub struct PluginService {
    plugins_path: String,
}

fn encode_path_segment(value: &str) -> String { encode_uri_component(value).into() }

fn required<T>(body: Option<T>) -> Result<T, Error> { body.ok_or(Error::DeserializeError) }

fn level_param(level: PluginLogLevelDto) -> &'static str {
    match level {
        PluginLogLevelDto::Error => "error",
        PluginLogLevelDto::Warn => "warn",
        PluginLogLevelDto::Info => "info",
        PluginLogLevelDto::Debug => "debug",
        PluginLogLevelDto::Trace => "trace",
    }
}

/// Message for a non-422 failure: the server's `error` field if present, else the raw body, else the status.
fn status_message(status: u16, text: &str) -> String {
    let message = serde_json::from_str::<ErrorInfo>(text).map_or_else(|_| text.trim().to_owned(), |info| info.error);
    if message.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        message
    }
}

/// `PUT` with a JSON body, reading the `422` body so it can be decoded as [`PluginSettingsErrorDto`].
/// Uses the same base URL and bearer token as `requests.rs`, which only exposes the 422 body as a generic error set.
async fn put_settings(url: &str, body: &PluginSettingsSaveDto) -> Result<PluginSettingsDto, PluginSaveError> {
    let payload = serde_json::to_string(body).map_err(|_| PluginSaveError::Other(Error::RequestError.to_string()))?;
    let mut request = Request::put(url).header("Accept", CONTENT_TYPE_JSON);
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }
    let request = request
        .header("Content-Type", CONTENT_TYPE_JSON)
        .body(payload)
        .map_err(|_| PluginSaveError::Other(Error::RequestError.to_string()))?;
    let response = request.send().await.map_err(|_| PluginSaveError::Other(Error::RequestError.to_string()))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    match status {
        200..=299 => serde_json::from_str::<PluginSettingsDto>(&text)
            .map_err(|_| PluginSaveError::Other(Error::DeserializeError.to_string())),
        422 => serde_json::from_str::<PluginSettingsErrorDto>(&text).map_or_else(
            |_| Err(PluginSaveError::Other(status_message(status, &text))),
            |dto| Err(PluginSaveError::Invalid(dto)),
        ),
        _ => Err(PluginSaveError::Other(status_message(status, &text))),
    }
}

/// The body of a failed management action: the server's `PluginUploadErrorDto`,
/// or whatever text it sent.
fn action_error(status: u16, text: &str) -> PluginActionError {
    serde_json::from_str::<PluginUploadErrorDto>(text).map_or_else(
        |_| PluginActionError { status, error: status_message(status, text), problems: Vec::new() },
        |dto| PluginActionError { status, error: dto.error, problems: dto.problems },
    )
}

fn decode_action<T: DeserializeOwned>(response: &RawResponse) -> Result<T, PluginActionError> {
    match response.status {
        200..=299 => {
            serde_json::from_str(&response.body).map_err(|_| PluginActionError::transport(&Error::DeserializeError))
        }
        status => Err(action_error(status, &response.body)),
    }
}

/// Send `request` with the bearer token and an optional JSON body, and hand
/// back the status and body text.
async fn call(request: RequestBuilder, body: Option<String>) -> Result<RawResponse, Error> {
    let mut request = request.header("Accept", CONTENT_TYPE_JSON);
    if let Some(token) = get_token() {
        request = request.header("Authorization", format!("Bearer {token}").as_str());
    }
    let response = match body {
        Some(payload) => {
            request
                .header("Content-Type", CONTENT_TYPE_JSON)
                .body(payload)
                .map_err(|_| Error::RequestError)?
                .send()
                .await
        }
        None => request.send().await,
    }
    .map_err(|_| Error::RequestError)?;
    Ok(RawResponse { status: response.status(), body: response.text().await.unwrap_or_default() })
}

impl PluginService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self { plugins_path: concat_path_leading_slash(&base_href, "api/v1/plugins") }
    }

    pub async fn list(&self) -> Result<Vec<PluginSummaryDto>, Error> {
        request_get(&self.plugins_path, None, None).await.map(Option::unwrap_or_default)
    }

    pub async fn get_stats(&self, id: &str) -> Result<PluginStatsDto, Error> {
        let id = encode_path_segment(id);
        required(request_get(&concat_path(&self.plugins_path, &format!("{id}/stats")), None, None).await?)
    }

    pub async fn get_settings(&self, id: &str) -> Result<PluginSettingsDto, Error> {
        let id = encode_path_segment(id);
        required(request_get(&concat_path(&self.plugins_path, &format!("{id}/settings")), None, None).await?)
    }

    pub async fn save_settings(
        &self,
        id: &str,
        req: &PluginSettingsSaveDto,
    ) -> Result<PluginSettingsDto, PluginSaveError> {
        let id = encode_path_segment(id);
        put_settings(&concat_path(&self.plugins_path, &format!("{id}/settings")), req).await
    }

    /// `POST /api/v1/plugins`: upload a `.tlpx` and get the capability review.
    /// The package is staged, not installed, until [`PluginService::approve`].
    pub async fn upload(&self, file: &File) -> Result<PluginUploadReviewDto, PluginActionError> {
        let form = FormData::new().map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        form.append_with_blob_and_filename("file", file, &file.name())
            .map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let response =
            request_post_form(&self.plugins_path, form).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `PUT …/approve` with the hash and capabilities that were reviewed.
    pub async fn approve(&self, id: &str, body: &PluginApproveDto) -> Result<PluginSummaryDto, PluginActionError> {
        let payload = serde_json::to_string(body).map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let url = concat_path(&self.plugins_path, &format!("{}/approve", encode_path_segment(id)));
        let response =
            call(Request::put(&url), Some(payload)).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `PUT …/enable` or `…/disable`.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<PluginSummaryDto, PluginActionError> {
        let action = if enabled { "enable" } else { "disable" };
        let url = concat_path(&self.plugins_path, &format!("{}/{action}", encode_path_segment(id)));
        let response = call(Request::put(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `DELETE /api/v1/plugins/{id}`, with the KV store when `purge_kv`.
    pub async fn uninstall(&self, id: &str, purge_kv: bool) -> Result<(), PluginActionError> {
        let url = concat_path(&self.plugins_path, &encode_path_segment(id));
        let url = if purge_kv { format!("{url}?purge_kv=true") } else { url };
        let response = call(Request::delete(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        match response.status {
            200..=299 => Ok(()),
            status => Err(action_error(status, &response.body)),
        }
    }

    pub async fn get_tasks(&self, id: &str) -> Result<Vec<PluginTaskDto>, Error> {
        let id = encode_path_segment(id);
        request_get(&concat_path(&self.plugins_path, &format!("{id}/tasks")), None, None)
            .await
            .map(Option::unwrap_or_default)
    }

    /// Only the set query fields are sent.
    pub async fn get_logs(&self, id: &str, query: &PluginLogQueryDto) -> Result<Vec<PluginLogEntryDto>, Error> {
        let id = encode_path_segment(id);
        let mut params = Vec::new();
        if let Some(level) = query.level {
            params.push(format!("level={}", level_param(level)));
        }
        if let Some(since) = query.since {
            params.push(format!("since={since}"));
        }
        if let Some(limit) = query.limit {
            params.push(format!("limit={limit}"));
        }
        let mut url = concat_path(&self.plugins_path, &format!("{id}/logs"));
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        request_get(&url, None, None).await.map(Option::unwrap_or_default)
    }
}

impl Default for PluginService {
    fn default() -> Self { Self::new() }
}
