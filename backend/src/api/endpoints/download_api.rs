use crate::{
    api::model::{
        AppState, DownloadControl, DownloadQueue, DownloadState, EventManager, EventMessage, FileDownload,
        FileDownloadRequest, FileRecordingRequest,
    },
    model::{AppConfig, VideoDownloadConfig},
    utils::{async_file_writer, request, request::create_client, IO_BUFFER_SIZE},
};
use axum::response::IntoResponse;
use futures::stream::TryStreamExt;
use log::{debug, info, warn};
use serde::Deserialize;
use serde_json::{json, Value};
use shared::{error::to_io_error, model::DownloadsResponse, utils::bytes_to_megabytes};
use std::{ops::Deref, sync::Arc};
use tokio::{fs, io::AsyncWriteExt, sync::RwLock, time::{self, Duration, Instant}};

const DOWNLOAD_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const DOWNLOAD_PROGRESS_LOG_BYTES: u64 = 16 * 1024 * 1024;

enum DownloadExecutionResult {
    Completed,
    Paused,
    Cancelled,
    Retryable(String),
    Failed(String),
}

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
    let downloads = download_queue
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
        .map(shared::model::FileDownloadDto::from);

    DownloadsResponse {
        completed: active.is_none(),
        queue,
        downloads,
        active,
    }
}

async fn broadcast_download_queue_update(event_manager: &Arc<EventManager>, download_queue: &DownloadQueue) {
    let snapshot = download_queue_snapshot(download_queue).await;
    let _ = event_manager.send_event(EventMessage::DownloadsUpdate(snapshot));
}

