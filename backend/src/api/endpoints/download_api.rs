use crate::{
    api::model::{
        AppState, ActiveProviderManager, ConnectionManager, DownloadControl, DownloadKind, DownloadQueue,
        DownloadState, DownloadWaitOutcome, EventManager, EventMessage, FileDownload, FileDownloadRequest,
        FileRecordingRequest,
        RecordingExecutionResult, run_recording,
    },
    model::{AppConfig, VideoDownloadConfig},
    utils::{async_file_writer, request, request::create_client, IO_BUFFER_SIZE},
};
use axum::response::IntoResponse;
use futures::stream::TryStreamExt;
use log::{debug, error, info, warn};
use serde::Deserialize;
use serde_json::json;
use shared::{error::to_io_error, model::{DownloadsDelta, DownloadsResponse}, utils::bytes_to_megabytes};
use std::{collections::HashMap, ops::Deref, pin::Pin, sync::Arc};
use tokio::{
    fs,
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Notify, RwLock},
    time::{self, Duration, Instant, Sleep},
};
use tokio_util::sync::CancellationToken;

const DOWNLOAD_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const DOWNLOAD_PROGRESS_LOG_BYTES: u64 = 16 * 1024 * 1024;
const DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const DOWNLOAD_SNAPSHOT_UPDATE_BYTES: u64 = 4 * 1024 * 1024;
const RECORDING_PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(5);
type ProviderCapacities = Vec<(Arc<str>, usize, usize)>;

enum DownloadExecutionResult {
    Completed,
    Paused,
    Cancelled,
    Preempted,
    Retryable(String),
    Failed(String),
}

enum ProviderAcquireResult {
    Acquired(Option<crate::api::model::ProviderHandle>),
    Paused,
    Cancelled,
    Preempted,
}

fn classify_download_open_error(url: &reqwest::Url, err: &reqwest::Error) -> DownloadExecutionResult {
    if is_retryable_download_error(err) {
        DownloadExecutionResult::Retryable(format!("Error while opening url: {url} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while opening url: {url} {err}"))
    }
}

fn classify_download_stream_io_error(file_path_str: &str, err: &std::io::Error) -> DownloadExecutionResult {
    if retryable_transport_error_message(&err.to_string()) {
        DownloadExecutionResult::Retryable(format!("Error while downloading file: {file_path_str} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while downloading file: {file_path_str} {err}"))
    }
}

fn apply_download_retry_jitter(base_secs: u64, jitter_percent: u8) -> u64 {
    let jitter_percent = i64::from(jitter_percent.min(95));
    if jitter_percent == 0 {
        return base_secs.max(1);
    }
    let jitter_percent = fastrand::i64(-jitter_percent..=jitter_percent);
    let base_i64 = i64::try_from(base_secs.max(1)).unwrap_or(i64::MAX);
    let jitter_delta = base_i64.saturating_mul(jitter_percent).saturating_div(100);
    let jittered = base_i64.saturating_add(jitter_delta);
    u64::try_from(jittered.max(1)).unwrap_or(1)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn compute_download_retry_backoff_secs(attempts: u8, download_cfg: &VideoDownloadConfig) -> u64 {
    let exponent = i32::from(attempts.saturating_sub(1));
    let scaled_secs = (download_cfg.retry_backoff_initial_secs as f64) * download_cfg.retry_backoff_multiplier.powi(exponent);
    let clamped_secs = scaled_secs.clamp(
        download_cfg.retry_backoff_initial_secs as f64,
        download_cfg.retry_backoff_max_secs as f64,
    );
    let base_secs = clamped_secs.round() as u64;
    apply_download_retry_jitter(base_secs, download_cfg.retry_backoff_jitter_percent)
}

fn is_retryable_download_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

fn is_retryable_download_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || retryable_transport_error_message(&err.to_string())
}

fn retryable_transport_error_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("timed out")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("temporary failure")
        || msg.contains("temporarily unavailable")
        || msg.contains("network is unreachable")
        || msg.contains("dns")
        || msg.contains("name or service not known")
        || msg.contains("connection closed before message completed")
        || msg.contains("unexpected eof")
}

fn parse_content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers.get("content-range").and_then(|v| {
        v.to_str().ok().and_then(|s| s.split('/').next_back().and_then(|total| total.parse::<u64>().ok()))
    })
}

fn compute_download_total_size(response: &reqwest::Response, existing_size: u64) -> Option<u64> {
    let is_resume = existing_size > 0 || response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if is_resume {
        parse_content_range_total(response.headers()).or_else(|| {
            if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                response.content_length().map(|len| len.saturating_add(existing_size))
            } else {
                response.content_length()
            }
        })
    } else {
        response.content_length()
    }
}

fn background_download_should_wait(
    priority: i8,
    capacities: &[(Arc<str>, usize, usize)],
    download_cfg: &VideoDownloadConfig,
) -> bool {
    if priority <= 0 || capacities.is_empty() {
        return false;
    }

    let background_limit = usize::from(download_cfg.max_background_per_provider);
    let reserve_slots = usize::from(download_cfg.reserve_slots_for_users);

    let blocked_by_background_limit =
        background_limit > 0 && capacities.iter().all(|(_, current, _)| *current >= background_limit);
    let blocked_by_reserved_slots = reserve_slots > 0
        && capacities
            .iter()
            .all(|(_, current, max)| *max > 0 && current.saturating_add(reserve_slots) >= *max);

    blocked_by_background_limit || blocked_by_reserved_slots
}

fn capacities_have_free_slot(capacities: &[(Arc<str>, usize, usize)]) -> bool {
    capacities.iter().any(|(_, current, max)| *max == 0 || current < max)
}

#[cfg(test)]
fn recording_deadline_reached(download: &FileDownload, now_ts: i64) -> bool {
    download.kind == crate::api::model::DownloadKind::Recording
        && download
            .start_at
            .zip(download.duration_secs)
            .is_some_and(|(start_at, duration_secs)| now_ts >= start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(90)))
}

async fn active_download_snapshot(active: &RwLock<Option<FileDownload>>) -> Option<FileDownload> { active.read().await.clone() }

pub async fn download_queue_snapshot(download_queue: &DownloadQueue) -> DownloadsResponse {
    let queue: Vec<shared::model::FileDownloadDto> = download_queue
        .queue
        .lock()
        .await
        .iter()
        .map(shared::model::FileDownloadDto::from)
        .collect();
    let mut queue = queue;
    queue.extend(
        download_queue
            .scheduled
            .read()
            .await
            .iter()
            .map(shared::model::FileDownloadDto::from),
    );
    let finished = download_queue
        .finished
        .read()
        .await
        .iter()
        .map(shared::model::FileDownloadDto::from)
        .collect();
    let active = download_queue
        .active
        .read()
        .await
        .as_ref()
        .map(shared::model::FileDownloadDto::from)
        .into_iter()
        .collect();

    DownloadsResponse {
        queue,
        finished,
        active,
    }
}

