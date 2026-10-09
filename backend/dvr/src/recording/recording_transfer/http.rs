use super::{
    classify_download_open_error, classify_download_stream_io_error, current_download_control, handle_download_control,
    handle_download_control_without_writer, publish_recording_change, publish_recording_progress,
    update_active_download_for_worker, DownloadExecutionResult, QuotaGate, DOWNLOAD_CONTROL_POLL_INTERVAL,
    DOWNLOAD_PROGRESS_LOG_BYTES, DOWNLOAD_PROGRESS_LOG_INTERVAL, DOWNLOAD_SNAPSHOT_UPDATE_BYTES,
    DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL, QUOTA_EXCEEDED_DURING_TRANSFER, RANGE_UNSUPPORTED_ERROR,
};
use crate::{
    http_transfer::{
        compute_total_size, is_retryable_status, validate_resume_response, ResponseSnapshot, ResumeValidationError,
        ResumeValidator,
    },
    recording::{
        recording_queue::{RecordingControl, RecordingTask},
        recording_url::build_stable_recording_url,
        recording_worker::recording_partial_path,
    },
};
use futures::stream::TryStreamExt;
use log::{debug, info, warn};
use shared::{error::to_io_error, model::EventSink, utils::bytes_to_megabytes};
use std::sync::Arc;
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{Notify, RwLock},
    time,
    time::Instant,
};
use tuliprox_core::{
    model::AppConfig,
    utils::{async_file_writer, IO_BUFFER_SIZE},
};

pub(super) fn recording_execution_download(
    app_config: &AppConfig,
    task: &RecordingTask,
    provider_handle: Option<&tuliprox_core::model::ProviderHandle>,
) -> Result<RecordingTask, String> {
    let source = &task.recording.source;
    let virtual_id = source.virtual_id.parse::<u32>().map_err(|_| "Recording source virtual id invalid".to_string())?;
    let url = build_stable_recording_url(
        app_config,
        &source.target_id,
        &source.input_name,
        virtual_id,
        source.cluster,
        provider_handle.map(|handle| handle.allocation_id),
    )
    .ok_or_else(|| "Recording execution URL unavailable".to_string())?;
    let mut execution = task.clone();
    execution.url = reqwest::Url::parse(&url).map_err(|_| "Recording execution URL invalid".to_string())?;
    Ok(execution)
}

/// The validators a later resume can be checked against.
///
/// A weak `ETag` is discarded: it only promises semantic equivalence, so it
/// cannot prove the bytes on the far side of an interruption are the same
/// ones. `Last-Modified` is the fallback, and is only meaningful when no
/// strong tag was offered.
fn capture_resume_validators(response: &reqwest::Response) -> (Option<String>, Option<String>) {
    let headers = response.headers();
    let etag = headers
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::http_transfer::strong_etag)
        .map(str::to_owned);
    if etag.is_some() {
        return (etag, None);
    }
    let last_modified = headers
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    (None, last_modified)
}

pub(super) async fn send_download_request(
    client: &reqwest::Client,
    upstream_client: Option<&reqwest::Client>,
    url: &reqwest::Url,
    offset: u64,
    control_signal: &RwLock<RecordingControl>,
    control_notify: &Notify,
) -> Result<reqwest::Response, DownloadExecutionResult> {
    if let Some(result) = handle_download_control_without_writer(current_download_control(control_signal)) {
        return Err(result);
    }

    let send = async {
        let mut destination = url.clone();
        let mut upstream_origin = None;
        for redirects in 0..=10 {
            let hop_client =
                if destination.origin() == url.origin() { client } else { upstream_client.unwrap_or(client) };
            // Resume offsets refer to bytes on disk, so every hop must preserve encoding.
            let mut request = hop_client
                .get(destination.clone())
                .header(reqwest::header::USER_AGENT, shared::model::RECORDING_STREAM_USER_AGENT)
                .header(reqwest::header::ACCEPT_ENCODING, "identity");
            if offset > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            }
            if destination.origin() != url.origin() {
                let trusted_origin = upstream_origin.get_or_insert_with(|| destination.origin());
                if upstream_client.is_none() || destination.origin() != *trusted_origin {
                    // Listener credentials stay local; provider credentials stay
                    // on the first upstream origin throughout the redirect chain.
                    request = request.header(reqwest::header::AUTHORIZATION, "").header(reqwest::header::COOKIE, "");
                }
            }
            let response = request.send().await.map_err(|error| classify_download_open_error(&destination, &error))?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                return Ok(response);
            };
            let next = location
                .to_str()
                .ok()
                .and_then(|value| response.url().join(value).ok())
                .filter(|value| matches!(value.scheme(), "http" | "https"));
            let Some(next) = next else {
                return Err(DownloadExecutionResult::Failed("Invalid recording redirect location".to_string()));
            };
            if redirects == 10 {
                return Err(DownloadExecutionResult::Failed("Recording redirect limit exceeded".to_string()));
            }
            destination = next;
        }
        Err(DownloadExecutionResult::Failed("Recording redirect limit exceeded".to_string()))
    };
    tokio::pin!(send);
    loop {
        tokio::select! {
            biased;
            () = control_notify.notified() => {
                if let Some(result) = handle_download_control_without_writer(*control_signal.read().await) {
                    return Err(result);
                }
            }
            response = &mut send => return response,
        }
    }
}

