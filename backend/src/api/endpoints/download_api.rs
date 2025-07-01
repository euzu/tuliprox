use crate::api::model::app_state::AppState;
use crate::api::model::download::{DownloadQueue, FileDownload, FileDownloadRequest};
use crate::model::{AppConfig, Config, VideoDownloadConfig};
use crate::utils::request;
use tokio::sync::RwLock;
use futures::stream::TryStreamExt;
use log::info;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{Write};
use std::ops::Deref;
use std::sync::Arc;
use std::{fs};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use axum::response::IntoResponse;
use shared::utils::bytes_to_megabytes;
use shared::error::to_io_error;
use crate::utils::request::create_client;

async fn download_file(active: Arc<RwLock<Option<FileDownload>>>, client: &reqwest::Client) -> Result<(), String> {
    let file_download = { active.read().await.as_ref().unwrap().clone() };
    match client.get(file_download.url.clone()).send().await {
        Ok(response) => {
            match fs::create_dir_all(&file_download.file_dir) {
                Ok(()) => {
                    if let Some(file_path_str) = file_download.file_path.to_str() {
                        info!("Downloading {file_path_str}");
                        match File::create(&file_download.file_path) {
                            Ok(mut file) => {
                                let mut downloaded: u64 = 0;
                                let mut stream = response.bytes_stream().map_err(to_io_error);
                                loop {
                                    match stream.try_next().await {
                                        Ok(item) => {
                                            if let Some(chunk) = item {
                                                match file.write_all(&chunk) {
                                                    Ok(()) => {
                                                        downloaded += chunk.len() as u64;
                                                        active.write().await.as_mut().unwrap().size = downloaded;
                                                    }
                                                    Err(err) => return Err(format!("Error while writing to file: {file_path_str} {err}"))
                                                }
                                            } else {
                                                let megabytes = bytes_to_megabytes(downloaded);
                                                info!("Downloaded {file_path_str}, filesize: {megabytes}MB");
                                                active.write().await.as_mut().unwrap().size = downloaded;
                                                return Ok(());
                                            }
                                        }
                                        Err(err) => return Err(format!("Error while writing to file: {file_path_str} {err}"))
                                    }
                                }
                            }
                            Err(err) => Err(format!("Error while writing to file: {file_path_str} {err}"))
                        }
                    } else {
                        Err("Error file-download file-path unknown".to_string())
                    }
                }
                Err(err) => Err(format!("Error while creating directory for file: {} {}", &file_download.file_dir.to_str().unwrap_or("?"), err))
            }
        }
        Err(err) => Err(format!("Error while opening url: {} {}", &file_download.url, err))
    }
}

async fn run_download_queue(cfg: &AppConfig, download_cfg: &VideoDownloadConfig, download_queue: &Arc<DownloadQueue>) -> Result<(), String> {
    let next_download = download_queue.as_ref().queue.lock().await.pop_front();
    if next_download.is_some() {
        { *download_queue.as_ref().active.write().await = next_download; }
        let headers = request::get_request_headers(Some(&download_cfg.headers), None);
        let dq = Arc::clone(download_queue);

        match create_client(cfg).default_headers(headers).build() {
            Ok(client) => {
                tokio::spawn(async move {
                    loop {
                        if dq.active.read().await.deref().is_some() {
                            match download_file(Arc::clone(&dq.active), &client).await {
                                Ok(()) => {
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        dq.finished.write().await.push(fd.clone());
                                    }
                                }
                                Err(err) => {
                                    if let Some(fd) = &mut *dq.active.write().await {
                                        fd.finished = true;
                                        fd.error = Some(err);
                                        dq.finished.write().await.push(fd.clone());
                                    }
                                }
                            }
                            *dq.active.write().await = dq.queue.lock().await.pop_front();
                        } else {
                            break;
                        }
                    }
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
       "filesize": $file_download.size, "finished": $file_download.finished,
       "error": $file_download.error})
    }
}

pub async fn queue_download_file(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FileDownloadRequest>,
) ->  impl axum::response::IntoResponse + Send {
    let app_config = &*app_state.app_config;

    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config);
    if let Some(download_cfg) = config.video.as_ref().unwrap().download.as_ref() {
        if download_cfg.directory.is_none() {
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Server config missing video.download.directory configuration"}))).into_response();
        }
        match FileDownload::new(req.url.as_str(), req.filename.as_str(), download_cfg) {
            Some(file_download) => {
                app_state.downloads.queue.lock().await.push_back(file_download.clone());
                if app_state.downloads.active.read().await.is_none() {
                    match run_download_queue(&app_state.app_config, download_cfg, &app_state.downloads).await {
                        Ok(()) => {}
                        Err(err) => return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err}))).into_response(),
                    }
                }
                axum::Json(download_info!(&file_download)).into_response()
            }
            None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response(),
        }
    } else {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Server config missing video.download configuration"}))).into_response()
    }
}

pub async fn download_file_info(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) ->  impl axum::response::IntoResponse + Send {
    let finished_list: &[Value] = &app_state.downloads.finished.write().await.drain(..)
        .map(|fd| download_info!(fd)).collect::<Vec<Value>>();

    (*app_state.downloads.active.read().await).as_ref().map_or_else(|| axum::Json(json!({
            "completed": true, "downloads": finished_list
        })), |file_download| axum::Json(json!({
            "completed": false, "downloads": finished_list, "active": download_info!(file_download)
        })))
}