async fn broadcast_download_queue_update(event_manager: &Arc<EventManager>, download_queue: &DownloadQueue) {
    if !event_manager.has_event_receivers() {
        return;
    }
    let mut queue = download_queue
        .queue
        .lock()
        .await
        .iter()
        .map(shared::model::FileDownloadDto::from)
        .collect::<Vec<_>>();
    queue.extend(
        download_queue
            .scheduled
            .read()
            .await
            .iter()
            .map(shared::model::FileDownloadDto::from),
    );
    let finished = download_queue
        .finished
        .read()
        .await
        .iter()
        .map(shared::model::FileDownloadDto::from)
        .collect::<Vec<_>>();
    let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::QueueReplaced { queue }));
    let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::FinishedReplaced {
        finished,
    }));
    if let Some(download) = download_queue.active.read().await.as_ref() {
        let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActivePatched(
            shared::model::FileDownloadDto::from(download),
        )));
    } else {
        let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActiveCleared));
    }
}

async fn broadcast_active_download_delta(event_manager: &Arc<EventManager>, active: &RwLock<Option<FileDownload>>) {
    if !event_manager.has_event_receivers() {
        return;
    }
    if let Some(download) = active.read().await.as_ref() {
        let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActivePatched(
            shared::model::FileDownloadDto::from(download),
        )));
    }
}