pub(super) fn http_transfer_path(task: &RecordingTask) -> std::path::PathBuf {
    if task.kind.is_resumable() {
        recording_partial_path(&task.file_path)
    } else {
        task.file_path.clone()
    }
}

/// Publish a completed transfer at its final path.
///
/// Idempotent: linking is the only step that is not naturally repeatable. A
/// crash between the link and the partial's removal leaves the final file in
/// place, and finalizing again accepts it instead of failing the recording.
pub(super) async fn finalize_http_transfer(
    final_path: &std::path::Path,
    transfer_path: &std::path::Path,
) -> std::io::Result<()> {
    if transfer_path == final_path {
        return Ok(());
    }
    fs::File::open(transfer_path).await?.sync_all().await?;
    match fs::hard_link(transfer_path, final_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Path reservation gives this recording sole claim on `final_path`,
            // so something already there is this task's own earlier link. A
            // size mismatch means that reasoning does not hold; say so rather
            // than publish bytes that were never checked.
            let published = fs::metadata(final_path).await?.len();
            let staged = fs::metadata(transfer_path).await?.len();
            if published != staged {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already exists with {published} bytes, but the transfer staged {staged}",
                        final_path.display()
                    ),
                ));
            }
        }
        Err(error) => return Err(error),
    }
    if let Err(error) = fs::remove_file(transfer_path).await {
        warn!("Finalized {} but could not remove partial {}: {error}", final_path.display(), transfer_path.display());
    }
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) async fn download_file<E: EventSink>(
    active: Arc<RwLock<Vec<RecordingTask>>>,
    file_download: RecordingTask,
    client: &reqwest::Client,
    upstream_client: Option<&reqwest::Client>,
    control_signal: Arc<RwLock<RecordingControl>>,
    control_notify: Arc<Notify>,
    event_manager: Option<&E>,
    quota_gate: Option<QuotaGate<'_>>,
) -> DownloadExecutionResult {
    let worker_uuid = file_download.uuid.as_str();
    let url = file_download.url.clone();
    let file_path = http_transfer_path(&file_download);
    let existing_size = tokio::fs::metadata(&file_path).await.map_or(0, |metadata| metadata.len());
    let response_result =
        send_download_request(client, upstream_client, &url, existing_size, &control_signal, &control_notify).await;

    match response_result {
        Ok(response) => {
            if existing_size > 0 {
                // The validators were captured when the first byte was written.
                // Without them a provider that replaces the file between an
                // interruption and the resume returns a well-formed 206 whose
                // bytes belong to a different resource, and they are appended
                // to the old partial.
                let validator = ResumeValidator {
                    expected_offset: existing_size,
                    expected_total: file_download.total_size,
                    expected_etag: file_download.recording.resume_etag.clone(),
                    expected_last_modified: file_download.recording.resume_last_modified.clone(),
                };
                match validate_resume_response(&ResponseSnapshot::from_response(&response), &validator) {
                    Ok(()) => {}
                    Err(ResumeValidationError::Unsatisfiable { complete: true }) => {
                        return match finalize_http_transfer(&file_download.file_path, &file_path).await {
                            Ok(()) => DownloadExecutionResult::Completed,
                            Err(error) => DownloadExecutionResult::Failed(format!(
                                "Could not finalize {}: {error}",
                                file_download.file_path.display()
                            )),
                        };
                    }
                    Err(ResumeValidationError::IgnoredRange) => {
                        return DownloadExecutionResult::Failed(RANGE_UNSUPPORTED_ERROR.to_string());
                    }
                    Err(error) => {
                        return DownloadExecutionResult::Failed(format!(
                            "Cannot resume download at byte {existing_size} for {url}: {error}; partial file preserved"
                        ));
                    }
                }
            }
            let status = response.status();
            if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                if is_retryable_status(status) {
                    return DownloadExecutionResult::Retryable(format!(
                        "Download request failed for {url} with transient HTTP {status}"
                    ));
                }
                return DownloadExecutionResult::Failed(format!(
                    "Download request failed for {url} with HTTP {status}"
                ));
            }
            let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

            let total_size = compute_total_size(&response, existing_size);
            let captured = capture_resume_validators(&response);

            if total_size.is_some() || existing_size == 0 {
                update_active_download_for_worker(&active, worker_uuid, |download| {
                    if let Some(total) = total_size {
                        download.total_size = Some(total);
                    }
                    // Only a fresh transfer may set the validators; a resume
                    // must keep the ones its partial was written against.
                    if existing_size == 0 {
                        download.recording.resume_etag.clone_from(&captured.0);
                        download.recording.resume_last_modified.clone_from(&captured.1);
                    }
                    true
                })
                .await;
            }

            // The size is known from here on, or known to be unknown.
            let byte_cap = match quota_gate {
                Some(gate) => match gate.admit_size(worker_uuid, total_size, existing_size).await {
                    Ok(admitted) => {
                        if admitted.siblings_refused {
                            if let Some(event_manager) = event_manager {
                                publish_recording_change(event_manager);
                            }
                        }
                        admitted.byte_cap
                    }
                    Err(reason) => return DownloadExecutionResult::Failed(reason),
                },
                None => None,
            };

            match fs::create_dir_all(&file_download.file_dir).await {
                Ok(()) => {
                    let mut open_options = tokio::fs::OpenOptions::new();
                    let file_mode = if existing_size > 0 && is_resume {
                        open_options.append(true)
                    } else {
                        open_options.write(true).create(true).truncate(true)
                    };

                    if let Some(file_path_str) = file_path.to_str() {
                        info!("{} {}", if is_resume { "Resuming" } else { "Downloading" }, file_path_str);
                        match file_mode.open(&file_path).await {
                            Ok(file) => {
                                let mut buf_writer = async_file_writer(file);
                                let mut downloaded: u64 = if is_resume { existing_size } else { 0 };
                                let mut stream = response.bytes_stream();
                                let mut write_counter = 0;
                                let mut saw_first_chunk = existing_size > 0;
                                let mut last_progress_log_at = Instant::now();
                                let mut last_progress_logged_bytes = downloaded;
                                let mut last_snapshot_update_at = Instant::now();
                                let mut last_snapshot_update_bytes = downloaded;
                                let mut control_poll = time::interval(DOWNLOAD_CONTROL_POLL_INTERVAL);
                                control_poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                                control_poll.tick().await;
                                loop {
                                    let next_item = tokio::select! {
                                        biased;
                                        () = control_notify.notified() => {
                                            if let Some(result) =
                                                handle_download_control(*control_signal.read().await, &mut buf_writer)
                                                    .await
                                            {
                                                return result;
                                            }
                                            continue;
                                        }
                                        _ = control_poll.tick() => {
                                            if let Some(result) = handle_download_control(
                                                current_download_control(&control_signal),
                                                &mut buf_writer,
                                            )
                                            .await
                                            {
                                                return result;
                                            }
                                            continue;
                                        }
                                        next_item = stream.try_next() => next_item.map_err(to_io_error),
                                    };

                                    match next_item {
                                        Ok(item) => {
                                            if let Some(chunk) = item {
                                                match buf_writer.write_all(&chunk).await {
                                                    Ok(()) => {
                                                        write_counter += chunk.len();
                                                        if write_counter >= IO_BUFFER_SIZE {
                                                            if let Err(err) = buf_writer.flush().await {
                                                                return DownloadExecutionResult::Failed(
                                                                    err.to_string(),
                                                                );
                                                            }
                                                            write_counter = 0;
                                                        }

                                                        downloaded += chunk.len() as u64;
                                                        if byte_cap.is_some_and(|cap| downloaded > cap) {
                                                            // Unknown size, so the quota is enforced as
                                                            // the bytes arrive.
                                                            let _ = buf_writer.flush().await;
                                                            let _ = buf_writer.shutdown().await;
                                                            return DownloadExecutionResult::Failed(
                                                                QUOTA_EXCEEDED_DURING_TRANSFER.to_string(),
                                                            );
                                                        }
                                                        if saw_first_chunk {
                                                            let now = Instant::now();
                                                            let should_log_progress = now
                                                                .duration_since(last_progress_log_at)
                                                                >= DOWNLOAD_PROGRESS_LOG_INTERVAL
                                                                || downloaded
                                                                    .saturating_sub(last_progress_logged_bytes)
                                                                    >= DOWNLOAD_PROGRESS_LOG_BYTES;
                                                            if should_log_progress {
                                                                match total_size {
                                                                    Some(total) if total > 0 => {
                                                                        let percent = downloaded
                                                                            .saturating_mul(100)
                                                                            .checked_div(total)
                                                                            .unwrap_or(0)
                                                                            .min(100);
                                                                        debug!(
                                                                                "Download progress for {file_path_str}: {}MB / {}MB ({}%)",
                                                                                bytes_to_megabytes(downloaded),
                                                                                bytes_to_megabytes(total),
                                                                                percent
                                                                            );
                                                                    }
                                                                    _ => {
                                                                        debug!(
                                                                                "Download progress for {file_path_str}: {}MB received",
                                                                                bytes_to_megabytes(downloaded)
                                                                            );
                                                                    }
                                                                }
                                                                last_progress_log_at = now;
                                                                last_progress_logged_bytes = downloaded;
                                                            }
                                                        } else {
                                                            saw_first_chunk = true;
                                                            info!(
                                                                    "Receiving download data for {file_path_str}: {}MB received",
                                                                    bytes_to_megabytes(downloaded)
                                                                );
                                                            last_progress_log_at = Instant::now();
                                                            last_progress_logged_bytes = downloaded;
                                                        }
                                                        let should_update_snapshot = (last_snapshot_update_bytes == 0
                                                            && downloaded > 0)
                                                            || downloaded.saturating_sub(last_snapshot_update_bytes)
                                                                >= DOWNLOAD_SNAPSHOT_UPDATE_BYTES
                                                            || Instant::now().duration_since(last_snapshot_update_at)
                                                                >= DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL;
                                                        if should_update_snapshot {
                                                            let changed = update_active_download_for_worker(
                                                                &active,
                                                                worker_uuid,
                                                                |download| {
                                                                    download.size = downloaded;
                                                                    true
                                                                },
                                                            )
                                                            .await;
                                                            if changed {
                                                                if let Some(event_manager) = event_manager {
                                                                    publish_recording_progress(event_manager);
                                                                }
                                                            }
                                                            last_snapshot_update_at = Instant::now();
                                                            last_snapshot_update_bytes = downloaded;
                                                        }
                                                    }
                                                    Err(err) => {
                                                        return DownloadExecutionResult::Failed(format!(
                                                            "Error while writing to file: {file_path_str} {err}"
                                                        ));
                                                    }
                                                }
                                            } else {
                                                let megabytes = bytes_to_megabytes(downloaded);
                                                info!("Downloaded {file_path_str}, filesize: {megabytes}MB");
                                                update_active_download_for_worker(&active, worker_uuid, |download| {
                                                    download.size = downloaded;
                                                    true
                                                })
                                                .await;
                                                if let Err(err) = buf_writer.flush().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                if let Err(err) = buf_writer.shutdown().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                return match finalize_http_transfer(
                                                    &file_download.file_path,
                                                    &file_path,
                                                )
                                                .await
                                                {
                                                    Ok(()) => DownloadExecutionResult::Completed,
                                                    Err(error) => DownloadExecutionResult::Failed(format!(
                                                        "Could not finalize {}: {error}",
                                                        file_download.file_path.display()
                                                    )),
                                                };
                                            }
                                        }
                                        Err(err) => return classify_download_stream_io_error(file_path_str, &err),
                                    }
                                }
                            }
                            Err(err) => DownloadExecutionResult::Failed(format!(
                                "Error while opening file: {file_path_str} {err}"
                            )),
                        }
                    } else {
                        DownloadExecutionResult::Failed("Error file-download file-path unknown".to_string())
                    }
                }
                Err(err) => DownloadExecutionResult::Failed(format!(
                    "Error while creating directory for file: {} {}",
                    file_download.file_dir.to_str().unwrap_or("?"),
                    err
                )),
            }
        }
        Err(result) => result,
    }
}
