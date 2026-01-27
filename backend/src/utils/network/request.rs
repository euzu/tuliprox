use crate::api::model::persist_pipe_stream::tee_dyn_reader;
use crate::api::model::AppState;
use crate::model::{format_elapsed_time, AppConfig, InputSource, ReverseProxyDisabledHeaderConfig};
use crate::model::{ConfigInput, ResourceRetryConfig};
use crate::utils::compression::compression_utils::{is_deflate, is_gzip};
use crate::utils::{async_file_reader, async_file_writer, debug_if_enabled};
use crate::utils::{get_file_path, persist_file};
use axum::http::header::RETRY_AFTER;
use futures::{StreamExt, TryStreamExt};
use log::{debug, error, log_enabled, trace, Level};
use reqwest::header::CONTENT_ENCODING;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode};
use shared::error::{notify_err_res, string_to_io_error, TuliproxError};
use shared::model::{InputFetchMethod, DEFAULT_USER_AGENT};
use shared::utils::{
    filter_request_header, human_readable_byte_size, sanitize_sensitive_info, ENCODING_DEFLATE,
    ENCODING_GZIP,
};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::io::StreamReader;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MimeCategory {
    Unknown,
    Video,
    M3U8,
    Image,
    Json,
    Xml,
    Text,
    Unclassified,
}

pub fn classify_content_type(headers: &[(String, String)]) -> MimeCategory {
    headers.iter()
        .find_map(|(k, v)| {
            (k == axum::http::header::CONTENT_TYPE.as_str()).then_some(v)
        })
        .map_or(MimeCategory::Unknown, |v| match v.to_lowercase().as_str() {
            v if v.starts_with("video/") || v == "application/octet-stream" => MimeCategory::Video,
            v if v.contains("mpegurl") => MimeCategory::M3U8,
            v if v.starts_with("image/") => MimeCategory::Image,
            v if v.starts_with("application/json") || v.ends_with("+json") => MimeCategory::Json,
            v if v.starts_with("application/xml") || v.ends_with("+xml") || v == "text/xml" => MimeCategory::Xml,
            v if v.starts_with("text/") => MimeCategory::Text,
            _ => MimeCategory::Unclassified,
        })
}

pub fn format_http_status(status: StatusCode) -> String {
    let code = status.as_u16();
    match status.canonical_reason() {
        Some(reason) => format!("{code} {reason}"),
        None => code.to_string(),
    }
}

pub fn content_type_from_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}


#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub fn calculate_retry_backoff(base_delay_ms: u64, multiplier: f64, attempt: u32) -> u64 {
    let base = base_delay_ms.max(1);
    if multiplier <= 1.0 {
        return base;
    }
    let delay = (base as f64) * multiplier.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
    if !delay.is_finite() || delay < 1.0 {
        base
    } else if delay >= u64::MAX as f64 {
        u64::MAX
    } else {
        delay as u64
    }
}