async fn refresh_recording_progress(
    active: &RwLock<Option<FileDownload>>,
    file_path: &std::path::Path,
    event_manager: &Arc<EventManager>,
) {
    let current_size = tokio::fs::metadata(file_path).await.map_or(0, |metadata| metadata.len());
    let changed = {
        let mut active = active.write().await;
        if let Some(download) = active.as_mut() {
            if download.kind == DownloadKind::Recording && download.size != current_size {
                download.size = current_size;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        broadcast_active_download_delta(event_manager, active).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn download_file(
    active: Arc<RwLock<Option<FileDownload>>>,
    client: &reqwest::Client,
    control_signal: Arc<RwLock<DownloadControl>>,
    control_notify: Arc<Notify>,
    provider_cancel_token: Option<CancellationToken>,
    event_manager: Option<&Arc<EventManager>>,
    download_queue: Option<&Arc<DownloadQueue>>,
) -> DownloadExecutionResult {
    if let Some(file_download) = active_download_snapshot(&active).await {
        let url = file_download.url.clone();
        let file_path = file_download.file_path.clone();
        // Check for existing partial file for resume
        let existing_size = tokio::fs::metadata(&file_path).await.map_or(0, |metadata| metadata.len());

        let mut request_builder = client.get(url.clone());
        if existing_size > 0 {
            request_builder = request_builder.header("Range", format!("bytes={existing_size}-"));
        }

        if let Some(result) =
            handle_download_control_without_writer(&active, current_download_control(&control_signal)).await
        {
            return result;
        }

        let send_request = request_builder.send();
        tokio::pin!(send_request);
        let response_result = loop {
            if let Some(cancel_token) = provider_cancel_token.as_ref() {
                tokio::select! {
                    biased;
                    () = cancel_token.cancelled() => return DownloadExecutionResult::Preempted,
                    () = control_notify.notified() => {
                        if let Some(result) = handle_download_control_without_writer(
                            &active,
                            *control_signal.read().await,
                        ).await {
                            return result;
                        }
                    }
                    response = &mut send_request => break response,
                }
            } else {
                tokio::select! {
                    biased;
                    () = control_notify.notified() => {
                        if let Some(result) = handle_download_control_without_writer(
                            &active,
                            *control_signal.read().await,
                        ).await {
                            return result;
                        }
                    }
                    response = &mut send_request => break response,
                }
            }
        };

        match response_result {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                    if is_retryable_download_status(status) {
                        return DownloadExecutionResult::Retryable(format!(
                            "Download request failed for {url} with transient HTTP {status}"
                        ));
                    }
                    return DownloadExecutionResult::Failed(format!("Download request failed for {url} with HTTP {status}"));
                }
                let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

                let total_size = compute_download_total_size(&response, existing_size);

                if let Some(total) = total_size {
                    if let Some(download) = active.write().await.as_mut() {
                        download.total_size = Some(total);
                    }
                    if let Some(event_manager) = event_manager {
                        broadcast_active_download_delta(event_manager, &active).await;
                    }
                }

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
                                    let deadline_at = recording_deadline_instant(&file_download);
                                    let mut deadline_sleep = deadline_at.map(|deadline| {
                                        Box::pin(time::sleep_until(deadline)) as Pin<Box<Sleep>>
                                    });

                                    loop {
                                        if let Some(result) =
                                            handle_download_control(&active, current_download_control(&control_signal), &mut buf_writer).await
                                        {
                                            return result;
                                        }

                                        if deadline_at.is_some_and(|deadline| Instant::now() >= deadline) {
                                            if let Some(lock) = active.write().await.as_mut() {
                                                lock.paused = false;
                                                lock.finished = true;
                                                lock.state = DownloadState::Completed;
                                                lock.error = None;
                                            }
                                            if let Err(err) = buf_writer.flush().await {
                                                return DownloadExecutionResult::Failed(err.to_string());
                                            }
                                            if let Err(err) = buf_writer.shutdown().await {
                                                return DownloadExecutionResult::Failed(err.to_string());
                                            }
                                            return DownloadExecutionResult::Completed;
                                        }

                                        let next_item = if let (Some(cancel_token), Some(deadline_sleep)) =
                                            (provider_cancel_token.as_ref(), deadline_sleep.as_mut())
                                        {
                                            tokio::select! {
                                                biased;
                                                () = cancel_token.cancelled() => return DownloadExecutionResult::Preempted,
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                () = deadline_sleep.as_mut() => return DownloadExecutionResult::Completed,
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        } else if let Some(cancel_token) = provider_cancel_token.as_ref() {
                                            tokio::select! {
                                                biased;
                                                () = cancel_token.cancelled() => return DownloadExecutionResult::Preempted,
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        } else if let Some(deadline_sleep) = deadline_sleep.as_mut() {
                                            tokio::select! {
                                                biased;
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                () = deadline_sleep.as_mut() => return DownloadExecutionResult::Completed,
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        } else {
                                            tokio::select! {
                                                biased;
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        };

                                        match next_item {
                                            Ok(item) => {
                                                if let Some(chunk) = item {
                                                    match buf_writer.write_all(&chunk).await {
                                                        Ok(()) => {
                                                            write_counter += chunk.len();
                                                            if write_counter >= IO_BUFFER_SIZE {
                                                                if let Err(err) = buf_writer.flush().await {
                                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                                }
                                                                write_counter = 0;
                                                            }

                                                            downloaded += chunk.len() as u64;
                                                            if saw_first_chunk {
                                                                let now = Instant::now();
                                                                let should_log_progress = now.duration_since(last_progress_log_at)
                                                                    >= DOWNLOAD_PROGRESS_LOG_INTERVAL
                                                                    || downloaded.saturating_sub(last_progress_logged_bytes)
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
                                                                if let (Some(event_manager), Some(download_queue)) =
                                                                    (event_manager, download_queue)
                                                                {
                                                                    let _ = download_queue;
                                                                    broadcast_active_download_delta(event_manager, &active).await;
                                                                }
                                                            }
                                                        } else {
                                                                saw_first_chunk = true;
                                                                info!(
                                                                    "Receiving download data for {file_path_str}: {}MB received",
                                                                    bytes_to_megabytes(downloaded)
                                                                );
                                                                last_progress_log_at = Instant::now();
                                                                last_progress_logged_bytes = downloaded;
                                                                if let (Some(event_manager), Some(download_queue)) =
                                                                    (event_manager, download_queue)
                                                                {
                                                                    let _ = download_queue;
                                                                    broadcast_active_download_delta(event_manager, &active).await;
                                                                }
                                                            }
                                                            let should_update_snapshot = downloaded
                                                                .saturating_sub(last_snapshot_update_bytes)
                                                                >= DOWNLOAD_SNAPSHOT_UPDATE_BYTES
                                                                || Instant::now().duration_since(last_snapshot_update_at)
                                                                    >= DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL;
                                                            if should_update_snapshot {
                                                                if let Some(lock) = active.write().await.as_mut() {
                                                                    lock.size = downloaded;
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
                                                    if let Some(lock) = active.write().await.as_mut() {
                                                        lock.paused = false;
                                                        lock.size = downloaded;
                                                        lock.finished = true;
                                                        lock.state = DownloadState::Completed;
                                                    }
                                                    if let Err(err) = buf_writer.flush().await {
                                                        return DownloadExecutionResult::Failed(err.to_string());
                                                    }
                                                    if let Err(err) = buf_writer.shutdown().await {
                                                        return DownloadExecutionResult::Failed(err.to_string());
                                                    }
                                                    return DownloadExecutionResult::Completed;
                                                }
                                            }
                                            Err(err) => return classify_download_stream_io_error(file_path_str, &err),
                                        }
                                    }
                                }
                                Err(err) => DownloadExecutionResult::Failed(format!("Error while opening file: {file_path_str} {err}")),
                            }
                        } else {
                            DownloadExecutionResult::Failed("Error file-download file-path unknown".to_string())
                        }
                    }
                    Err(err) => DownloadExecutionResult::Failed(format!(
                        "Error while creating directory for file: {} {}",
                        &file_download.file_dir.to_str().unwrap_or("?"),
                        err
                    )),
                }
            }
            Err(err) => classify_download_open_error(&url, &err),
        }
    } else {
        DownloadExecutionResult::Failed("No active file download".to_string())
    }
}

fn current_download_control(control_signal: &RwLock<DownloadControl>) -> DownloadControl {
    control_signal.try_read().map_or(DownloadControl::None, |control| *control)
}

fn should_exit_worker_after_preempt(control: DownloadControl) -> bool { control == DownloadControl::Restart }

async fn handle_download_control<W>(
    active: &Arc<RwLock<Option<FileDownload>>>,
    control: DownloadControl,
    buf_writer: &mut W,
) -> Option<DownloadExecutionResult>
where
    W: AsyncWrite + Unpin,
{
    match control {
        DownloadControl::Pause => {
            if let Some(download) = active.write().await.as_mut() {
                download.paused = true;
                download.state = DownloadState::Paused;
            }
            if let Err(err) = buf_writer.flush().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            if let Err(err) = buf_writer.shutdown().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            Some(DownloadExecutionResult::Paused)
        }
        DownloadControl::Cancel => {
            if let Some(download) = active.write().await.as_mut() {
                download.finished = true;
                download.paused = false;
                download.state = DownloadState::Cancelled;
                if download.error.is_none() {
                    download.error = Some("Cancelled by user".to_string());
                }
            }
            if let Err(err) = buf_writer.flush().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            if let Err(err) = buf_writer.shutdown().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            Some(DownloadExecutionResult::Cancelled)
        }
        DownloadControl::Restart => {
            if let Err(err) = buf_writer.flush().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            if let Err(err) = buf_writer.shutdown().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            Some(DownloadExecutionResult::Preempted)
        }
        DownloadControl::None => None,
    }
}

async fn handle_download_control_without_writer(
    active: &Arc<RwLock<Option<FileDownload>>>,
    control: DownloadControl,
) -> Option<DownloadExecutionResult> {
    match control {
        DownloadControl::Pause => {
            if let Some(download) = active.write().await.as_mut() {
                download.paused = true;
                download.state = DownloadState::Paused;
            }
            Some(DownloadExecutionResult::Paused)
        }
        DownloadControl::Cancel => {
            if let Some(download) = active.write().await.as_mut() {
                download.finished = true;
                download.paused = false;
                download.state = DownloadState::Cancelled;
                if download.error.is_none() {
                    download.error = Some("Cancelled by user".to_string());
                }
            }
            Some(DownloadExecutionResult::Cancelled)
        }
        DownloadControl::Restart => Some(DownloadExecutionResult::Preempted),
        DownloadControl::None => None,
    }
}

fn recording_deadline_instant(download: &FileDownload) -> Option<Instant> {
    if download.kind != DownloadKind::Recording {
        return None;
    }
    let (start_at, duration_secs) = download.start_at.zip(download.duration_secs)?;
    let deadline_ts = start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX));
    let now_ts = chrono::Utc::now().timestamp();
    if now_ts >= deadline_ts {
        return Some(Instant::now());
    }
    let remaining_secs = u64::try_from(deadline_ts.saturating_sub(now_ts)).ok()?;
    Some(Instant::now() + Duration::from_secs(remaining_secs))
}

async fn set_active_download_state(
    download_queue: &DownloadQueue,
    state: DownloadState,
    error: Option<String>,
    paused: bool,
) -> bool {
    let mut active = download_queue.active.write().await;
    if let Some(download) = active.as_mut() {
        download.state = state;
        download.error = error;
        download.paused = paused;
        download.finished = false;
        true
    } else {
        false
    }
}

async fn requeue_active_download_for_retry(download_queue: &DownloadQueue) {
    if let Some(mut download) = download_queue.active.write().await.take() {
        download.finished = false;
        download.paused = false;
        download.error = None;
        download.state = DownloadState::Queued;
        download.next_retry_at = None;
        download_queue.queue.lock().await.push_front(download);
    }
    let _ = download_queue.persist_to_disk().await;
}

async fn requeue_active_download_for_capacity_wait(download_queue: &DownloadQueue, reason: &str) {
    if let Some(mut download) = download_queue.active.write().await.take() {
        download.finished = false;
        download.paused = false;
        download.error = Some(reason.to_string());
        download.state = DownloadState::WaitingForCapacity;
        download.next_retry_at = None;
        download_queue.queue.lock().await.push_front(download);
    }
    let _ = download_queue.persist_to_disk().await;
}

const DOWNLOAD_PREEMPTED_REASON: &str = "Preempted by higher-priority foreground stream";
const RECORDING_PREEMPTED_REASON: &str =
    "Recording preempted by higher-priority foreground stream; waiting to resume within the remaining window";

fn preemption_reason_for(download: &FileDownload) -> &'static str {
    match download.kind {
        DownloadKind::Download => DOWNLOAD_PREEMPTED_REASON,
        DownloadKind::Recording => RECORDING_PREEMPTED_REASON,
    }
}

#[allow(clippy::too_many_lines)]
pub(in crate::api) async fn ensure_download_worker_running(
    cfg: &AppConfig,
    download_cfg: &VideoDownloadConfig,
    download_queue: &Arc<DownloadQueue>,
    event_manager: &Arc<EventManager>,
    active_provider: &Arc<ActiveProviderManager>,
    connection_manager: &Arc<ConnectionManager>,
) -> Result<(), String> {
    let mut worker_running = download_queue.worker_running.write().await;
    if *worker_running {
        debug!("Download worker already running");
        return Ok(());
    }
    *worker_running = true;
    drop(worker_running);

    if download_queue.active.read().await.is_none() {
        let next_download = download_queue.as_ref().queue.lock().await.pop_front();
        if let Some(next_download) = next_download {
            debug!(
                "Promoting queued download {} ({}) to active",
                next_download.uuid, next_download.filename
            );
            *download_queue.as_ref().active.write().await = Some(next_download);
            broadcast_download_queue_update(event_manager, download_queue).await;
        }
    }

    if download_queue.active.read().await.is_some() {
        let config = cfg.config.load();
        let disabled_headers = cfg.get_disabled_headers();
        let headers = request::get_request_headers(
            Some(&download_cfg.headers),
            None,
            disabled_headers.as_ref(),
            config.default_user_agent.as_deref(),
        );
        let dq = Arc::clone(download_queue);
        let control_signal = Arc::clone(&dq.control_signal);
        let control_notify = Arc::clone(&dq.control_notify);
        let event_manager = Arc::clone(event_manager);
        let active_provider = Arc::clone(active_provider);
        let connection_manager = Arc::clone(connection_manager);
        let download_cfg = download_cfg.clone();

        if let Ok(client) = create_client(cfg).default_headers(headers).build() {
                if let Some(active) = dq.active.read().await.as_ref() {
                    info!(
                        "Starting download worker for active download {} ({})",
                        active.uuid, active.filename
                    );
                }
                tokio::spawn(async move {
                    loop {
                        if dq.active.read().await.deref().is_some() {
                            if let Some(download) = dq.active.read().await.as_ref() {
                                if download.paused {
                                    break;
                                }
                            }

                            // Acquire a provider connection slot for this download.
                            // If the provider is at capacity, wait in the priority queue until signalled.
                            // Never proceeds without a slot when input_name is set — account bans otherwise.
                            let provider_acquire_result = {
                                let (input_name, priority) = {
                                    let active = dq.active.read().await;
                                    active.as_ref().map_or((None, 0i8), |dl| (dl.input_name.clone(), dl.priority))
                                };
                                if let Some(input_name) = input_name {
                                    loop {
                                        let capacities = active_provider.provider_capacities_for_input(&input_name).await;
                                        if background_download_should_wait(priority, &capacities, &download_cfg) {
                                            if set_active_download_state(&dq, DownloadState::WaitingForCapacity, None, false).await {
                                                let _ = dq.persist_to_disk().await;
                                                broadcast_download_queue_update(&event_manager, &dq).await;
                                            }
                                            match dq
                                                .slot_waiters
                                                .wait(
                                                    Some(Arc::clone(&input_name)),
                                                    priority,
                                                    control_signal.as_ref(),
                                                    control_notify.as_ref(),
                                                )
                                                .await
                                            {
                                                DownloadWaitOutcome::Signalled => {}
                                                DownloadWaitOutcome::Paused => break ProviderAcquireResult::Paused,
                                                DownloadWaitOutcome::Cancelled => break ProviderAcquireResult::Cancelled,
                                                DownloadWaitOutcome::Restarted => break ProviderAcquireResult::Preempted,
                                            }
                                            continue;
                                        }
                                        if let Some(handle) = active_provider.acquire_connection_for_download(&input_name, priority).await {
                                            let _ = set_active_download_state(&dq, DownloadState::Downloading, None, false).await;
                                            let _ = dq.persist_to_disk().await;
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                            break ProviderAcquireResult::Acquired(Some(handle));
                                        }
                                        if *control_signal.read().await == DownloadControl::Cancel {
                                            break ProviderAcquireResult::Cancelled;
                                        }
                                        if *control_signal.read().await == DownloadControl::Pause {
                                            break ProviderAcquireResult::Paused;
                                        }
                                        if *control_signal.read().await == DownloadControl::Restart {
                                            break ProviderAcquireResult::Preempted;
                                        }
                                        if set_active_download_state(&dq, DownloadState::WaitingForCapacity, None, false).await {
                                            let _ = dq.persist_to_disk().await;
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                        }
                                        // Wait for highest-priority signal — no sleep, no polling.
                                        match dq
                                            .slot_waiters
                                            .wait(
                                                Some(Arc::clone(&input_name)),
                                                priority,
                                                control_signal.as_ref(),
                                                control_notify.as_ref(),
                                            )
                                            .await
                                        {
                                            DownloadWaitOutcome::Signalled => {}
                                            DownloadWaitOutcome::Paused => break ProviderAcquireResult::Paused,
                                            DownloadWaitOutcome::Cancelled => break ProviderAcquireResult::Cancelled,
                                            DownloadWaitOutcome::Restarted => break ProviderAcquireResult::Preempted,
                                        }
                                    }
                                } else {
                                    ProviderAcquireResult::Acquired(None)
                                }
                            };

                            let provider_handle = match provider_acquire_result {
                                ProviderAcquireResult::Acquired(handle) => {
                                    *control_signal.write().await = DownloadControl::None;
                                    handle
                                }
                                ProviderAcquireResult::Paused => {
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                    break;
                                }
                                ProviderAcquireResult::Cancelled => {
                                    if let Some(fd) = dq.active.write().await.take() {
                                        let mut fd = fd;
                                        fd.next_retry_at = None;
                                        dq.finished.write().await.push(fd);
                                    }
                                    *dq.control_signal.write().await = DownloadControl::None;
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                    continue;
                                }
                                ProviderAcquireResult::Preempted => {
                                    *control_signal.write().await = DownloadControl::None;
                                    requeue_active_download_for_capacity_wait(&dq, "Reloading download service configuration").await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                    break;
                                }
                            };

                            let execution_result = {
                                let active = dq.active.read().await;
                                let Some(download) = active.as_ref().cloned() else {
                                    break;
                                };
                                drop(active);
                                match download.kind {
                                    DownloadKind::Download => download_file(
                                        Arc::clone(&dq.active),
                                        &client,
                                        Arc::clone(&control_signal),
                                        Arc::clone(&control_notify),
                                        provider_handle.as_ref().and_then(|handle| handle.cancel_token.clone()),
                                        Some(&event_manager),
                                        Some(&dq),
                                    )
                                    .await,
                                    DownloadKind::Recording => {
                                        let mut recording_future = Box::pin(run_recording(
                                            &download,
                                            &control_signal,
                                            &control_notify,
                                            provider_handle.as_ref().and_then(|handle| handle.cancel_token.as_ref()),
                                        ));
                                        let mut progress_tick = time::interval(RECORDING_PROGRESS_UPDATE_INTERVAL);
                                        progress_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

                                        let result = loop {
                                            tokio::select! {
                                                recording_result = &mut recording_future => break recording_result,
                                                _ = progress_tick.tick() => {
                                                    if event_manager.has_event_receivers() {
                                                        refresh_recording_progress(&dq.active, &download.file_path, &event_manager).await;
                                                    }
                                                }
                                            }
                                        };
                                        if event_manager.has_event_receivers() {
                                            refresh_recording_progress(&dq.active, &download.file_path, &event_manager).await;
                                        }

                                        match result {
                                            RecordingExecutionResult::Completed => DownloadExecutionResult::Completed,
                                            RecordingExecutionResult::Paused => DownloadExecutionResult::Paused,
                                            RecordingExecutionResult::Cancelled => DownloadExecutionResult::Cancelled,
                                            RecordingExecutionResult::Preempted => DownloadExecutionResult::Preempted,
                                            RecordingExecutionResult::Retryable(err) => DownloadExecutionResult::Retryable(err),
                                            RecordingExecutionResult::Failed(err) => DownloadExecutionResult::Failed(err),
                                        }
                                    }
                                }
                            };

                            match execution_result
                            {
                                DownloadExecutionResult::Completed => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        fd.state = DownloadState::Completed;
                                        fd.next_retry_at = None;
                                        dq.finished.write().await.push(fd.clone());
                                    }
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                }
                                DownloadExecutionResult::Paused => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                    break;
                                }
                                DownloadExecutionResult::Cancelled => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    if let Some(fd) = dq.active.write().await.take() {
                                        let mut fd = fd;
                                        fd.next_retry_at = None;
                                        dq.finished.write().await.push(fd);
                                    }
                                    *dq.control_signal.write().await = DownloadControl::None;
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                }
                                DownloadExecutionResult::Preempted => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    let control = *control_signal.read().await;
                                    match control {
                                        DownloadControl::Restart => warn!("Active transfer is restarting to apply updated download service configuration"),
                                        _ => warn!("Active transfer was preempted by a higher-priority stream"),
                                    }
                                    *dq.control_signal.write().await = DownloadControl::None;
                                    let reason = {
                                        let active = dq.active.read().await;
                                        if control == DownloadControl::Restart {
                                            "Reloading download service configuration"
                                        } else {
                                            active.as_ref().map_or(DOWNLOAD_PREEMPTED_REASON, preemption_reason_for)
                                        }
                                    };
                                    requeue_active_download_for_capacity_wait(&dq, reason).await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                    if should_exit_worker_after_preempt(control) {
                                        break;
                                    }
                                }
                                DownloadExecutionResult::Retryable(_err) => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    warn!("Retrying active download after transient failure");
                                    let retry_plan = {
                                        let mut active = dq.active.write().await;
                                        if let Some(download) = active.as_mut() {
                                            download.retry_attempts = download.retry_attempts.saturating_add(1);
                                            if download.retry_attempts > download_cfg.retry_max_attempts {
                                                None
                                            } else {
                                                let retry_delay_secs =
                                                    compute_download_retry_backoff_secs(download.retry_attempts, &download_cfg);
                                                let next_retry_at = chrono::Utc::now()
                                                    .timestamp()
                                                    .saturating_add(i64::try_from(retry_delay_secs).unwrap_or(i64::MAX));
                                                download.next_retry_at = Some(next_retry_at);
                                                Some((retry_delay_secs, next_retry_at, download.retry_attempts))
                                            }
                                        } else {
                                            let retry_delay_secs = compute_download_retry_backoff_secs(1, &download_cfg);
                                            let next_retry_at = chrono::Utc::now()
                                                .timestamp()
                                                .saturating_add(i64::try_from(retry_delay_secs).unwrap_or(i64::MAX));
                                            Some((retry_delay_secs, next_retry_at, 1))
                                        }
                                    };
                                    let Some((retry_delay_secs, _next_retry_at, retry_attempts)) = retry_plan else {
                                        if let Some(fd) = &mut *dq.active.write().await {
                                            fd.finished = true;
                                            fd.paused = false;
                                            fd.next_retry_at = None;
                                            fd.state = DownloadState::Failed;
                                            fd.error = Some(format!(
                                                "Retry limit reached after {} attempts",
                                                download_cfg.retry_max_attempts
                                            ));
                                            dq.finished.write().await.push(fd.clone());
                                        }
                                        let _ = dq.persist_to_disk().await;
                                        *dq.active.write().await = dq.queue.lock().await.pop_front();
                                        let _ = dq.persist_to_disk().await;
                                        broadcast_download_queue_update(&event_manager, &dq).await;
                                        if dq.active.read().await.is_some() {
                                            continue;
                                        }
                                        break;
                                    };
                                    if set_active_download_state(
                                        &dq,
                                        DownloadState::RetryWaiting,
                                        Some(format!(
                                            "Retrying after transient failure in {retry_delay_secs}s (attempt {retry_attempts}/{})",
                                            download_cfg.retry_max_attempts
                                        )),
                                        false,
                                    )
                                    .await
                                    {
                                        let _ = dq.persist_to_disk().await;
                                        broadcast_download_queue_update(&event_manager, &dq).await;
                                    }
                                    let mut retry_sleep = Box::pin(time::sleep(Duration::from_secs(retry_delay_secs)));
                                    let retry_wait_outcome = loop {
                                        tokio::select! {
                                            () = &mut retry_sleep => break DownloadExecutionResult::Retryable(String::new()),
                                            () = control_notify.notified() => {
                                                match *control_signal.read().await {
                                                    DownloadControl::Pause => break DownloadExecutionResult::Paused,
                                                    DownloadControl::Cancel => break DownloadExecutionResult::Cancelled,
                                                    DownloadControl::Restart => break DownloadExecutionResult::Preempted,
                                                    DownloadControl::None => {}
                                                }
                                            }
                                        }
                                    };

                                    match retry_wait_outcome {
                                        DownloadExecutionResult::Retryable(_) => {
                                            *dq.control_signal.write().await = DownloadControl::None;
                                            requeue_active_download_for_retry(&dq).await;
                                            *dq.active.write().await = dq.queue.lock().await.pop_front();
                                            let _ = dq.persist_to_disk().await;
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                        }
                                        DownloadExecutionResult::Paused => {
                                            if let Some(download) = dq.active.write().await.as_mut() {
                                                download.paused = true;
                                                download.state = DownloadState::Paused;
                                                download.next_retry_at = None;
                                            }
                                            let _ = dq.persist_to_disk().await;
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                            break;
                                        }
                                        DownloadExecutionResult::Cancelled => {
                                            if let Some(fd) = dq.active.write().await.take() {
                                                let mut fd = fd;
                                                fd.next_retry_at = None;
                                                fd.error.get_or_insert_with(|| "Cancelled by user".to_string());
                                                fd.state = DownloadState::Cancelled;
                                                dq.finished.write().await.push(fd);
                                            }
                                            *dq.control_signal.write().await = DownloadControl::None;
                                            let _ = dq.persist_to_disk().await;
                                            *dq.active.write().await = dq.queue.lock().await.pop_front();
                                            let _ = dq.persist_to_disk().await;
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                        }
                                        DownloadExecutionResult::Completed
                                        | DownloadExecutionResult::Failed(_) => {}
                                        DownloadExecutionResult::Preempted => {
                                            *dq.control_signal.write().await = DownloadControl::None;
                                            requeue_active_download_for_capacity_wait(&dq, "Reloading download service configuration").await;
                                            *dq.active.write().await = dq.queue.lock().await.pop_front();
                                            let _ = dq.persist_to_disk().await;
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                            break;
                                        }
                                    }
                                }
                                DownloadExecutionResult::Failed(err) => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    warn!("Download failed permanently: {err}");
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        fd.paused = false;
                                        fd.next_retry_at = None;
                                        fd.error = Some(err);
                                        fd.state = DownloadState::Failed;
                                        dq.finished.write().await.push(fd.clone());
                                    }
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    *dq.worker_running.write().await = false;
                });
        } else {
            *download_queue.worker_running.write().await = false;
            return Err("Failed to build http client".to_string());
        }
    } else {
        *download_queue.worker_running.write().await = false;
    }
    Ok(())
}

