use crate::{
    api::model::{
        AppState, DownloadControl, DownloadQueue, DownloadState, FileDownload, FileDownloadRequest, FileRecordingRequest,
    },
    model::{AppConfig, VideoDownloadConfig},
    utils::{async_file_writer, request, request::create_client, IO_BUFFER_SIZE},
};
use axum::response::IntoResponse;
use futures::stream::TryStreamExt;
use log::info;
use serde::Deserialize;
use serde_json::{json, Value};
use shared::{error::to_io_error, utils::bytes_to_megabytes};
use std::{ops::Deref, sync::Arc};
use tokio::{fs, io::AsyncWriteExt, sync::RwLock};

enum DownloadExecutionResult {
    Completed,
    Paused,
    Cancelled,
}

async fn download_file(
    active: Arc<RwLock<Option<FileDownload>>>,
    client: &reqwest::Client,
    control_signal: Arc<RwLock<DownloadControl>>,
) -> Result<DownloadExecutionResult, String> {
    if let Some(file_download) = active.read().await.as_ref().as_ref() {
        let url = file_download.url.clone();
        let file_path = file_download.file_path.clone();
        let _file_path_str = file_path.to_str().unwrap_or("unknown");

        // Check for existing partial file for resume
        let existing_size = if file_path.exists() {
            tokio::fs::metadata(&file_path).await.map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        let mut request_builder = client.get(url.clone());
        if existing_size > 0 {
            request_builder = request_builder.header("Range", format!("bytes={}-", existing_size));
        }

        match request_builder.send().await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                    return Err(format!("Download request failed for {} with HTTP {}", &url, status));
                }
                let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

                let total_size = response.content_length().or_else(|| {
                    if is_resume {
                        response.headers().get("content-range").and_then(|v| {
                            v.to_str().ok().and_then(|s| {
                                s.split('/').last().and_then(|total| total.parse::<u64>().ok())
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

                                    loop {
                                        match *control_signal.read().await {
                                            DownloadControl::Pause => {
                                                if let Some(download) = active.write().await.as_mut() {
                                                    download.paused = true;
                                                    download.state = DownloadState::Paused;
                                                }
                                                buf_writer.flush().await.map_err(|err| err.to_string())?;
                                                buf_writer.shutdown().await.map_err(|err| err.to_string())?;
                                                return Ok(DownloadExecutionResult::Paused);
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
                                                buf_writer.flush().await.map_err(|err| err.to_string())?;
                                                buf_writer.shutdown().await.map_err(|err| err.to_string())?;
                                                return Ok(DownloadExecutionResult::Cancelled);
                                            }
                                            DownloadControl::None => {}
                                        }

                                        match stream.try_next().await {
                                            Ok(item) => {
                                                if let Some(chunk) = item {
                                                    match buf_writer.write_all(&chunk).await {
                                                        Ok(()) => {
                                                            write_counter += chunk.len();
                                                            if write_counter >= IO_BUFFER_SIZE {
                                                                buf_writer.flush().await.map_err(|err| err.to_string())?;
                                                                write_counter = 0;
                                                            }

                                                            downloaded += chunk.len() as u64;
                                                            if let Some(lock) = active.write().await.as_mut() {
                                                                lock.size = downloaded;
                                                            }
                                                        }
                                                        Err(err) => {
                                                            return Err(format!(
                                                                "Error while writing to file: {file_path_str} {err}"
                                                            ))
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
                                                    buf_writer.flush().await.map_err(|err| err.to_string())?;
                                                    buf_writer.shutdown().await.map_err(|err| err.to_string())?;
                                                    return Ok(DownloadExecutionResult::Completed);
                                                }
                                            }
                                            Err(err) => {
                                                return Err(format!("Error while writing to file: {file_path_str} {err}"))
                                            }
                                        }
                                    }
                                }
                                Err(err) => Err(format!("Error while opening file: {file_path_str} {err}")),
                            }
                        } else {
                            Err("Error file-download file-path unknown".to_string())
                        }
                    }
                    Err(err) => Err(format!(
                        "Error while creating directory for file: {} {}",
                        &file_download.file_dir.to_str().unwrap_or("?"),
                        err
                    )),
                }
            }
            Err(err) => Err(format!("Error while opening url: {} {}", &url, err)),
        }
    } else {
        Err("No active file download".to_string())
    }
}

pub(in crate::api) async fn ensure_download_worker_running(
    cfg: &AppConfig,
    download_cfg: &VideoDownloadConfig,
    download_queue: &Arc<DownloadQueue>,
) -> Result<(), String> {
    if *download_queue.worker_running.read().await {
        return Ok(());
    }

    if download_queue.active.read().await.is_none() {
        let next_download = download_queue.as_ref().queue.lock().await.pop_front();
        if let Some(next_download) = next_download {
            *download_queue.as_ref().active.write().await = Some(next_download);
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

        match create_client(cfg).default_headers(headers).build() {
            Ok(client) => {
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

                            match download_file(Arc::clone(&dq.active), &client, Arc::clone(&control_signal)).await {
                                Ok(DownloadExecutionResult::Completed) => {
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        fd.state = DownloadState::Completed;
                                        dq.finished.write().await.push(fd.clone());
                                    }
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                }
                                Ok(DownloadExecutionResult::Paused) => {
                                    let _ = dq.persist_to_disk().await;
                                    break;
                                }
                                Ok(DownloadExecutionResult::Cancelled) => {
                                    if let Some(fd) = dq.active.write().await.take() {
                                        dq.finished.write().await.push(fd);
                                    }
                                    *dq.control_signal.write().await = DownloadControl::None;
                                    let _ = dq.persist_to_disk().await;
                                    *dq.active.write().await = dq.queue.lock().await.pop_front();
                                    let _ = dq.persist_to_disk().await;
                                }
                                Err(err) => {
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
                    app_state.downloads.queue.lock().await.push_back(file_download.clone());
                    let _ = app_state.downloads.persist_to_disk().await;
                    if app_state.downloads.active.read().await.is_none() {
                        match ensure_download_worker_running(&app_state.app_config, download_cfg, &app_state.downloads).await {
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
            return axum::Json(json!({"success": true})).into_response();
        }
    }
    // Check queue
    let found = app_state.downloads.remove_from_queue(&req.uuid).await;
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
                    let _ = ensure_download_worker_running(app_config, download_cfg, &app_state.downloads).await;
                }
            }
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
            return axum::Json(json!({"success": true})).into_response();
        }
    }
    // Remove from queue
    let found = app_state.downloads.remove_from_queue(&req.uuid).await;
    axum::Json(json!({"success": found})).into_response()
}

pub async fn remove_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let removed_from_finished = app_state.downloads.remove_finished(&req.uuid).await;
    let removed_from_queue = app_state.downloads.remove_from_queue(&req.uuid).await;
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
                    let _ = ensure_download_worker_running(app_config, download_cfg, &app_state.downloads).await;
                }
            }
        }
    }
    axum::Json(json!({"success": retried})).into_response()
}