pub async fn send_with_retry(
    app_config: &Arc<AppConfig>,
    url: &Url,
    mut send: impl FnMut() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    let config = app_config.config.load();
    let (max_attempts, backoff_ms, backoff_multiplier) = config
        .reverse_proxy
        .as_ref()
        .map_or_else(
            ResourceRetryConfig::get_default_retry_values,
            |rp| rp.resource_retry.get_retry_values(),
        );
    drop(config);

    for attempt in 0..max_attempts {
        match send().send().await {
            Ok(response) => {
                let status = response.status();

                if status.is_success() {
                    return Ok(response);
                }

                let should_retry = status.is_server_error()
                    || matches!(
                        status,
                        StatusCode::REQUEST_TIMEOUT
                            | StatusCode::TOO_EARLY
                            | StatusCode::TOO_MANY_REQUESTS
                    );

                if attempt < max_attempts - 1 && should_retry {
                    let wait_dur = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map_or_else(
                            || {
                                let delay = calculate_retry_backoff(
                                    backoff_ms,
                                    backoff_multiplier,
                                    attempt,
                                );
                                Duration::from_millis(delay)
                            },
                            Duration::from_secs,
                        );

                    tokio::time::sleep(wait_dur).await;
                    continue;
                }

                return Err(string_to_io_error(format!(
                    "Request failed with status {} {}",
                    format_http_status(status),
                    sanitize_sensitive_info(url.as_str())
                )));
            }

            Err(err) => {
                if (err.is_timeout() || err.is_connect()) && attempt < max_attempts - 1 {
                    let delay = calculate_retry_backoff(
                        backoff_ms,
                        backoff_multiplier,
                        attempt,
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }

                error!(
                    "Received failure from server {}: {}",
                    sanitize_sensitive_info(url.as_str()),
                    sanitize_sensitive_info(err.to_string().as_str())
                );

                return Err(string_to_io_error(format!(
                    "Request failed: {} {}",
                    sanitize_sensitive_info(url.as_str()),
                    sanitize_sensitive_info(err.to_string().as_str())
                )));
            }
        }
    }

    Err(string_to_io_error(format!(
        "Failed to download file from {} after all retry attempts",
        sanitize_sensitive_info(url.as_str())
    )))
}

pub async fn get_input_epg_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    working_dir: &str,
    url_str: &str,
    persist_filepath: &Path,
) -> Result<PathBuf, TuliproxError> {
    debug_if_enabled!(
        "getting input epg content working_dir: {}, url: {}",
        working_dir,
        sanitize_sensitive_info(url_str)
    );
    if url_str.parse::<url::Url>().is_ok() {
        match download_epg_content_as_file(
            app_config,
            client,
            input,
            headers,
            url_str,
            persist_filepath,
        )
            .await
        {
            Ok(content) => Ok(content),
            Err(e) => {
                error!("can't download input {} epg url: {}  => {}", input.name, sanitize_sensitive_info(url_str), sanitize_sensitive_info(e.to_string().as_str()));
                notify_err_res!("Failed to download")
            }
        }
    } else {
        let result = match get_file_path(working_dir, Some(PathBuf::from(url_str))) {
            Some(filepath) => {
                if filepath.exists() {
                    if let Err(e) = tokio::fs::copy(&filepath, persist_filepath).await {
                        error!("can't persist to: {}  => {}", persist_filepath.display(), e);
                        return notify_err_res!("Failed to persist: {}  => {}", persist_filepath.display(), e);
                    }
                    if filepath.exists() {
                        Some(filepath)
                    } else {
                        return notify_err_res!("Failed: file does not exists {filepath:?}");
                    }
                } else {
                    None
                }
            }
            None => None,
        };

        result.map_or_else(|| {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            notify_err_res!("{msg}")
        }, Ok)
    }
}

