use crate::{
    error::{Error, ErrorInfo},
    services::{
        get_base_href, request_delete_raw, request_get, request_post, request_post_raw, request_put_raw, Encoding,
        RawResponse,
    },
    utils::encoding_for_query,
};
use serde::de::DeserializeOwned;
use shared::{
    model::{
        CandidatePageDto, FailoverCandidateFilter, FailoverGroupConflictDto, FailoverGroupHealthDto,
        FailoverGroupListDto, FailoverGroupSaveResultDto, FailoverGroupTargetsDto, OperationRunAccepted,
        OverrideValidationError, ReassignOverridesRequestDto, SaveOverridesRequestDto, TargetFailoverOverridesDto,
        TargetOverridesResponseDto, VirtualId,
    },
    utils::concat_path_leading_slash,
};

/// Result of a write of the failover overrides; the answers a caller has to handle, not just report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved(FailoverGroupSaveResultDto),
    /// The section changed since `expected_etag`; carries the etag found on the server.
    Conflict(Option<String>),
    Invalid(Vec<OverrideValidationError>),
    /// The failover file cannot be written (broken or read-only); carries the reason.
    Locked(String),
}

fn parse_body<T: DeserializeOwned>(body: &str) -> Result<T, Error> {
    serde_json::from_str(body).map_err(|_| Error::DeserializeError)
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<ErrorInfo>(body).map_or_else(|_| body.trim().to_string(), |info| info.error)
}

fn non_empty_or(message: String, fallback: &str) -> String {
    if message.is_empty() {
        fallback.to_string()
    } else {
        message
    }
}

/// Maps the answer of a write route to its outcome; statuses outside the contract become errors.
fn write_outcome(response: &RawResponse) -> Result<SaveOutcome, Error> {
    let body = response.body.as_str();
    match response.status {
        200 => parse_body(body).map(SaveOutcome::Saved),
        409 => Ok(SaveOutcome::Conflict(
            serde_json::from_str::<FailoverGroupConflictDto>(body).ok().and_then(|conflict| conflict.current_etag),
        )),
        422 => parse_body(body).map(SaveOutcome::Invalid),
        423 => Ok(SaveOutcome::Locked(non_empty_or(error_message(body), "Failover file is locked"))),
        400 => Err(Error::BadRequest(non_empty_or(error_message(body), "400"))),
        401 => Err(Error::Unauthorized),
        403 => Err(Error::Forbidden(non_empty_or(error_message(body), "Forbidden"))),
        404 => Err(Error::NotFound),
        500 => Err(Error::InternalServerError(non_empty_or(error_message(body), "Internal Server Error"))),
        _ => {
            let message = error_message(body);
            Err(if message.is_empty() { Error::RequestError } else { Error::HttpResponse(message) })
        }
    }
}

#[derive(Default)]
pub struct FailoverService;

impl FailoverService {
    pub fn new() -> Self { Self }

    fn collection_url_from_base(base_href: &str) -> String {
        concat_path_leading_slash(base_href.trim_end_matches('/'), "api/v1/failover")
    }

