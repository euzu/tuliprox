use crate::{
    error::{Error, ErrorInfo},
    services::{get_base_href, request_get, request_post_form, request_raw, RawResponse},
};
use gloo_net::http::Request;
use js_sys::encode_uri_component;
use serde::de::DeserializeOwned;
use shared::{
    model::{
        MarketplaceDto, MarketplaceInstallDto, MarketplaceQueryDto, PluginApproveDto, PluginBootReportDto,
        PluginLogEntryDto, PluginLogLevelDto, PluginLogQueryDto, PluginReferenceDto, PluginRepositoryAddDto,
        PluginRepositoryDto, PluginRepositoryInfoDto, PluginSettingsDto, PluginSettingsErrorDto, PluginSettingsSaveDto,
        PluginStatsDto, PluginSummaryDto, PluginTaskDto, PluginUpdatePolicy, PluginUpdatePolicyDto,
        PluginUpdateRequestDto, PluginUpdateResultDto, PluginUpdateSettingsDto, PluginUpdatesDto, PluginUploadErrorDto,
        PluginUploadReviewDto,
    },
    utils::{concat_path, concat_path_leading_slash},
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

impl PluginService {
    fn repositories_path(&self) -> String { concat_path(&self.plugins_path, "repositories") }

    /// `…/repositories/{id}` plus `suffix`.
    fn repository_path(&self, id: &str, suffix: &str) -> String {
        concat_path(&self.repositories_path(), &format!("{}{suffix}", encode_path_segment(id)))
    }

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
        let url = concat_path(&self.plugins_path, &format!("{id}/settings"));
        let payload =
            serde_json::to_string(req).map_err(|_| PluginSaveError::Other(Error::RequestError.to_string()))?;
        let response = request_raw(Request::put(&url), Some(payload))
            .await
            .map_err(|err| PluginSaveError::Other(err.to_string()))?;
        match response.status {
            200..=299 => serde_json::from_str::<PluginSettingsDto>(&response.body)
                .map_err(|_| PluginSaveError::Other(Error::DeserializeError.to_string())),
            422 => serde_json::from_str::<PluginSettingsErrorDto>(&response.body).map_or_else(
                |_| Err(PluginSaveError::Other(status_message(response.status, &response.body))),
                |dto| Err(PluginSaveError::Invalid(dto)),
            ),
            status => Err(PluginSaveError::Other(status_message(status, &response.body))),
        }
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
            request_raw(Request::put(&url), Some(payload)).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `GET …/staged`: the review of an update waiting in staging, so it can be
    /// reviewed and approved without uploading it again.
    pub async fn staged_review(&self, id: &str) -> Result<PluginUploadReviewDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, &format!("{}/staged", encode_path_segment(id)));
        let response = request_raw(Request::get(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `POST …/rollback`: go back to the retained prior version. A `409` means the
    /// prior failed its activation check and the current version is still live.
    pub async fn rollback(&self, id: &str) -> Result<PluginSummaryDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, &format!("{}/rollback", encode_path_segment(id)));
        let response =
            request_raw(Request::post(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `PUT …/enable` or `…/disable`.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<PluginSummaryDto, PluginActionError> {
        let action = if enabled { "enable" } else { "disable" };
        let url = concat_path(&self.plugins_path, &format!("{}/{action}", encode_path_segment(id)));
        let response = request_raw(Request::put(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `GET /api/v1/plugins/{id}/usage`: the inputs, targets and schedules that name the plugin.
    pub async fn references(&self, id: &str) -> Result<Vec<PluginReferenceDto>, Error> {
        let id = encode_path_segment(id);
        request_get(&concat_path(&self.plugins_path, &format!("{id}/usage")), None, None)
            .await
            .map(Option::unwrap_or_default)
    }

    /// `DELETE /api/v1/plugins/{id}`, with the KV store when `purge_kv`.
    pub async fn uninstall(&self, id: &str, purge_kv: bool) -> Result<(), PluginActionError> {
        let url = concat_path(&self.plugins_path, &encode_path_segment(id));
        let url = if purge_kv { format!("{url}?purge_kv=true") } else { url };
        let response =
            request_raw(Request::delete(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        match response.status {
            200..=299 => Ok(()),
            status => Err(action_error(status, &response.body)),
        }
    }

    /// `GET …/repositories`: every configured marketplace source with its sync state.
    pub async fn list_repositories(&self) -> Result<Vec<PluginRepositoryInfoDto>, Error> {
        request_get(&self.repositories_path(), None, None).await.map(Option::unwrap_or_default)
    }

    /// `POST …/repositories`: add a source by its url; the server takes the id and name from the catalog
    /// (`422` on a bad url, the fetch error status when the catalog cannot be read).
    pub async fn add_repository(
        &self,
        repository: &PluginRepositoryAddDto,
    ) -> Result<PluginRepositoryInfoDto, PluginActionError> {
        let payload =
            serde_json::to_string(repository).map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let response = request_raw(Request::post(&self.repositories_path()), Some(payload))
            .await
            .map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `PUT …/repositories/{id}`: replace a source; the body id must equal `id`.
    pub async fn update_repository(
        &self,
        id: &str,
        repository: &PluginRepositoryDto,
    ) -> Result<PluginRepositoryInfoDto, PluginActionError> {
        let payload =
            serde_json::to_string(repository).map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let response = request_raw(Request::put(&self.repository_path(id, "")), Some(payload))
            .await
            .map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `DELETE …/repositories/{id}`: drop the source and its cache; installed plugins stay.
    pub async fn remove_repository(&self, id: &str) -> Result<(), PluginActionError> {
        let response = request_raw(Request::delete(&self.repository_path(id, "")), None)
            .await
            .map_err(|err| PluginActionError::transport(&err))?;
        match response.status {
            200..=299 => Ok(()),
            status => Err(action_error(status, &response.body)),
        }
    }

    /// `POST …/repositories/{id}/sync`: fetch the catalog now (`502` when the fetch fails).
    pub async fn sync_repository(&self, id: &str) -> Result<PluginRepositoryInfoDto, PluginActionError> {
        let response = request_raw(Request::post(&self.repository_path(id, "/sync")), None)
            .await
            .map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `GET …/marketplace`: the plugins of every enabled source. Only the set filters are sent.
    pub async fn marketplace(&self, query: &MarketplaceQueryDto) -> Result<MarketplaceDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, "marketplace");
        let params = [("q", &query.q), ("category", &query.category), ("source", &query.source)]
            .into_iter()
            .filter_map(|(name, value)| {
                value
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("{name}={}", encode_path_segment(value)))
            })
            .collect::<Vec<_>>();
        let url = if params.is_empty() { url } else { format!("{url}?{}", params.join("&")) };
        let response = request_raw(Request::get(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `POST …/marketplace/install`: download a release into staging and get its review. Nothing
    /// is installed until [`PluginService::approve`]. `404`, `409`, `413`, `422` and `502` carry a reason.
    pub async fn marketplace_install(
        &self,
        request: &MarketplaceInstallDto,
    ) -> Result<PluginUploadReviewDto, PluginActionError> {
        let payload = serde_json::to_string(request).map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let url = concat_path(&self.plugins_path, "marketplace/install");
        let response =
            request_raw(Request::post(&url), Some(payload)).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `GET …/updates`: the newer releases of installed marketplace plugins, from the cached catalogs.
    pub async fn updates(&self) -> Result<PluginUpdatesDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, "updates");
        let response = request_raw(Request::get(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `POST …/updates/check`: sync the sources of installed marketplace plugins, then list the updates.
    pub async fn check_updates(&self) -> Result<PluginUpdatesDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, "updates/check");
        let response =
            request_raw(Request::post(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `POST …/{id}/update`: stage a release and, with `apply` and an eligible package, activate it.
    pub async fn update(
        &self,
        id: &str,
        request: &PluginUpdateRequestDto,
    ) -> Result<PluginUpdateResultDto, PluginActionError> {
        let payload = serde_json::to_string(request).map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let url = concat_path(&self.plugins_path, &format!("{}/update", encode_path_segment(id)));
        let response =
            request_raw(Request::post(&url), Some(payload)).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `PUT …/{id}/update-policy`.
    pub async fn set_update_policy(&self, id: &str, policy: PluginUpdatePolicy) -> Result<(), PluginActionError> {
        let payload = serde_json::to_string(&PluginUpdatePolicyDto { policy })
            .map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let url = concat_path(&self.plugins_path, &format!("{}/update-policy", encode_path_segment(id)));
        let response =
            request_raw(Request::put(&url), Some(payload)).await.map_err(|err| PluginActionError::transport(&err))?;
        match response.status {
            200..=299 => Ok(()),
            status => Err(action_error(status, &response.body)),
        }
    }

    /// `GET …/updates/settings`: the global update fields as written.
    pub async fn update_settings(&self) -> Result<PluginUpdateSettingsDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, "updates/settings");
        let response = request_raw(Request::get(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
    }

    /// `PUT …/updates/settings` (`422` when the budget is not a duration with a unit).
    /// Store the update settings and return them as the server now has them (resolved directories included).
    pub async fn save_update_settings(
        &self,
        settings: &PluginUpdateSettingsDto,
    ) -> Result<PluginUpdateSettingsDto, PluginActionError> {
        let payload =
            serde_json::to_string(settings).map_err(|_| PluginActionError::transport(&Error::RequestError))?;
        let url = concat_path(&self.plugins_path, "updates/settings");
        let response =
            request_raw(Request::put(&url), Some(payload)).await.map_err(|err| PluginActionError::transport(&err))?;
        match response.status {
            200..=299 => {
                serde_json::from_str(&response.body).map_err(|_| PluginActionError::transport(&Error::RequestError))
            }
            status => Err(action_error(status, &response.body)),
        }
    }

    /// `GET …/updates/boot-report`: what the last boot update run did.
    pub async fn boot_report(&self) -> Result<PluginBootReportDto, PluginActionError> {
        let url = concat_path(&self.plugins_path, "updates/boot-report");
        let response = request_raw(Request::get(&url), None).await.map_err(|err| PluginActionError::transport(&err))?;
        decode_action(&response)
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
