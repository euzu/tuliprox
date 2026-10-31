use super::{
    get_local_file_content, is_retryable_text_response_status, perform_backoff, read_text_response_with_body_options,
    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result, should_retry_text_body_error,
    sleep_before_text_body_retry, text_body_retry_values, text_response_status_error, RequestFetchOptions,
    TextContentFetchOptions,
};
use crate::{
    model::{AppConfig, InputSource},
    utils::persist_file,
};
use log::log_enabled;
use regex::Regex;
use reqwest::header::HeaderMap;
use shared::{error::string_to_io_error, model::format_elapsed_time, utils::sanitize_sensitive_info};
use std::{
    collections::HashMap,
    io::{Error, ErrorKind},
    path::PathBuf,
    sync::Arc,
};
use url::Url;

pub(super) fn is_failover_redirect(url: &Url, patterns: &[Arc<Regex>]) -> bool {
    let redirect_url = url.as_str();
    patterns.iter().any(|pattern| pattern.is_match(redirect_url))
}

#[allow(clippy::too_many_lines)]
async fn get_remote_content_with_manual_redirects_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    max_redirects: usize,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    get_remote_content_with_manual_redirects_and_headers_and_options(
        app_config,
        client,
        input,
        headers,
        url,
        max_redirects,
        TextContentFetchOptions::with_request_options(options),
    )
    .await
    .map(|(content, response_url, _)| (content, response_url))
}

async fn get_remote_content_with_manual_redirects_and_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    max_redirects: usize,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let (max_attempts, backoff_ms, backoff_multiplier) = text_body_retry_values(app_config, options.body);
    let attempt_options = if max_attempts > 1 { options.request.without_resource_retries() } else { options.request };

    // Manual redirects retain their own credential-scrubbing loop inside each caller-owned logical attempt.
    for attempt in 0..max_attempts {
        let response = match send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
            app_config,
            client,
            input,
            headers,
            url,
            max_redirects,
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

/// Whether following a redirect from `from` to `to` moves the request from `https` to plain `http`.
pub(super) fn is_https_downgrade(from: &Url, to: &Url) -> bool {
    from.scheme().eq_ignore_ascii_case("https") && to.scheme().eq_ignore_ascii_case("http")
}

/// Refuses a redirect that downgrades `https` to `http` when `options` ask for it.
///
/// The message names the two schemes and hosts only, never the path or query, so a token carried in
/// either cannot reach a log through this error.
pub(super) fn reject_https_downgrade(from: &Url, to: &Url, options: RequestFetchOptions) -> Result<(), Error> {
    if options.rejects_https_downgrade() && is_https_downgrade(from, to) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "Refusing redirect from {}://{} to {}://{}: it would downgrade https to http",
                from.scheme(),
                from.host_str().unwrap_or("?"),
                to.scheme(),
                to.host_str().unwrap_or("?")
            ),
        ));
    }
    Ok(())
}

pub(super) fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

/// Reports whether a request header may be retained when the target origin changes.
pub fn is_safe_cross_origin_redirect_header(key: &str) -> bool {
    key.eq_ignore_ascii_case("accept")
        || key.eq_ignore_ascii_case("accept-encoding")
        || key.eq_ignore_ascii_case("accept-language")
        || key.eq_ignore_ascii_case("user-agent")
        || key.eq_ignore_ascii_case("range")
        || key.eq_ignore_ascii_case("if-range")
        || key.eq_ignore_ascii_case("icy-metadata")
}

pub(super) fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HashMap<String, String>) {
    headers.retain(|key, _| is_safe_cross_origin_redirect_header(key));
}