    /// `endpoint` below the failover routes with the given query parameters, values URL-encoded.
    fn url(endpoint: &str, params: &[(&str, &str)]) -> String {
        let collection = Self::collection_url_from_base(&get_base_href());
        let mut url = concat_path_leading_slash(&collection, endpoint);
        for (index, (key, value)) in params.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(&encoding_for_query(value));
        }
        url
    }

    async fn get<T>(url: &str) -> Result<T, Error>
    where
        T: DeserializeOwned + 'static + std::fmt::Debug,
    {
        request_get::<T>(url, None, Some(Encoding::Json)).await?.ok_or(Error::NotFound)
    }

    pub async fn fetch_targets() -> Result<FailoverGroupTargetsDto, Error> {
        Self::get(&Self::url("targets", &[])).await
    }

    pub async fn fetch_groups(target: &str) -> Result<FailoverGroupListDto, Error> {
        Self::get(&Self::url("groups", &[("target", target)])).await
    }

    pub async fn fetch_overrides(target: &str) -> Result<TargetOverridesResponseDto, Error> {
        Self::get(&Self::url("overrides", &[("target", target)])).await
    }

    /// The groups the draft would produce; nothing is written.
    pub async fn preview(target: &str, draft: &TargetFailoverOverridesDto) -> Result<FailoverGroupListDto, Error> {
        request_post::<_, FailoverGroupListDto>(
            &Self::url("preview", &[("target", target)]),
            draft,
            Some(Encoding::Json),
            Some(Encoding::Json),
        )
        .await?
        .ok_or(Error::NotFound)
    }

    pub async fn search_candidates(
        target: &str,
        query: &str,
        filter: FailoverCandidateFilter,
        offset: usize,
        limit: usize,
    ) -> Result<CandidatePageDto, Error> {
        let filter = serde_json::to_value(filter).ok().and_then(|value| value.as_str().map(str::to_string));
        let offset = offset.to_string();
        let limit = limit.to_string();
        let url = Self::url(
            "candidates",
            &[
                ("target", target),
                ("q", query),
                ("filter", filter.as_deref().unwrap_or("all")),
                ("offset", &offset),
                ("limit", &limit),
            ],
        );
        Self::get(&url).await
    }

    pub async fn fetch_health(target: &str, group_vid: VirtualId) -> Result<FailoverGroupHealthDto, Error> {
        let group = group_vid.get().to_string();
        Self::get(&Self::url("health", &[("target", target), ("group", &group)])).await
    }

    /// Stores the section; `expected_etag` is the etag the draft is based on (`None` for a new section).
    pub async fn save(
        target: &str,
        expected_etag: Option<String>,
        overrides: TargetFailoverOverridesDto,
    ) -> Result<SaveOutcome, Error> {
        let request = SaveOverridesRequestDto { expected_etag, overrides };
        write_outcome(&request_put_raw(&Self::url("overrides", &[("target", target)]), &request).await?)
    }

    /// Removes the whole section of the target.
    pub async fn delete(target: &str, expected_etag: &str) -> Result<SaveOutcome, Error> {
        let url = Self::url("overrides", &[("target", target), ("expected_etag", expected_etag)]);
        write_outcome(&request_delete_raw(&url).await?)
    }

    /// Moves the section of `from` (typically an orphan) to the configured target `to`.
    pub async fn reassign(from: &str, to: &str, expected_etag: &str) -> Result<SaveOutcome, Error> {
        let request = ReassignOverridesRequestDto {
            from: from.to_string(),
            to: to.to_string(),
            expected_etag: expected_etag.to_string(),
        };
        write_outcome(&request_post_raw(&Self::url("overrides/reassign", &[]), &request).await?)
    }

    /// Queues a playlist update for the target; a busy queue answers with `Error::Conflict`.
    pub async fn apply(target: &str) -> Result<OperationRunAccepted, Error> {
        Ok(request_post::<_, OperationRunAccepted>(
            &Self::url("apply", &[("target", target)]),
            (),
            None,
            Some(Encoding::Json),
        )
        .await?
        .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, body: &str) -> RawResponse { RawResponse { status, body: body.to_string() } }

    #[test]
    fn collection_url_never_becomes_protocol_relative() {
        assert_eq!(FailoverService::collection_url_from_base("/"), "/api/v1/failover");
        assert_eq!(FailoverService::collection_url_from_base("/tuliprox/"), "/tuliprox/api/v1/failover");
    }

    #[test]
    fn success_is_parsed_as_save_result() {
        let outcome = write_outcome(&raw(200, r#"{"etag":"abc","file":"f.yml","hot_applied":true}"#)).expect("outcome");
        assert_eq!(
            outcome,
            SaveOutcome::Saved(FailoverGroupSaveResultDto {
                etag: Some("abc".to_string()),
                file: Some("f.yml".to_string()),
                hot_applied: true,
            })
        );
    }

    #[test]
    fn conflict_carries_current_etag() {
        assert_eq!(
            write_outcome(&raw(409, r#"{"current_etag":"e7"}"#)).expect("outcome"),
            SaveOutcome::Conflict(Some("e7".to_string()))
        );
        assert_eq!(write_outcome(&raw(409, r#"{"current_etag":null}"#)).expect("outcome"), SaveOutcome::Conflict(None));
        assert_eq!(write_outcome(&raw(409, r#"{"error":"busy"}"#)).expect("outcome"), SaveOutcome::Conflict(None));
    }

    #[test]
    fn unprocessable_carries_findings() {
        let outcome =
            write_outcome(&raw(422, r#"[{"error":"empty_manual_id"},{"error":"duplicate_manual_id","detail":"a"}]"#))
                .expect("outcome");
        assert_eq!(
            outcome,
            SaveOutcome::Invalid(vec![
                OverrideValidationError::EmptyManualId,
                OverrideValidationError::DuplicateManualId("a".to_string()),
            ])
        );
    }

    #[test]
    fn locked_carries_reason() {
        assert_eq!(
            write_outcome(&raw(423, r#"{"error":"file has a parse error"}"#)).expect("outcome"),
            SaveOutcome::Locked("file has a parse error".to_string())
        );
        assert_eq!(
            write_outcome(&raw(423, "")).expect("outcome"),
            SaveOutcome::Locked("Failover file is locked".to_string())
        );
    }

    #[test]
    fn other_statuses_are_errors() {
        assert_eq!(write_outcome(&raw(401, "")), Err(Error::Unauthorized));
        assert_eq!(write_outcome(&raw(404, r#"{"error":"x"}"#)), Err(Error::NotFound));
        assert_eq!(write_outcome(&raw(500, r#"{"error":"io"}"#)), Err(Error::InternalServerError("io".to_string())));
        assert_eq!(write_outcome(&raw(503, "")), Err(Error::RequestError));
        assert_eq!(write_outcome(&raw(200, "not json")), Err(Error::DeserializeError));
    }
}