pub async fn get_input_text_content(
    app_state: &Arc<AppState>,
    client: &reqwest::Client,
    input: &InputSource,
    working_dir: &str,
    persist_filepath: Option<PathBuf>,
) -> Result<String, TuliproxError> {
    debug_if_enabled!(
        "getting input text content working_dir: {}, url: {}",
        working_dir,
        sanitize_sensitive_info(&input.url)
    );

    if input.url.parse::<url::Url>().is_ok() {
        match download_text_content(
            &app_state.app_config,
            client,
            input,
            None,
            persist_filepath,
            false,
        )
            .await
        {
            Ok((content, _response_url)) => Ok(content),
            Err(e) => {
                error!("Failed to download input '{}': {}", &input.name, sanitize_sensitive_info(e.to_string().as_str()));
                notify_err_res!("Failed to download")
            }
        }
    } else {
        let result = match get_file_path(working_dir, Some(PathBuf::from(&input.url))) {
            Some(filepath) => {
                if filepath.exists() {
                    if let Some(persist_file_value) = persist_filepath {
                        let to_file = &persist_file_value;
                        if let Err(e) = tokio::fs::copy(&filepath, to_file).await {
                            error!("can't persist to: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                            return notify_err_res!("Failed to persist: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                        }
                    }

                    match get_local_file_content(&filepath).await {
                        Ok(content) => Some(content),
                        Err(err) => {
                            return notify_err_res!("Failed : {}", err);
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        result.map_or_else(|| {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(&input.url));
            error!("{msg}");
            notify_err_res!("{msg}")
        }, Ok)
    }
}

pub async fn get_input_text_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    working_dir: &str,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, TuliproxError> {
    debug_if_enabled!(
        "getting input text content working_dir: {}, url: {}",
        working_dir,
        sanitize_sensitive_info(&input.url)
    );

    if input.url.parse::<url::Url>().is_ok() {
        match download_text_content_as_stream(
            app_config,
            client,
            input,
            persist_filepath,
        )
            .await
        {
            Ok((content, _response_url)) => Ok(content),
            Err(e) => {
                error!(
                    "Failed to download input '{}': {}",
                    &input.name,
                    sanitize_sensitive_info(e.to_string().as_str())
                );
                notify_err_res!("Failed to download")
            }
        }
    } else {
        let result = match get_file_path(working_dir, Some(PathBuf::from(&input.url))) {
            Some(filepath) => {
                if filepath.exists() {
                    match get_local_file_content_as_stream(&filepath).await {
                        Ok(content) => {
                            if let Some(path) = persist_filepath {
                                let tee = tee_dyn_reader(
                                    content,
                                    &path,
                                    Some(Arc::new(|size| {
                                        debug_if_enabled!(
                                            "Persisted {} bytes",
                                            human_readable_byte_size(size as u64)
                                        );
                                    })),
                                )
                                    .await;
                                Some(tee)
                            } else {
                                Some(content)
                            }
                        }
                        Err(err) => {
                            return notify_err_res!("Failed : {}", err);
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        result.map_or_else(|| {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(&input.url));
            error!("{msg}");
            notify_err_res!("{msg}")
        }, Ok)
    }
}

pub fn get_client_request<S: ::std::hash::BuildHasher + Default>(
    client: &reqwest::Client,
    method: InputFetchMethod,
    headers: Option<&HashMap<String, String, S>>,
    url: &Url,
    custom_headers: Option<&HashMap<String, Vec<u8>, S>>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = match method {
        InputFetchMethod::GET => client.get(url.clone()),
        InputFetchMethod::POST => {
            // let base_url = url[..url::Position::BeforePath].to_string() + url.path();
            let mut params: HashMap<String, String, S> = HashMap::default();
            for (key, value) in url.query_pairs() {
                params.insert(key.to_string(), value.to_string());
            }
            // we could cut the params but we leave them as query and add them as form.
            client.post(url.clone()).form(&params)
        }
    };
    let headers = get_request_headers(
        headers,
        custom_headers,
        disabled_headers,
        default_user_agent,
    );
    request.headers(headers)
}

pub fn get_request_headers<S: ::std::hash::BuildHasher + Default>(
    request_headers: Option<&HashMap<String, String, S>>,
    custom_headers: Option<&HashMap<String, Vec<u8>, S>>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::default();
    let mut has_user_agent = false;

    // 1. First, we process the configured request headers (from input config).
    // These should have the highest priority.
    if let Some(req_headers) = request_headers {
        for (key, value) in req_headers {
            if let (Ok(key), Ok(value)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                if filter_request_header(key.as_str()) {
                    if disabled_headers
                        .as_ref()
                        .is_some_and(|d| d.should_remove(key.as_str()))
                    {
                        continue;
                    }
                    if key == axum::http::header::USER_AGENT {
                        has_user_agent = true;
                    }
                    headers.insert(key, value);
                }
            }
        }
    }

    // 2. Next, we process custom headers (from the client request).
    // These are only added if they don't already exist in the headers map (i.e., not overridden by config).
    if let Some(custom) = custom_headers {
        for (key, value) in custom {
            let key_lc = key.to_lowercase();
            if filter_request_header(key_lc.as_str()) {
                if disabled_headers
                    .as_ref()
                    .is_some_and(|d| d.should_remove(key_lc.as_str()))
                {
                    continue;
                }
                if let (Ok(name), Ok(val)) = (
                    HeaderName::from_bytes(key.as_bytes()),
                    HeaderValue::from_bytes(value),
                ) {
                    // Only insert if not already present (config takes precedence)
                    if !headers.contains_key(&name) {
                        if name == axum::http::header::USER_AGENT {
                            has_user_agent = true;
                        }
                        headers.insert(name, val);
                    }
                }
            }
        }
    }

    if log_enabled!(Level::Trace) {
        let he: HashMap<String, String> = headers
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    String::from_utf8_lossy(v.as_bytes()).to_string(),
                )
            })
            .collect();
        if !he.is_empty() {
            trace!("Request headers {he:?}");
        }
    }

    // 3. Finally, if no User-Agent was provided by config OR client, use the default.
    if !has_user_agent {
        let config_ua = default_user_agent
            .and_then(|ua| {
                let trimmed = ua.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .and_then(|ua| HeaderValue::from_str(ua).ok());

        headers.insert(
            axum::http::header::USER_AGENT,
            config_ua.unwrap_or_else(|| HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
    }

    headers
}

// read local file content and return it as a string.
// Gzipped file content is supported.
pub async fn get_local_file_content(file_path: &Path) -> Result<String, std::io::Error> {
    // open file
    let file = File::open(file_path).await.map_err(|err| {
        std::io::Error::new(
            ErrorKind::NotFound,
            format!("Failed to open file: {}, {err:?}", file_path.display()),
        )
    })?;

    let mut buf_reader = async_file_reader(file);

    // Peek first 2 bytes to detect gzip encoding
    let buffer = buf_reader.fill_buf().await?;
    let is_gzipped = buffer.len() >= 2 && is_gzip(&buffer[0..2]);

    let mut decoded = String::new();

    if is_gzipped {
        // Use async gzip decoder
        let mut gzip_decoder = async_compression::tokio::bufread::GzipDecoder::new(buf_reader);
        gzip_decoder
            .read_to_string(&mut decoded)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to decode gzip content: {e}")))?;
    } else {
        // read plaintext
        buf_reader
            .read_to_string(&mut decoded)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to read file: {e}")))?;
    }

    Ok(decoded)
}

pub async fn get_local_file_content_as_stream(
    file_path: &Path,
) -> Result<DynReader, std::io::Error> {
    // open file
    let file = File::open(file_path).await.map_err(|err| {
        std::io::Error::new(
            ErrorKind::NotFound,
            format!("Failed to open file: {}, {err:?}", file_path.display()),
        )
    })?;

    let mut buf_reader = async_file_reader(file);

    // Peek first 2 Bytes, for gzip detection
    let buffer = buf_reader.fill_buf().await?;
    let is_gzipped = buffer.len() >= 2 && is_gzip(&buffer[0..2]);

    if is_gzipped {
        // use Async Gzip Decoder
        Ok(Box::pin(
            async_compression::tokio::bufread::GzipDecoder::new(buf_reader),
        ))
    } else {
        Ok(Box::pin(buf_reader))
    }
}

pub async fn get_remote_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url: &Url,
    file_path: &Path,
) -> Result<PathBuf, std::io::Error> {
    let custom_headers = headers.map(|h| {
        h.iter()
            .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
            .collect::<HashMap<_, _>>()
    });

    let config = app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    drop(config);

    let response = send_with_retry(
        app_config,
        url,
        || {
            get_client_request(
                client,
                input.method,
                Some(&input.headers),
                url,
                custom_headers.as_ref(),
                None,
                default_user_agent.as_deref(),
            )
        },
    )
        .await?;

    let start_time = Instant::now();
    let mut writer = async_file_writer(File::create(file_path).await?);

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| {
            string_to_io_error(format!("Failed to read chunk: {e}"))
        })?;
        writer.write_all(&bytes).await?;
    }

    writer.flush().await?;
    writer.shutdown().await?;

    debug!(
        "File downloaded successfully to {}, took {}",
        file_path.display(),
        format_elapsed_time(start_time.elapsed().as_secs())
    );

    Ok(file_path.to_path_buf())
}

pub type DynReader = Pin<Box<dyn AsyncRead + Send>>;

async fn build_decoded_stream_reader(
    response: reqwest::Response,
) -> Result<DynReader, std::io::Error> {
    let headers = response.headers();
    let header_value = headers.get(CONTENT_ENCODING);
    let mut encoding = header_value
        .and_then(|h| h.to_str().ok())
        .map(ToString::to_string);

    let stream_reader =
        StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
    let mut buf_reader = async_file_reader(stream_reader);

    let peek = buf_reader.fill_buf().await?;

    if peek.len() >= 2 {
        if is_gzip(&peek[0..2]) {
            encoding = Some(ENCODING_GZIP.to_string());
        } else if is_deflate(&peek[0..2]) {
            encoding = Some(ENCODING_DEFLATE.to_string());
        }
    }

    let reader: DynReader = if encoding
        .as_ref()
        .is_some_and(|e| e.eq_ignore_ascii_case(ENCODING_GZIP))
    {
        Box::pin(async_compression::tokio::bufread::GzipDecoder::new(
            buf_reader,
        ))
    } else if encoding
        .as_ref()
        .is_some_and(|e| e.eq_ignore_ascii_case(ENCODING_DEFLATE))
    {
        Box::pin(async_compression::tokio::bufread::ZlibDecoder::new(
            buf_reader,
        ))
    } else {
        Box::pin(buf_reader)
    };

    Ok(reader)
}


#[allow(clippy::implicit_hasher)]
pub async fn get_remote_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
) -> Result<(DynReader, String), Error> {
    let custom_headers = headers.map(|h| {
        h.iter()
            .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
            .collect::<HashMap<_, _>>()
    });

    let config = app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    let disabled_headers = config.get_disabled_headers();
    drop(config);

    let merged = get_request_headers(
        Some(&input.headers),
        custom_headers.as_ref(),
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
    );

    let headers: HashMap<String, String> = merged
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect();

    let response = send_with_retry(
        app_config,
        url,
        || {
            get_client_request(
                client,
                input.method,
                Some(&headers),
                url,
                None,
                None,
                default_user_agent.as_deref(),
            )
        },
    )
        .await?;

    let response_url = response.url().to_string();

    let reader = build_decoded_stream_reader(response).await?;
    Ok((reader, response_url))
}

async fn get_remote_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
) -> Result<(String, String), Error> {
    let (mut stream, response_url) = get_remote_content_as_stream(
        app_config,
        client,
        input,
        headers,
        url,
    )
        .await
        .map_err(|e| string_to_io_error(format!("Failed to read content: {e}")))?;
    let mut content = String::new();
    stream
        .read_to_string(&mut content)
        .await
        .map_err(|e| string_to_io_error(format!("Failed to read content: {e}")))?;
    Ok((content, response_url))
}

async fn download_epg_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url_str: &str,
    persist_filepath: &Path,
) -> Result<PathBuf, Error> {
    if let Ok(url) = url_str.parse::<url::Url>() {
        if url.scheme() == "file" {
            url.to_file_path().map_or_else(
                |()| {
                    Err(Error::new(
                        ErrorKind::Unsupported,
                        format!("Unknown file {}", sanitize_sensitive_info(url_str)),
                    ))
                },
                |file_path| {
                    if file_path.exists() {
                        Ok(file_path)
                    } else {
                        Err(Error::new(
                            ErrorKind::NotFound,
                            format!("Unknown file {}", file_path.display()),
                        ))
                    }
                },
            )
        } else {
            get_remote_content_as_file(app_config, client, input, headers, &url, persist_filepath)
                .await
        }
    } else {
        Err(Error::new(
            ErrorKind::Unsupported,
            format!("Malformed URL {}", sanitize_sensitive_info(url_str)),
        ))
    }
}

pub async fn download_text_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<(String, String), Error> {
    let start_time = Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content(&file_path)
                    .await
                    .map(|c| (c, url.to_string())),
                Err(()) => Err(string_to_io_error(format!(
                    "Unknown file {}",
                    sanitize_sensitive_info(&input.url)
                ))),
            }
        } else {
            get_remote_content(
                app_config,
                client,
                input,
                headers,
                &url,
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
        Err(string_to_io_error(format!(
            "Malformed URL {}",
            sanitize_sensitive_info(&input.url)
        )))
    };

    let level = if trace_log {
        log::Level::Trace
    } else {
        log::Level::Debug
    };
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

pub async fn download_text_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<(DynReader, String), Error> {
    if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content_as_stream(&file_path)
                    .await
                    .map(|c| (c, url.to_string())),
                Err(()) => Err(string_to_io_error(format!(
                    "Unknown file {}",
                    sanitize_sensitive_info(&input.url)
                ))),
            }
        } else {
            get_remote_content_as_stream(
                app_config,
                client,
                input,
                None,
                &url,
            )
                .await
        };
        match result {
            Ok((content, response_url)) => {
                if let Some(path) = persist_filepath {
                    let tee_reader: DynReader = tee_dyn_reader(
                        content,
                        &path,
                        Some(Arc::new(|size| {
                            debug!("Persisted {size} bytes");
                        })),
                    )
                        .await;
                    Ok((tee_reader, response_url))
                } else {
                    Ok((content, response_url))
                }
            }
            Err(err) => Err(err),
        }
    } else {
        Err(string_to_io_error(format!(
            "Malformed URL {}",
            sanitize_sensitive_info(&input.url)
        )))
    }
}

async fn download_json_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<serde_json::Value, Error> {
    debug_if_enabled!(
        "Downloading json content from {}",
        sanitize_sensitive_info(&input.url)
    );
    match download_text_content(
        app_config,
        client,
        input,
        None,
        persist_filepath,
        trace_log,
    )
        .await
    {
        Ok((content, _response_url)) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(value) => Ok(value),
            Err(err) => Err(string_to_io_error(format!("Failed to parse json {err}"))),
        },
        Err(err) => Err(err),
    }
}