pub(in crate::api) fn start_download_scheduler(
    app_config: Arc<AppConfig>,
    download_cfg: VideoDownloadConfig,
    download_queue: Arc<DownloadQueue>,
    event_manager: Arc<EventManager>,
    active_provider: Arc<ActiveProviderManager>,
    connection_manager: Arc<ConnectionManager>,
    cancel_token: CancellationToken,
) {
    // Bridge task: whenever any provider connection is released, wake only the
    // highest-priority download waiter. This prevents lower-priority downloads
    // from racing ahead of higher-priority ones.
    let capacity_notify = connection_manager.capacity_notified();
    let slot_waiters = Arc::clone(&download_queue.slot_waiters);
    let bridge_active_provider = Arc::clone(&active_provider);
    let bridge_download_cfg = download_cfg.clone();
    let bridge_cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = bridge_cancel_token.cancelled() => break,
                () = capacity_notify.notified() => {}
            }

            let mut capacities_by_input: HashMap<Arc<str>, ProviderCapacities> = HashMap::new();
            let mut ready_waiter = None;

            let mut waiters = slot_waiters.snapshots().await;
            waiters.sort_by_key(|waiter| waiter.priority);

            for waiter in waiters {
                let Some(input_name) = waiter.input_name.as_ref() else {
                    ready_waiter = Some(waiter.id);
                    break;
                };
                let capacities = if let Some(capacities) = capacities_by_input.get(input_name) {
                    capacities.clone()
                } else {
                    let capacities = bridge_active_provider.provider_capacities_for_input(input_name).await;
                    capacities_by_input.insert(Arc::clone(input_name), capacities.clone());
                    capacities
                };

                if !capacities_have_free_slot(&capacities) {
                    continue;
                }
                if background_download_should_wait(waiter.priority, &capacities, &bridge_download_cfg) {
                    continue;
                }
                ready_waiter = Some(waiter.id);
                break;
            }

            if let Some(waiter_id) = ready_waiter {
                let _ = slot_waiters.signal_waiter(waiter_id).await;
            }
        }
    });

    let scheduler_download_cfg = download_cfg;
    let scheduler_cancel_token = cancel_token;
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = scheduler_cancel_token.cancelled() => break,
                _ = interval.tick() => {}
            }

            if download_queue.promote_due_scheduled_now().await == 0 {
                continue;
            }

            broadcast_download_queue_update(&event_manager, &download_queue).await;

            let _ = ensure_download_worker_running(
                &app_config,
                &scheduler_download_cfg,
                &download_queue,
                &event_manager,
                &active_provider,
                &connection_manager,
            )
            .await;
        }
    });
}

