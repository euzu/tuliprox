use super::{
    classify_host, get_remote_content_as_stream, get_remote_content_with_headers_and_options,
    get_remote_content_with_options,
    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result, DynReader, InputEpgFileRequest,
    RecordingTaskOptions, RequestFetchOptions, ResourceDestination, ResourceRetryExecution, TextContentBodyOptions,
    TextContentFetchOptions, TextContentRetryOwner, STREAM_IDLE_TIMEOUT,
};
use crate::{
    model::{AppConfig, ConfigInput, InputSource},
    utils::{
        async_file_reader, async_file_writer,
        compression::compression_utils::is_gzip,
        content_coding::{ContentCodingDetection, OutboundContentCodingPolicy},
        debug_if_enabled, get_file_path,
        network::persist_pipe::tee_dyn_reader,
        persist_file,
    },
};
use futures::StreamExt;
use log::{debug, error, log_enabled, warn};
use reqwest::header::HeaderMap;
use shared::{
    error::{string_to_io_error, TuliproxError},
    model::format_elapsed_time,
    utils::{human_readable_byte_size, sanitize_sensitive_info},
};
use std::{
    io::{Error, ErrorKind},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    time::sleep,
};
use url::Url;

/// Classifies a resource destination given as host name or IP literal, bracketed IPv6 included.
///
/// An IP literal is decided by its address alone, without resolving anything. Names are resolved once
/// per verdict lifetime and remembered in the process-wide memo. A name that cannot be resolved is
/// reported as [`ResourceDestination::Private`]: callers must not hand a possibly internal name to a
/// client, and the fetch attempt itself decides whether the destination is reachable.
pub async fn classify_resource_destination(host: &str) -> ResourceDestination { classify_host(host).await.0 }

impl RequestFetchOptions {
    pub fn with_attempt_idle_timeout(timeout: Duration) -> Self {
        Self { attempt_idle_timeout: Some(timeout.max(Duration::from_millis(1))), ..Self::default() }
    }

    pub const fn with_content_coding(mut self, content_coding: OutboundContentCodingPolicy) -> Self {
        self.content_coding = content_coding;
        self
    }

    /// Returns the final HTTP error response after the configured retry and failover policy.
    pub const fn with_http_error_responses(mut self, enabled: bool) -> Self {
        self.return_http_errors = enabled;
        self
    }

    /// Leaves bounded provider failover intact while assigning retry rounds to the caller.
    pub const fn without_resource_retries(mut self) -> Self {
        self.resource_retry = ResourceRetryExecution::ProviderFailoverOnly;
        self
    }

    pub(super) fn attempt_idle_timeout_or_default(self) -> Duration {
        self.attempt_idle_timeout.unwrap_or_else(|| Duration::from_secs(STREAM_IDLE_TIMEOUT))
    }

    pub(super) const fn uses_provider_failover_only(self) -> bool {
        matches!(self.resource_retry, ResourceRetryExecution::ProviderFailoverOnly)
    }
}

pub async fn get_input_epg_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    request: InputEpgFileRequest<'_>,
) -> Result<PathBuf, TuliproxError> {
    let InputEpgFileRequest { headers, storage_dir, url: url_str, persist_path, max_bytes } = request;
    debug_if_enabled!(
        "getting input epg content storage_dir: {}, url: {}",
        storage_dir,
        sanitize_sensitive_info(url_str)
    );

    // This is the single write-lock boundary for EPG cache population. Callers must
    // not hold a lock for `persist_path` while invoking this function.
    let _persist_lock = app_config.file_locks.write_lock(persist_path).await;

    // On Windows, drive-letter paths also parse as URLs (with `c` as the scheme).
    // Interpret an absolute platform path before attempting URL parsing.
    if !Path::new(url_str).is_absolute() && url_str.parse::<url::Url>().is_ok() {
        match download_epg_content_as_file(app_config, client, input, headers, url_str, persist_path, max_bytes).await {
            Ok(content) => Ok(content),
            Err(e) => {
                error!(
                    "can't download input {} epg url: {}  => {}",
                    input.name,
                    sanitize_sensitive_info(url_str),
                    sanitize_sensitive_info(&e.to_string())
                );
                Err(TuliproxError::RepositoryNetwork(format!(
                    "can't download input {} epg url: {}  => {}",
                    input.name,
                    sanitize_sensitive_info(url_str),
                    sanitize_sensitive_info(&e.to_string())
                )))
            }
        }
    } else {
        let Some(file_path) = get_file_path(storage_dir, Some(PathBuf::from(url_str))) else {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            return Err(TuliproxError::RepositoryNetwork(msg));
        };
        if !file_path.exists() {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            return Err(TuliproxError::RepositoryNetwork(msg));
        }

        copy_local_epg_file_to_persist(&file_path, persist_path, max_bytes).await.map_err(|err| {
            error!("can't persist to: {}  => {}", persist_path.display(), err);
            TuliproxError::RepositoryNetwork(format!("Failed to persist: {}  => {err}", persist_path.display()))
        })
    }
}