pub async fn get_input_json_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<serde_json::Value, TuliproxError> {
    match download_json_content(
        app_config,
        client,
        input,
        persist_filepath,
        trace_log,
    )
        .await
    {
        Ok(content) => Ok(content),
        Err(e) => notify_err_res!(
            "can't download input {}, => {}",
            input.name,
            sanitize_sensitive_info(e.to_string().as_str())
        ),
    }
}

async fn download_json_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, Error> {
    debug_if_enabled!(
        "Downloading json content as stream from {}",
        sanitize_sensitive_info(&input.url)
    );
    match download_text_content_as_stream(
        app_config,
        client,
        input,
        persist_filepath,
    )
        .await
    {
        Ok((reader, _response_url)) => Ok(reader),
        Err(err) => Err(err),
    }
}

pub async fn get_input_json_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, TuliproxError> {
    match download_json_content_as_stream(
        app_config,
        client,
        input,
        persist_filepath,
    )
        .await
    {
        Ok(stream) => Ok(stream),
        Err(e) => notify_err_res!(
            "can't download input {} => {}",
            input.name,
            sanitize_sensitive_info(e.to_string().as_str())
        ),
    }
}

pub fn create_client(cfg: &AppConfig) -> reqwest::ClientBuilder {
    let config = cfg.config.load();
    let mut client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .danger_accept_invalid_certs(config.accept_insecure_ssl_certificates);

    if let Some(proxy_cfg) = config.proxy.as_ref() {
        match Url::parse(&proxy_cfg.url) {
            Ok(mut url) => {
                let scheme = url.scheme().to_ascii_lowercase();

                match scheme.as_str() {
                    "socks5" | "socks5h" => {
                        if let Some(user) = &proxy_cfg.username {
                            let _ = url.set_username(user);
                        }
                        if let Some(pass) = &proxy_cfg.password {
                            let _ = url.set_password(Some(pass));
                        }
                        match reqwest::Proxy::all(url.as_str()) {
                            Ok(p) => {
                                client = client.proxy(p);
                            }
                            Err(err) => error!("Failed to create SOCKS proxy {url}: {err}"),
                        }
                    }
                    "http" | "https" => match reqwest::Proxy::all(url.as_str()) {
                        Ok(p) => {
                            if let (Some(username), Some(password)) =
                                (&proxy_cfg.username, &proxy_cfg.password)
                            {
                                client = client.proxy(p.basic_auth(username, password));
                            } else {
                                client = client.proxy(p);
                            }
                        }
                        Err(err) => error!("Failed to create HTTP proxy {url}: {err}"),
                    },
                    _ => {
                        error!("Unsupported proxy scheme '{scheme}' in URL: {url}");
                    }
                }
            }
            Err(e) => {
                error!("Invalid proxy URL '{}': {e}", &proxy_cfg.url);
            }
        }
    }

    if let Some(rp_config) = config.reverse_proxy.as_ref() {
        if rp_config
            .disabled_header
            .as_ref()
            .is_some_and(|d| d.referer_header)
        {
            client = client.referer(false);
        }
    }

    client
}