#[allow(clippy::too_many_lines)]
async fn download_file(
    active: Arc<RwLock<Option<FileDownload>>>,
    client: &reqwest::Client,
    control_signal: Arc<RwLock<DownloadControl>>,
    event_manager: Option<&Arc<EventManager>>,
    download_queue: Option<&Arc<DownloadQueue>>,
) -> DownloadExecutionResult {
    if let Some(file_download) = active_download_snapshot(&active).await {
        let url = file_download.url.clone();
        let file_path = file_download.file_path.clone();
        let _file_path_str = file_path.to_str().unwrap_or("unknown");

        // Check for existing partial file for resume
        let existing_size = if file_path.exists() {
            tokio::fs::metadata(&file_path).await.map_or(0, |m| m.len())
        } else {
            0
        };

        let mut request_builder = client.get(url.clone());
        if existing_size > 0 {
            request_builder = request_builder.header("Range", format!("bytes={existing_size}-"));
        }

        match request_builder.send().await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                    return DownloadExecutionResult::Failed(format!("Download request failed for {url} with HTTP {status}"));
                }
                let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

                let total_size = response.content_length().or_else(|| {
                    if is_resume {
                        response.headers().get("content-range").and_then(|v| {
                            v.to_str().ok().and_then(|s| {
                                s.split('/').next_back().and_then(|total| total.parse::<u64>().ok())
                            })
                        })
                    } else {
                        None
                    }
                });

                if let Some(total) = total_size {
                    if let Some(download) = active.write().await.as_mut() {
                        download.total_size = Some(total);
                    }
                    if let (Some(event_manager), Some(download_queue)) = (event_manager, download_queue) {
                        broadcast_download_queue_update(event_manager, download_queue).await;
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
                                    let mut stream = response.bytes_stream().map_err(to_io_error);
                                    let mut write_counter = 0;
                                    let mut saw_first_chunk = existing_size > 0;
                                    let mut last_progress_log_at = Instant::now();
                                    let mut last_progress_logged_bytes = downloaded;

                                    loop {
                                        match *control_signal.read().await {
                                            DownloadControl::Pause => {
                                                if let Some(download) = active.write().await.as_mut() {
                                                    download.paused = true;
                                                    download.state = DownloadState::Paused;
                                                }
                                                if let Err(err) = buf_writer.flush().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                if let Err(err) = buf_writer.shutdown().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                return DownloadExecutionResult::Paused;
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
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                if let Err(err) = buf_writer.shutdown().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                return DownloadExecutionResult::Cancelled;
                                            }
                                            DownloadControl::None => {}
                                        }

                                        let deadline_reached = active
                                            .read()
                                            .await
                                            .as_ref()
                                            .is_some_and(|download| {
                                                recording_deadline_reached(download, chrono::Utc::now().timestamp())
                                            });
                                        if deadline_reached {
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

                                        match stream.try_next().await {
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
                                                                            let percent =
                                                                                ((downloaded as f64 / total as f64) * 100.0).round();
                                                                            debug!(
                                                                                "Download progress for {file_path_str}: {}MB / {}MB ({}%)",
                                                                                bytes_to_megabytes(downloaded),
                                                                                bytes_to_megabytes(total),
                                                                                percent as u32
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
                                                                        broadcast_download_queue_update(
                                                                            event_manager,
                                                                            download_queue,
                                                                        )
                                                                            .await;
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
                                                                    broadcast_download_queue_update(
                                                                        event_manager,
                                                                        download_queue,
                                                                    )
                                                                        .await;
                                                                }
                                                            }
                                                            if let Some(lock) = active.write().await.as_mut() {
                                                                lock.size = downloaded;
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
                                            Err(err) => {
                                                return DownloadExecutionResult::Retryable(format!(
                                                    "Error while downloading file: {file_path_str} {err}"
                                                ))
                                            }
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
            Err(err) => DownloadExecutionResult::Retryable(format!("Error while opening url: {} {}", &url, err)),
        }
    } else {
        DownloadExecutionResult::Failed("No active file download".to_string())
    }
}

async fn requeue_active_download_for_retry(download_queue: &DownloadQueue) {
    if let Some(mut download) = download_queue.active.write().await.take() {
        download.finished = false;
        download.paused = false;
        download.error = None;
        download.state = DownloadState::Queued;
        download_queue.queue.lock().await.push_front(download);
    }
    let _ = download_queue.persist_to_disk().await;
}

pub(in crate::api) async fn ensure_download_worker_running(
    cfg: &AppConfig,
    download_cfg: &VideoDownloadConfig,
    download_queue: &Arc<DownloadQueue>,
    event_manager: &Arc<EventManager>,
) -> Result<(), String> {
    if *download_queue.worker_running.read().await {
        debug!("Download worker already running");
        return Ok(());
    }

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
        let event_manager = Arc::clone(event_manager);

        match create_client(cfg).default_headers(headers).build() {
            Ok(client) => {
                if let Some(active) = dq.active.read().await.as_ref() {
                    info!(
                        "Starting download worker for active download {} ({})",
                        active.uuid, active.filename
                    );
                }
                *dq.worker_running.write().await = true;
                tokio::spawn(async move {
                    loop {
                        if dq.active.read().await.deref().is_some() {
                            if let Some(download) = dq.active.read().await.as_ref() {
                                if download.paused {
                                    break;
                                }
                            }

                            *control_signal.write().await = DownloadControl::None;

                            match download_file(
                                Arc::clone(&dq.active),
                                &client,
                                Arc::clone(&control_signal),
                                Some(&event_manager),
                                Some(&dq),
                            )
                            .await
                            {
                                DownloadExecutionResult::Completed => {
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        fd.state = DownloadState::Completed;
                                        dq.finished.write().await.push(fd.clone());
                                    }
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                }
                                DownloadExecutionResult::Paused => {
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                    break;
                                }
                                DownloadExecutionResult::Cancelled => {
                                    if let Some(fd) = dq.active.write().await.take() {
                                        dq.finished.write().await.push(fd);
                                    }
                                    *dq.control_signal.write().await = DownloadControl::None;
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                }
                                DownloadExecutionResult::Retryable(_err) => {
                                    warn!("Retrying active download after transient failure");
                                    requeue_active_download_for_retry(&dq).await;
                                    time::sleep(Duration::from_secs(1)).await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                    broadcast_download_queue_update(&event_manager, &dq).await;
                                }
                                DownloadExecutionResult::Failed(err) => {
                                    warn!("Download failed permanently: {err}");
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        fd.paused = false;
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
            }
            Err(_) => return Err("Failed to build http client".to_string()),
        }
    }
    Ok(())
}

pub(in crate::api) fn start_download_scheduler(
    app_config: Arc<AppConfig>,
    download_queue: Arc<DownloadQueue>,
    event_manager: Arc<EventManager>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if download_queue.promote_due_scheduled_now().await == 0 {
                continue;
            }

            let config = app_config.config.load();
            let Some(download_cfg) = config.video.as_ref().and_then(|video| video.download.as_ref()) else {
                continue;
            };

            let _ = ensure_download_worker_running(&app_config, download_cfg, &download_queue, &event_manager).await;
        }
    });
}

macro_rules! download_info {
    ($file_download:expr) => {
       json!({"uuid": $file_download.uuid, "filename":  $file_download.filename,
       "kind": $file_download.kind,
       "filesize": $file_download.size, "total_size": $file_download.total_size, "finished": $file_download.finished,
       "paused": $file_download.paused, "state": $file_download.state, "start_at": $file_download.start_at,
       "duration_secs": $file_download.duration_secs, "error": $file_download.error})
    }
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
            match FileDownload::new(req.url.as_str(), req.filename.as_str(), download_cfg) {
                Some(file_download) => {
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
                    axum::Json(download_info!(&file_download)).into_response()
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
    let mut queue_list: Vec<Value> = app_state.downloads.queue.lock().await.iter().map(|fd| download_info!(fd)).collect();
    queue_list.extend(app_state.downloads.scheduled.read().await.iter().map(|fd| download_info!(fd)));
    let finished_list: Vec<Value> =
        app_state.downloads.finished.read().await.iter().map(|fd| download_info!(fd)).collect();

    (*app_state.downloads.active.read().await).as_ref().map_or_else(
        || {
            axum::Json(json!({
                "completed": true, "queue": queue_list, "downloads": finished_list
            }))
        },
        |file_download| {
            axum::Json(json!({
                "completed": false, "queue": queue_list, "downloads": finished_list, "active": download_info!(file_download)
            }))
        },
    )
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
            match FileDownload::new_recording(req.url.as_str(), req.filename.as_str(), download_cfg, req.start_at, req.duration_secs) {
                Some(recording) => {
                    app_state.downloads.scheduled.write().await.push(recording.clone());
                    let _ = app_state.downloads.persist_to_disk().await;
                    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
                    axum::Json(download_info!(&recording)).into_response()
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
    // Check if it's the active download
    if let Some(active) = app_state.downloads.active.read().await.as_ref() {
        if active.uuid == req.uuid {
            app_state.downloads.pause_active().await;
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            return axum::Json(json!({"success": true})).into_response();
        }
    }
    // Check queue
    let found = app_state.downloads.remove_from_queue(&req.uuid).await;
    if found {
        broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
    }
    axum::Json(json!({"success": found})).into_response()
}

pub async fn resume_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    if let Some(active) = app_state.downloads.active.read().await.as_ref() {
        if active.uuid == req.uuid && active.paused {
            app_state.downloads.resume_active().await;
            let app_config = &app_state.app_config;
            let config = app_config.config.load();
            if let Some(video_cfg) = config.video.as_ref() {
                if let Some(download_cfg) = video_cfg.download.as_ref() {
                    let _ = ensure_download_worker_running(
                        app_config,
                        download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                    )
                    .await;
                }
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
    if let Some(active) = app_state.downloads.active.read().await.as_ref() {
        if active.uuid == req.uuid {
            app_state.downloads.cancel_active().await;
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
    use super::{active_download_snapshot, recording_deadline_reached, requeue_active_download_for_retry};
    use crate::api::model::{DownloadKind, DownloadQueue, DownloadState, FileDownload};
    use std::{path::PathBuf, sync::Arc, time::Duration};
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
        })));
        let snapshot = active_download_snapshot(&active).await;
        assert!(snapshot.is_some());

        let write_result = tokio::time::timeout(Duration::from_millis(100), active.write()).await;
        assert!(write_result.is_ok(), "write lock should not be blocked by snapshot helper");
    }
}