pub(in crate::api) fn spawn_download_services(app_state: &AppState, cancel_token: &CancellationToken) {
    let config = app_state.app_config.config.load();
    let Some(download_cfg) = config.video.as_ref().and_then(|video| video.download.as_ref()).cloned() else {
        return;
    };

    start_download_scheduler(
        Arc::clone(&app_state.app_config),
        download_cfg.clone(),
        Arc::clone(&app_state.downloads),
        Arc::clone(&app_state.event_manager),
        Arc::clone(&app_state.active_provider),
        Arc::clone(&app_state.connection_manager),
        cancel_token.clone(),
    );
}

pub(in crate::api) async fn resume_download_worker_if_needed(
    app_state: &AppState,
    download_cfg: &VideoDownloadConfig,
) -> Result<(), String> {
    if app_state.downloads.queue.lock().await.is_empty() && app_state.downloads.active.read().await.is_none() {
        return Ok(());
    }

    ensure_download_worker_running(
        &app_state.app_config,
        download_cfg,
        &app_state.downloads,
        &app_state.event_manager,
        &app_state.active_provider,
        &app_state.connection_manager,
    )
    .await
}

pub async fn queue_download_file(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FileDownloadRequest>,
) -> impl axum::response::IntoResponse + Send {
    let app_config = &*app_state.app_config;

    let config = app_config.config.load();
    if let Some(video_cfg) = config.video.as_ref() {
        if let Some(download_cfg) = video_cfg.download.as_ref() {
            if download_cfg.directory.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "Server config missing video.download.directory configuration"})),
                )
                    .into_response();
            }
            let input_name = req.input_name.map(|s| std::sync::Arc::from(s.as_str()));
            let priority = req.priority.unwrap_or(download_cfg.download_priority);
            match FileDownload::new(req.url.as_str(), req.filename.as_str(), download_cfg, input_name, priority) {
                Some(file_download) => {
                    if let Some(existing) = app_state.downloads.find_duplicate(&file_download).await {
                        info!(
                            "Skipping duplicate download request for {} (matched existing task {})",
                            file_download.url, existing.uuid
                        );
                        return axum::Json(shared::model::FileDownloadDto::from(&existing)).into_response();
                    }
                    info!(
                        "Queueing download {} ({}) from {}",
                        file_download.uuid, file_download.filename, file_download.url
                    );
                    app_state.downloads.queue.lock().await.push_back(file_download.clone());
                    let _ = app_state.downloads.persist_to_disk().await;
                    if app_state.downloads.active.read().await.is_none() {
                        match ensure_download_worker_running(
                            &app_state.app_config,
                            download_cfg,
                            &app_state.downloads,
                            &app_state.event_manager,
                            &app_state.active_provider,
                            &app_state.connection_manager,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(err) => {
                                return (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    axum::Json(json!({"error": err})),
                                )
                                    .into_response()
                            }
                        }
                    }
                    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
                    axum::Json(shared::model::FileDownloadDto::from(&file_download)).into_response()
                }
                None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"})))
                    .into_response(),
            }
        } else {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Server config missing video.download configuration"})),
            )
                .into_response()
        }
    } else {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Server config missing video configuration"})))
            .into_response()
    }
}