pub fn parse_range(range: &str) -> Option<(u64, Option<u64>)> {
    // expect: "bytes=START-END"
    if !range.starts_with("bytes=") {
        return None;
    }

    let range = &range[6..];
    let mut parts = range.split('-');

    let start = parts.next()?.parse().ok()?;
    let end = parts.next().and_then(|s| s.parse().ok());

    Some((start, end))
}

pub fn is_file_url(url: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|u| u.scheme().eq_ignore_ascii_case("file"))
}

pub fn is_uri(url: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|u| u.scheme().eq_ignore_ascii_case("file") || u.scheme().eq_ignore_ascii_case("http") || u.scheme().eq_ignore_ascii_case("https"))
}

#[cfg(test)]
mod tests {
    use shared::utils::{get_base_url_from_str, replace_url_extension, sanitize_sensitive_info};

    #[test]
    fn test_url_mask() {
        // Replace with "***"
        let query = "https://bubblegum.tv/live/username/password/2344";
        let masked = sanitize_sensitive_info(query);
        println!("{masked}");
    }

    #[test]
    fn test_replace_ext() {
        let tests = [
            ("http://hello.world.com", "http://hello.world.com"),
            (
                "http://hello.world.com/123",
                "http://hello.world.com/123.mp4",
            ),
            (
                "http://hello.world.com/123.ts?hello=world",
                "http://hello.world.com/123.mp4?hello=world",
            ),
            (
                "http://hello.world.com/123?hello=world",
                "http://hello.world.com/123.mp4?hello=world",
            ),
            (
                "http://hello.world.com/123#hello=world",
                "http://hello.world.com/123.mp4#hello=world",
            ),
        ];

        for (test, expect) in &tests {
            assert_eq!(replace_url_extension(test, ".mp4"), *expect);
        }
    }

