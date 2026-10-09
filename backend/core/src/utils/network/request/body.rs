use super::{
    format_http_status, is_retryable_text_response_status, perform_backoff,
    send_input_with_retry_and_provider_policy_with_options_result, should_retry_text_body_error,
    sleep_before_text_body_retry, text_body_retry_values, DynReader, RequestFetchOptions, TextContentBodyOptions,
    TextContentFetchOptions,
};
use crate::{
    model::{AppConfig, InputSource},
    utils::content_coding::{
        content_decoding_error_from_io, decode_response_to_identity, is_http_body_transport_error,
        log_hls_origin_content_coding, read_utf8_limited, ContentBodyReadError, ContentCodingDetection,
        ContentCodingError, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource,
    },
};
use reqwest::{header::HeaderMap, StatusCode};
use shared::{error::string_to_io_error, utils::sanitize_sensitive_info};
use std::{
    io::{Error, ErrorKind},
    sync::Arc,
};
use tokio::io::AsyncReadExt;
use url::Url;

async fn build_decoded_stream_reader(response: reqwest::Response) -> Result<DynReader, std::io::Error> {
    decode_response_to_identity(response, ContentCodingDetection::DeclaredOrLegacyTextMagic)
        .await
        .map(|decoded| decoded.body)
        .map_err(content_coding_error_to_io)
}

fn content_coding_error_to_io(error: ContentCodingError) -> Error {
    let kind = match &error {
        ContentCodingError::PrefixRead(source) => source.kind(),
        ContentCodingError::InvalidHeader
        | ContentCodingError::Unsupported(_)
        | ContentCodingError::EncodedPartialContent => ErrorKind::Other,
    };
    Error::new(kind, error)
}

fn content_body_read_error_to_io(error: ContentBodyReadError) -> Error {
    match error {
        ContentBodyReadError::Io(error) => error,
        error @ ContentBodyReadError::LimitExceeded { .. } => Error::other(error),
        error @ ContentBodyReadError::InvalidUtf8 { .. } => Error::new(ErrorKind::InvalidData, error),
    }
}

/// Builds a fixed/numeric text-response diagnostic without origin-controlled content.
pub fn text_response_error_log_label(error: &Error) -> String {
    if let Some(error) = content_decoding_error_from_io(error) {
        return format!("content_decoding coding={}", error.coding.as_http_token());
    }
    if let Some(error) = error.get_ref().and_then(|source| source.downcast_ref::<ContentBodyReadError>()) {
        return match error {
            ContentBodyReadError::LimitExceeded { limit } => format!("decoded_body_limit limit={limit}"),
            ContentBodyReadError::InvalidUtf8 { valid_up_to, error_len } => {
                format!("invalid_utf8 valid_up_to={valid_up_to} error_len={error_len:?}")
            }
            ContentBodyReadError::Io(error) => format!("io kind={:?}", error.kind()),
        };
    }
    if let Some(error) = error.get_ref().and_then(|source| source.downcast_ref::<ContentCodingError>()) {
        return match error {
            ContentCodingError::InvalidHeader => "content_coding class=invalid_header".to_string(),
            ContentCodingError::Unsupported(_) => "content_coding class=unsupported".to_string(),
            ContentCodingError::EncodedPartialContent => "content_coding class=encoded_partial_content".to_string(),
            ContentCodingError::PrefixRead(_) => "content_coding class=prefix_read".to_string(),
        };
    }
    if error.kind() == ErrorKind::TimedOut {
        return "timeout".to_string();
    }
    if is_http_body_transport_error(error) {
        return "transport".to_string();
    }
    format!("io kind={:?}", error.kind())
}

pub(super) async fn read_text_response_with_body_options(
    response: reqwest::Response,
    body_options: TextContentBodyOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let request_url = response.url().to_string();
    let read = async move {
        let mut decoded =
            decode_response_to_identity(response, body_options.detection).await.map_err(content_coding_error_to_io)?;
        if let (ContentCodingDetection::DeclaredOrKnownHlsManifestMagic, Some(observation)) =
            (body_options.detection, decoded.content_coding_observation())
        {
            log_hls_origin_content_coding(
                observation,
                HlsOriginContentCodingObjectKind::Manifest,
                false,
                HlsOriginContentCodingSource::Legacy,
            );
        }
        let content = if let Some(max_decoded_bytes) = body_options.max_decoded_bytes {
            read_utf8_limited(&mut decoded.body, max_decoded_bytes).await.map_err(content_body_read_error_to_io)?
        } else {
            let mut content = String::new();
            decoded.body.read_to_string(&mut content).await.map_err(|error| Error::new(error.kind(), error))?;
            content
        };
        Ok((content, decoded.final_url.to_string(), decoded.headers))
    };

    if let Some(deadline) = body_options.deadline {
        tokio::time::timeout(deadline, read).await.map_err(|_| {
            Error::new(
                ErrorKind::TimedOut,
                format!("Timed out reading content body: {}", sanitize_sensitive_info(&request_url)),
            )
        })?
    } else {
        read.await
    }
}

#[allow(clippy::implicit_hasher)]
pub async fn get_remote_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
) -> Result<(DynReader, String), Error> {
    get_remote_content_as_stream_with_options(app_config, client, input, headers, url, RequestFetchOptions::default())
        .await
}

#[allow(clippy::implicit_hasher)]
async fn get_remote_content_as_stream_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: RequestFetchOptions,
) -> Result<(DynReader, String), Error> {
    let response =
        send_input_with_retry_and_provider_policy_with_options_result(app_config, client, input, headers, url, options)
            .await?
            .response;
    let response_url = response.url().to_string();

    let reader = build_decoded_stream_reader(response).await?;
    Ok((reader, response_url))
}

pub(super) fn text_response_status_error(status: StatusCode, url: &Url) -> Error {
    string_to_io_error(format!(
        "Request failed ({}): {}",
        format_http_status(status),
        sanitize_sensitive_info(url.as_str())
    ))
}

pub(super) async fn get_remote_content_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    get_remote_content_with_headers_and_options(
        app_config,
        client,
        input,
        headers,
        url,
        TextContentFetchOptions::with_request_options(options),
    )
    .await
    .map(|(content, response_url, _)| (content, response_url))
}

pub(super) async fn get_remote_content_with_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let (max_attempts, backoff_ms, backoff_multiplier) = text_body_retry_values(app_config, options.body);
    let attempt_options = if max_attempts > 1 { options.request.without_resource_retries() } else { options.request };

    // This consumer owns the logical attempt budget whenever decoded-body retries are enabled. Provider failover
    // and redirect hops remain bounded subrequests, but the configured retry count is not applied again below it.
    for attempt in 0..max_attempts {
        let response = match send_input_with_retry_and_provider_policy_with_options_result(
            app_config,
            client,
            input,
            headers,
            url,
            attempt_options,
        )
        .await
        {
            Ok(result) => result.response,
            Err(error) if should_retry_text_body_error(&error) && attempt + 1 < max_attempts => {
                sleep_before_text_body_retry(attempt, backoff_ms, backoff_multiplier, &error).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        let status = response.status();
        if !status.is_success() {
            if is_retryable_text_response_status(status) && attempt + 1 < max_attempts {
                perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                continue;
            }
            return Err(text_response_status_error(status, url));
        }
        match read_text_response_with_body_options(response, options.body).await {
            Ok(result) => return Ok(result),
            Err(error) if should_retry_text_body_error(&error) && attempt + 1 < max_attempts => {
                sleep_before_text_body_retry(attempt, backoff_ms, backoff_multiplier, &error).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(string_to_io_error("Text response body retry attempts exhausted"))
}