pub async fn download_file_info(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    axum::Json(download_queue_snapshot(&app_state.downloads).await)
}

pub async fn queue_recording_file(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FileRecordingRequest>,
) -> impl axum::response::IntoResponse + Send {
    let app_config = &*app_state.app_config;
    let config = app_config.config.load();

    if req.duration_secs == 0 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "duration_secs must be greater than zero"})),
        )
            .into_response();
    }

    if let Some(video_cfg) = config.video.as_ref() {
        if let Some(download_cfg) = video_cfg.download.as_ref() {
            if download_cfg.directory.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "Server config missing video.download.directory configuration"})),
                )
                    .into_response();
            }
            let input_name = req.input_name.map(|s| std::sync::Arc::from(s.as_str()));
            let priority = req.priority.unwrap_or(download_cfg.recording_priority);
            match FileDownload::new_recording(req.url.as_str(), req.filename.as_str(), download_cfg, req.start_at, req.duration_secs, input_name, priority) {
                Some(recording) => {
                    if let Some(existing) = app_state.downloads.find_duplicate(&recording).await {
                        info!(
                            "Skipping duplicate recording request for {} at {} (matched existing task {})",
                            recording.url,
                            recording.start_at.unwrap_or_default(),
                            existing.uuid
                        );
                        return axum::Json(shared::model::FileDownloadDto::from(&existing)).into_response();
                    }
                    app_state.downloads.scheduled.write().await.push(recording.clone());
                    let _ = app_state.downloads.persist_to_disk().await;
                    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
                    axum::Json(shared::model::FileDownloadDto::from(&recording)).into_response()
                }
                None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"})))
                    .into_response(),
            }
        } else {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Server config missing video.download configuration"})),
            )
                .into_response()
        }
    } else {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Server config missing video configuration"})))
            .into_response()
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct DownloadActionRequest {
    uuid: String,
}

pub async fn pause_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let active = { app_state.downloads.active.read().await.clone() };
    if let Some(active) = active {
        if active.uuid == req.uuid {
            app_state.downloads.pause_active().await;
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            return axum::Json(json!({"success": true})).into_response();
        }
    }
    axum::Json(json!({"success": false})).into_response()
}