    #[test]
    fn tes_base_url() {
        let url = "http://my.provider.com:8080/xmltv?username=hello";
        let expected = "http://my.provider.com:8080";
        assert_eq!(get_base_url_from_str(url).unwrap(), expected);
    }

    #[test]
    fn test_get_request_headers_prioritization() {
        use super::{get_request_headers, DEFAULT_USER_AGENT};
        use axum::http::header::USER_AGENT;
        use std::collections::HashMap;

        // Case 1: No headers provided -> Default UA
        let headers =
            get_request_headers::<std::collections::hash_map::RandomState>(None, None, None, None);
        assert_eq!(headers.get(USER_AGENT).unwrap(), DEFAULT_USER_AGENT);

        // Case 2: No headers provided but config default UA set -> Config default UA
        let headers = get_request_headers::<std::collections::hash_map::RandomState>(
            None,
            None,
            None,
            Some("Config-Default-UA"),
        );
        assert_eq!(headers.get(USER_AGENT).unwrap(), "Config-Default-UA");

        // Case 3: Only client header -> Client UA (overrides config default UA)
        let mut client_headers = HashMap::new();
        client_headers.insert("User-Agent".to_string(), b"Client-UA".to_vec());
        let headers =
            get_request_headers(None, Some(&client_headers), None, Some("Config-Default-UA"));
        assert_eq!(headers.get(USER_AGENT).unwrap(), "Client-UA");

        // Case 4: Both config and client -> Config UA overrides
        let mut config_headers = HashMap::new();
        config_headers.insert("User-Agent".to_string(), "Config-UA".to_string());
        let headers = get_request_headers(
            Some(&config_headers),
            Some(&client_headers),
            None,
            Some("Config-Default-UA"),
        );
        assert_eq!(headers.get(USER_AGENT).unwrap(), "Config-UA");

        // Case 5: Other headers also prioritized
        config_headers.insert("X-Test".to_string(), "From-Config".to_string());
        let mut client_headers = HashMap::new();
        client_headers.insert("X-Test".to_string(), b"From-Client".to_vec());
        let headers = get_request_headers(
            Some(&config_headers),
            Some(&client_headers),
            None,
            Some("Config-Default-UA"),
        );
        assert_eq!(headers.get("X-Test").unwrap(), "From-Config");
    }
}