pub async fn get_input_text_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    storage_dir: &str,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, TuliproxError> {
    debug_if_enabled!(
        "getting input text content storage_dir: {}, url: {}",
        storage_dir,
        sanitize_sensitive_info(&input.url)
    );

    if input.url.parse::<url::Url>().is_ok() {
        match download_text_content_as_stream(app_config, client, input, persist_filepath).await {
            Ok((content, _response_url)) => Ok(content),
            Err(e) => {
                error!("Failed to download input '{}': {}", input.name, sanitize_sensitive_info(&e.to_string()));
                Err(TuliproxError::RepositoryNetwork(format!(
                    "Failed to download input '{}': {}",
                    input.name,
                    sanitize_sensitive_info(&e.to_string())
                )))
            }
        }
    } else {
        let result = match get_file_path(storage_dir, Some(PathBuf::from(&input.url))) {
            Some(filepath) => {
                if filepath.exists() {
                    match get_local_file_content_as_stream(&filepath).await {
                        Ok(content) => {
                            if let Some(path) = persist_filepath {
                                let tee = tee_dyn_reader(
                                    content,
                                    &path,
                                    Some(Arc::new(|size| {
                                        debug_if_enabled!("Persisted {} bytes", human_readable_byte_size(size as u64));
                                    })),
                                )
                                .await;
                                Some(tee)
                            } else {
                                Some(content)
                            }
                        }
                        Err(err) => {
                            return Err(TuliproxError::RepositoryNetwork(format!("Failed : {err}")));
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        result.map_or_else(
            || {
                let msg = format!("can't read input url: {}", sanitize_sensitive_info(&input.url));
                error!("{msg}");
                Err(TuliproxError::RepositoryNetwork(msg))
            },
            Ok,
        )
    }
}

// read local file content and return it as a string.
// Gzipped file content is supported.
pub async fn get_local_file_content(file_path: &Path) -> Result<String, std::io::Error> {
    // open file
    let file = File::open(file_path).await.map_err(|err| {
        std::io::Error::new(ErrorKind::NotFound, format!("Failed to open file: {}, {err:?}", file_path.display()))
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

pub async fn get_local_file_content_as_stream(file_path: &Path) -> Result<DynReader, std::io::Error> {
    // open file
    let file = File::open(file_path).await.map_err(|err| {
        std::io::Error::new(ErrorKind::NotFound, format!("Failed to open file: {}, {err:?}", file_path.display()))
    })?;

    let mut buf_reader = async_file_reader(file);

    // Peek first 2 Bytes, for gzip detection
    let buffer = buf_reader.fill_buf().await?;
    let is_gzipped = buffer.len() >= 2 && is_gzip(&buffer[0..2]);

    if is_gzipped {
        // use Async Gzip Decoder
        Ok(Box::pin(async_compression::tokio::bufread::GzipDecoder::new(buf_reader)))
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
    get_remote_content_as_file_with_options(
        app_config,
        client,
        input,
        headers,
        url,
        file_path,
        RecordingTaskOptions::default(),
    )
    .await
}

pub async fn get_remote_content_as_file_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url: &Url,
    file_path: &Path,
    options: RecordingTaskOptions,
) -> Result<PathBuf, std::io::Error> {
    let input_source = InputSource {
        name: input.name.clone(),
        url: url.to_string(),
        provider: input.get_resolve_provider(url.as_str()),
        username: input.username.clone(),
        password: input.password.clone(),
        method: input.method,
        headers: input.headers.clone(),
    };

    let response = send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
        app_config,
        client,
        &input_source,
        headers,
        url,
        10,
        RequestFetchOptions::default(),
    )
    .await?
    .response;

    let start_time = tokio::time::Instant::now();
    let (temp_file, output_file) = if options.atomic_write {
        let (temp_file, output_file) = create_atomic_download_file(file_path)?;
        (Some(temp_file), output_file)
    } else {
        (None, File::create(file_path).await?)
    };
    let mut writer = async_file_writer(output_file);

    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;

    let idle_timeout = tokio::time::Duration::from_secs(STREAM_IDLE_TIMEOUT);
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    loop {
        tokio::select! {
        () = &mut idle => {
            warn!("Stream idle for request, closing {}", sanitize_sensitive_info(url.as_ref()));
            return Err(string_to_io_error(format!(
                "Download timed out for {}",
                sanitize_sensitive_info(url.as_ref())
            )));
        }

        chunk = stream.next() => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                match chunk {
                    Some(Ok(bytes)) => {
                        downloaded = downloaded.checked_add(bytes.len() as u64).ok_or_else(|| {
                            string_to_io_error(format!(
                                "Download size overflow for {}",
                                sanitize_sensitive_info(url.as_ref())
                            ))
                        })?;
                        if options.max_bytes.is_some_and(|max| downloaded > max) {
                            return Err(string_to_io_error(format!(
                                "Download exceeds configured limit for {}",
                                sanitize_sensitive_info(url.as_ref())
                            )));
                        }
                        writer.write_all(&bytes).await?;
                    }
                    Some(Err(e)) => {
                        return Err(string_to_io_error(format!("Failed to read chunk: {e}")));
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    }

    writer.flush().await?;
    writer.shutdown().await?;
    drop(writer);

    if let Some(temp_file) = temp_file {
        persist_atomic_download_file(temp_file, file_path)?;
    }

    debug!(
        "File downloaded successfully to {}, took {}",
        file_path.display(),
        format_elapsed_time(start_time.elapsed().as_secs())
    );

    Ok(file_path.to_path_buf())
}

impl TextContentBodyOptions {
    /// Selects the narrowly scoped HLS-manifest fallback detection and decoded-size limit.
    pub fn hls_manifest(max_decoded_bytes: usize, deadline: Duration) -> Self {
        Self {
            detection: ContentCodingDetection::DeclaredOrKnownHlsManifestMagic,
            max_decoded_bytes: Some(max_decoded_bytes),
            deadline: Some(deadline.max(Duration::from_millis(1))),
            retry_owner: TextContentRetryOwner::DecodedBodyConsumer,
        }
    }

    fn legacy_text_with_deadline(deadline: Option<Duration>) -> Self {
        Self { deadline: deadline.map(|value| value.max(Duration::from_millis(1))), ..Self::default() }
    }
}

impl TextContentFetchOptions {
    pub const fn new(request: RequestFetchOptions, body: TextContentBodyOptions) -> Self { Self { request, body } }

    pub(super) fn with_request_options(request: RequestFetchOptions) -> Self {
        Self { body: TextContentBodyOptions::legacy_text_with_deadline(request.attempt_idle_timeout), request }
    }
}

pub(super) fn create_atomic_download_file(file_path: &Path) -> Result<(tempfile::NamedTempFile, File), Error> {
    let parent = file_path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let temp_file = tempfile::Builder::new().prefix(".tuliprox-download-").suffix(".tmp").tempfile_in(parent)?;
    let output_file = File::from_std(temp_file.reopen()?);
    Ok((temp_file, output_file))
}

fn persist_atomic_download_file(temp_file: tempfile::NamedTempFile, file_path: &Path) -> Result<(), Error> {
    match temp_file.persist(file_path) {
        Ok(_) => Ok(()),
        Err(err) => Err(err.error),
    }
}

async fn copy_local_epg_file_to_persist(
    file_path: &Path,
    persist_filepath: &Path,
    max_bytes: Option<u64>,
) -> Result<PathBuf, Error> {
    let mut reader = File::open(file_path).await?;
    let (temp_file, output_file) = create_atomic_download_file(persist_filepath)?;
    let mut writer = async_file_writer(output_file);
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| string_to_io_error(format!("Local EPG file size overflow for {}", file_path.display())))?;
        if max_bytes.is_some_and(|max| copied > max) {
            return Err(string_to_io_error(format!("Local EPG file {} exceeds configured limit", file_path.display())));
        }
        writer.write_all(&buffer[..read]).await?;
    }

    writer.flush().await?;
    writer.shutdown().await?;
    drop(writer);
    drop(reader);
    persist_atomic_download_file(temp_file, persist_filepath)?;
    Ok(persist_filepath.to_path_buf())
}

async fn download_epg_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url_str: &str,
    persist_filepath: &Path,
    max_bytes: Option<u64>,
) -> Result<PathBuf, Error> {
    if let Ok(url) = url_str.parse::<url::Url>() {
        match url.scheme() {
            "file" => {
                let file_path = url.to_file_path().map_err(|()| {
                    Error::new(ErrorKind::Unsupported, format!("Unknown file {}", sanitize_sensitive_info(url_str)))
                })?;
                if file_path.exists() {
                    copy_local_epg_file_to_persist(&file_path, persist_filepath, max_bytes).await
                } else {
                    Err(Error::new(ErrorKind::NotFound, format!("Unknown file {}", file_path.display())))
                }
            }
            "http" | "https" | "provider" => {
                get_remote_content_as_file_with_options(
                    app_config,
                    client,
                    input,
                    headers,
                    &url,
                    persist_filepath,
                    RecordingTaskOptions { max_bytes, atomic_write: true },
                )
                .await
            }
            scheme => Err(Error::new(
                ErrorKind::Unsupported,
                format!("Unsupported EPG URL scheme '{scheme}' for {}", sanitize_sensitive_info(url_str)),
            )),
        }
    } else {
        Err(Error::new(ErrorKind::Unsupported, format!("Malformed URL {}", sanitize_sensitive_info(url_str))))
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
    Box::pin(download_text_content_with_options(
        app_config,
        client,
        input,
        headers,
        persist_filepath,
        trace_log,
        RequestFetchOptions::default(),
    ))
    .await
}

pub async fn download_text_content_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
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
            get_remote_content_with_options(app_config, client, input, headers, &url, options).await
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

pub async fn download_text_content_with_headers(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
) -> Result<(String, String, HeaderMap), Error> {
    Box::pin(download_text_content_with_headers_and_options(
        app_config,
        client,
        input,
        headers,
        trace_log,
        TextContentFetchOptions::default(),
    ))
    .await
}

pub async fn download_text_content_with_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
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
            get_remote_content_with_headers_and_options(app_config, client, input, headers, &url, options).await
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

pub async fn download_text_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<(DynReader, String), Error> {
    if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content_as_stream(&file_path).await.map(|c| (c, url.to_string())),
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_as_stream(app_config, client, input, None, &url).await
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
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    }
}

async fn download_json_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<serde_json::Value, Error> {
    debug_if_enabled!("Downloading json content from {}", sanitize_sensitive_info(&input.url));
    match download_text_content(app_config, client, input, None, persist_filepath, trace_log).await {
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
    match download_json_content(app_config, client, input, persist_filepath, trace_log).await {
        Ok(content) => Ok(content),
        Err(e) => Err(TuliproxError::RepositoryNetwork(format!(
            "can't download input {input} => {sanitized}",
            input = input.name,
            sanitized = sanitize_sensitive_info(&e.to_string())
        ))),
    }
}

async fn download_json_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, Error> {
    debug_if_enabled!("Downloading json content as stream from {}", sanitize_sensitive_info(&input.url));
    match download_text_content_as_stream(app_config, client, input, persist_filepath).await {
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
    match download_json_content_as_stream(app_config, client, input, persist_filepath).await {
        Ok(stream) => Ok(stream),
        Err(e) => Err(TuliproxError::RepositoryNetwork(format!(
            "can't download input {input} => {sanitized}",
            input = input.name,
            sanitized = sanitize_sensitive_info(&e.to_string())
        ))),
    }
}