pub async fn resume_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let active = { app_state.downloads.active.read().await.clone() };
    if let Some(active) = active {
        if active.uuid == req.uuid && active.paused {
            app_state.downloads.resume_active().await;
            let download_cfg = app_state
                .app_config
                .config
                .load()
                .video
                .as_ref()
                .and_then(|video| video.download.as_ref())
                .cloned();
            if let Some(download_cfg) = download_cfg {
                let app_state = Arc::clone(&app_state);
                tokio::spawn(async move {
                    for _ in 0..50 {
                        if !*app_state.downloads.worker_running.read().await {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if let Err(err) = ensure_download_worker_running(
                        &app_state.app_config,
                        &download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                        &app_state.active_provider,
                        &app_state.connection_manager,
                    )
                    .await
                    {
                        error!("Failed to restart resumed download worker: {err}");
                    }
                });
            }
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            return axum::Json(json!({"success": true})).into_response();
        }
    }
    axum::Json(json!({"success": false})).into_response()
}

pub async fn cancel_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let active = { app_state.downloads.active.read().await.clone() };
    if let Some(active) = active {
        if active.uuid == req.uuid {
            let was_paused = active.paused;
            app_state.downloads.cancel_active().await;
            if was_paused {
                if let Some(fd) = app_state.downloads.active.write().await.take() {
                    let mut fd = fd;
                    fd.next_retry_at = None;
                    fd.finished = true;
                    fd.paused = false;
                    fd.state = DownloadState::Cancelled;
                    fd.error.get_or_insert_with(|| "Cancelled by user".to_string());
                    app_state.downloads.finished.write().await.push(fd);
                }
                *app_state.downloads.control_signal.write().await = DownloadControl::None;
                let next_active = app_state.downloads.queue.lock().await.pop_front();
                *app_state.downloads.active.write().await = next_active;
                let _ = app_state.downloads.persist_to_disk().await;
                let config = app_state.app_config.config.load();
                if let Some(download_cfg) = config.video.as_ref().and_then(|video| video.download.as_ref()) {
                    let _ = ensure_download_worker_running(
                        &app_state.app_config,
                        download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                        &app_state.active_provider,
                        &app_state.connection_manager,
                    )
                    .await;
                }
            }
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            return axum::Json(json!({"success": true})).into_response();
        }
    }
    // Remove from queue
    let found = app_state.downloads.remove_from_queue(&req.uuid).await;
    if found {
        broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
    }
    axum::Json(json!({"success": found})).into_response()
}

pub async fn remove_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let removed_from_finished = app_state.downloads.remove_finished(&req.uuid).await;
    let removed_from_queue = app_state.downloads.remove_from_queue(&req.uuid).await;
    if removed_from_finished || removed_from_queue {
        broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
    }
    axum::Json(json!({"success": removed_from_finished || removed_from_queue})).into_response()
}