pub async fn download_text_content_with_manual_redirects(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
    max_redirects: usize,
) -> Result<(String, String), Error> {
    Box::pin(download_text_content_with_manual_redirects_and_options(
        app_config,
        client,
        input,
        headers,
        persist_filepath,
        trace_log,
        max_redirects,
        RequestFetchOptions::default(),
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn download_text_content_with_manual_redirects_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
    max_redirects: usize,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    let start_time = tokio::time::Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content(&file_path).await.map(|content| (content, url.to_string())),
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_with_manual_redirects_and_options(
                app_config,
                client,
                input,
                headers,
                &url,
                max_redirects,
                options,
            )
            .await
        };
        match result {
            Ok((content, response_url)) => {
                if persist_filepath.is_some() {
                    persist_file(persist_filepath, &content).await;
                }
                Ok((content, response_url))
            }
            Err(err) => Err(err),
        }
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    };

    let level = if trace_log { log::Level::Trace } else { log::Level::Debug };
    if log_enabled!(level) {
        if let Ok((_content, response_url)) = result.as_ref() {
            log::log!(
                level,
                "Request took: {} {}",
                format_elapsed_time(start_time.elapsed().as_secs()),
                sanitize_sensitive_info(response_url.as_str())
            );
        }
    }

    result
}

pub async fn download_text_content_with_manual_redirects_and_headers(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
    max_redirects: usize,
) -> Result<(String, String, HeaderMap), Error> {
    Box::pin(download_text_content_with_manual_redirects_and_headers_and_options(
        app_config,
        client,
        input,
        headers,
        trace_log,
        max_redirects,
        TextContentFetchOptions::default(),
    ))
    .await
}

pub async fn download_text_content_with_manual_redirects_and_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
    max_redirects: usize,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let start_time = tokio::time::Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => {
                    get_local_file_content(&file_path).await.map(|content| (content, url.to_string(), HeaderMap::new()))
                }
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_with_manual_redirects_and_headers_and_options(
                app_config,
                client,
                input,
                headers,
                &url,
                max_redirects,
                options,
            )
            .await
        };
        result
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    };

    let level = if trace_log { log::Level::Trace } else { log::Level::Debug };
    if log_enabled!(level) {
        if let Ok((_, response_url, _)) = result.as_ref() {
            log::log!(
                level,
                "Request took: {} {}",
                format_elapsed_time(start_time.elapsed().as_secs()),
                sanitize_sensitive_info(response_url.as_str())
            );
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{is_https_downgrade, reject_https_downgrade};
    use crate::utils::request::RequestFetchOptions;
    use std::io::ErrorKind;
    use url::Url;

    fn url(value: &str) -> Url { Url::parse(value).expect("test url parses") }

    #[test]
    fn only_https_to_http_is_a_downgrade() {
        assert!(is_https_downgrade(&url("https://a.example/x"), &url("http://a.example/x")));
        assert!(is_https_downgrade(&url("HTTPS://a.example/x"), &url("HTTP://b.example/y")));
        assert!(!is_https_downgrade(&url("http://a.example/x"), &url("https://a.example/x")));
        assert!(!is_https_downgrade(&url("http://a.example/x"), &url("http://b.example/x")));
        assert!(!is_https_downgrade(&url("https://a.example/x"), &url("https://b.example/x")));
    }

    #[test]
    fn the_default_options_still_follow_a_downgrade() {
        let result = reject_https_downgrade(
            &url("https://a.example/x"),
            &url("http://a.example/x"),
            RequestFetchOptions::default(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn a_downgrade_is_rejected_when_requested_and_the_message_hides_the_query() {
        let options = RequestFetchOptions::default().with_reject_https_downgrade(true);
        let error = reject_https_downgrade(
            &url("https://a.example/x?token=abc"),
            &url("http://b.example/y?token=abc"),
            options,
        )
        .expect_err("a downgrade must be refused");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        let message = error.to_string();
        assert!(message.contains("a.example") && message.contains("b.example"));
        assert!(!message.contains("abc") && !message.contains("/y"), "message leaked url detail: {message}");
    }

    #[test]
    fn an_upgrade_is_allowed_even_when_downgrades_are_rejected() {
        let options = RequestFetchOptions::default().with_reject_https_downgrade(true);
        assert!(reject_https_downgrade(&url("http://a.example/"), &url("https://a.example/"), options).is_ok());
    }
}