pub async fn retry_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let retried = app_state.downloads.retry_finished(&req.uuid).await;
    if retried {
        // Start the queue if not running
        let app_config = &app_state.app_config;
        let config = app_config.config.load();
        if let Some(video_cfg) = config.video.as_ref() {
            if let Some(download_cfg) = video_cfg.download.as_ref() {
                if app_state.downloads.active.read().await.is_none() {
                    let _ = ensure_download_worker_running(
                        app_config,
                        download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                        &app_state.active_provider,
                        &app_state.connection_manager,
                    )
                    .await;
                }
            }
        }
    }
    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
    axum::Json(json!({"success": retried})).into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        active_download_snapshot, parse_content_range_total, pause_download, preemption_reason_for, recording_deadline_reached,
        requeue_active_download_for_capacity_wait, requeue_active_download_for_retry, resume_download,
        retryable_transport_error_message, set_active_download_state, should_exit_worker_after_preempt, DownloadActionRequest,
        DOWNLOAD_PREEMPTED_REASON, RECORDING_PREEMPTED_REASON,
    };
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadControl,
            DownloadKind, DownloadQueue, DownloadState, EventManager, FileDownload, MetadataUpdateManager,
            PlaylistStorageState, SharedStreamManager, UpdateGuard,
        },
        model::{AppConfig, Config, ConfigInput, MediaToolCapabilities, ProcessTargets, SourcesConfig},
        utils::{FileLockManager, GeoIp},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::response::IntoResponse;
    use reqwest::header::{HeaderMap, HeaderValue};
    use shared::{model::{ConfigPaths, InputFetchMethod, InputType}, utils::Internable};
    use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
    use tokio::sync::mpsc;
    use tokio::sync::RwLock;

    fn make_download(kind: DownloadKind, state: DownloadState, start_at: Option<i64>, duration_secs: Option<u64>) -> FileDownload {
        FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/file.ts"),
            filename: "file.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/file.ts").expect("valid url"),
            finished: false,
            size: 128,
            total_size: Some(1024),
            paused: false,
            error: Some("transient".to_string()),
            state,
            start_at,
            duration_secs,
            kind,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
        }
    }

    #[test]
    fn recording_deadline_uses_start_plus_duration() {
        let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));
        let normal = make_download(DownloadKind::Download, DownloadState::Downloading, Some(1_000), Some(60));

        assert!(!recording_deadline_reached(&recording, 1_059));
        assert!(recording_deadline_reached(&recording, 1_060));
        assert!(!recording_deadline_reached(&normal, 1_060));
    }

    #[tokio::test]
    async fn retry_requeues_active_download_at_front() {
        let queue = DownloadQueue::new();
        let queued = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
        let active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);

        queue.queue.lock().await.push_back(queued);
        *queue.active.write().await = Some(active);

        requeue_active_download_for_retry(&queue).await;

        assert!(queue.active.read().await.is_none());
        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 2);
        assert_eq!(queued_items[0].state, DownloadState::Queued);
        assert_eq!(queued_items[0].size, 128);
        assert!(queued_items[0].error.is_none());
    }

    #[tokio::test]
    async fn preempted_active_download_requeues_to_capacity_wait_with_partial_progress() {
        let queue = DownloadQueue::new();
        let mut active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        active.size = 512;
        active.total_size = Some(2048);
        *queue.active.write().await = Some(active);

        requeue_active_download_for_capacity_wait(&queue, DOWNLOAD_PREEMPTED_REASON).await;

        assert!(queue.active.read().await.is_none());
        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 1);
        assert_eq!(queued_items[0].state, DownloadState::WaitingForCapacity);
        assert_eq!(queued_items[0].size, 512);
        assert_eq!(queued_items[0].total_size, Some(2048));
        assert_eq!(queued_items[0].error.as_deref(), Some(DOWNLOAD_PREEMPTED_REASON));
    }

    #[tokio::test]
    async fn preempted_active_recording_requeues_with_recording_specific_policy_message() {
        let queue = DownloadQueue::new();
        let mut active = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(600));
        active.size = 512;
        *queue.active.write().await = Some(active);

        requeue_active_download_for_capacity_wait(&queue, RECORDING_PREEMPTED_REASON).await;

        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 1);
        assert_eq!(queued_items[0].kind, DownloadKind::Recording);
        assert_eq!(queued_items[0].state, DownloadState::WaitingForCapacity);
        assert_eq!(queued_items[0].error.as_deref(), Some(RECORDING_PREEMPTED_REASON));
    }

    #[test]
    fn preemption_reason_is_explicit_for_recordings_and_downloads() {
        let download = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));

        assert_eq!(preemption_reason_for(&download), DOWNLOAD_PREEMPTED_REASON);
        assert_eq!(preemption_reason_for(&recording), RECORDING_PREEMPTED_REASON);
    }

    #[test]
    fn only_restart_exits_worker_after_preempt() {
        assert!(!should_exit_worker_after_preempt(DownloadControl::None));
        assert!(!should_exit_worker_after_preempt(DownloadControl::Pause));
        assert!(!should_exit_worker_after_preempt(DownloadControl::Cancel));
        assert!(should_exit_worker_after_preempt(DownloadControl::Restart));
    }

    #[tokio::test]
    async fn set_active_download_state_updates_snapshot_state() {
        let queue = DownloadQueue::new();
        let active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        *queue.active.write().await = Some(active);

        let changed = set_active_download_state(
            &queue,
            DownloadState::WaitingForCapacity,
            Some("waiting".to_string()),
            false,
        )
        .await;

        assert!(changed);
        let active = queue.active.read().await.clone().expect("active download");
        assert_eq!(active.state, DownloadState::WaitingForCapacity);
        assert_eq!(active.error.as_deref(), Some("waiting"));
        assert!(!active.paused);
    }

    #[test]
    fn compute_download_retry_backoff_uses_multiplier_and_cap() {
        let download_cfg = crate::model::VideoDownloadConfig {
            headers: std::collections::HashMap::new(),
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
        };

        assert_eq!(super::compute_download_retry_backoff_secs(1, &download_cfg), 3);
        assert_eq!(super::compute_download_retry_backoff_secs(2, &download_cfg), 9);
        assert_eq!(super::compute_download_retry_backoff_secs(3, &download_cfg), 27);
        assert_eq!(super::compute_download_retry_backoff_secs(8, &download_cfg), 30);
    }

    #[test]
    fn background_download_waits_when_all_candidates_hit_background_limit() {
        let download_cfg = crate::model::VideoDownloadConfig {
            headers: std::collections::HashMap::new(),
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 2,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
        };

        let capacities = vec![(Arc::<str>::from("a"), 2, 5), (Arc::<str>::from("b"), 3, 5)];
        assert!(super::background_download_should_wait(1, &capacities, &download_cfg));
        assert!(!super::background_download_should_wait(0, &capacities, &download_cfg));
    }

    #[test]
    fn background_download_waits_when_reserved_user_slots_would_be_consumed() {
        let download_cfg = crate::model::VideoDownloadConfig {
            headers: std::collections::HashMap::new(),
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 1,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
        };

        let blocked = vec![(Arc::<str>::from("a"), 4, 5), (Arc::<str>::from("b"), 4, 5)];
        let allowed = vec![(Arc::<str>::from("a"), 3, 5), (Arc::<str>::from("b"), 4, 6)];
        assert!(super::background_download_should_wait(1, &blocked, &download_cfg));
        assert!(!super::background_download_should_wait(1, &allowed, &download_cfg));
    }

    #[test]
    fn retryable_transport_error_message_detects_common_transient_failures() {
        assert!(retryable_transport_error_message("dns lookup failed"));
        assert!(retryable_transport_error_message("connection reset by peer"));
        assert!(retryable_transport_error_message("operation timed out"));
        assert!(!retryable_transport_error_message("invalid URL"));
    }

    #[tokio::test]
    async fn active_download_snapshot_releases_read_lock_before_followup_write() {
        let active = Arc::new(RwLock::new(Some(FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/file.bin"),
            filename: "deadlock-test.bin".to_string(),
            url: reqwest::Url::parse("https://example.com/file.bin").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
        })));
        let snapshot = active_download_snapshot(&active).await;
        assert!(snapshot.is_some());

        let write_result = tokio::time::timeout(Duration::from_millis(100), active.write()).await;
        assert!(write_result.is_ok(), "write lock should not be blocked by snapshot helper");
    }

    #[test]
    fn parse_content_range_total_extracts_full_size() {
        let mut headers = HeaderMap::new();
        headers.insert("content-range", HeaderValue::from_static("bytes 512-1023/4096"));

        assert_eq!(parse_content_range_total(&headers), Some(4096));
    }

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig {
            inputs: vec![input],
            ..SourcesConfig::default()
        };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_app_state() -> Arc<AppState> {
        let app_cfg = Arc::new(create_test_app_config());
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        let tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<Arc<ProcessTargets>>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    #[tokio::test]
    async fn pause_and_resume_handlers_return_without_hanging() {
        let app_state = create_test_app_state();
        let active = FileDownload {
            uuid: "handler-id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/handler-file.bin"),
            filename: "handler-file.bin".to_string(),
            url: reqwest::Url::parse("https://example.com/file.bin").expect("valid url"),
            finished: false,
            size: 32,
            total_size: Some(64),
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
        };
        *app_state.downloads.active.write().await = Some(active);

        let pause_response = tokio::time::timeout(
            Duration::from_millis(100),
            pause_download(
                axum::extract::State(Arc::clone(&app_state)),
                axum::extract::Json(DownloadActionRequest {
                    uuid: "handler-id".to_string(),
                }),
            ),
        )
        .await;
        assert!(pause_response.is_ok(), "pause handler should return promptly");

        let resume_response = tokio::time::timeout(
            Duration::from_millis(100),
            resume_download(
                axum::extract::State(app_state),
                axum::extract::Json(DownloadActionRequest {
                    uuid: "handler-id".to_string(),
                }),
            ),
        )
        .await;
        assert!(resume_response.is_ok(), "resume handler should return promptly");

        let _ = pause_response.expect("pause response").into_response();
        let _ = resume_response.expect("resume response").into_response();
    }
}
