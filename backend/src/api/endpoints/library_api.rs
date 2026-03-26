use crate::{api::{
    library_scan::{spawn_library_scan, LibraryScanTaskOptions},
    model::{AppState, EventMessage},
}, auth::permission_layer, library::{resolve_metadata_storage_path, LibraryProcessor, MetadataStorage}};
use axum::response::IntoResponse;
use log::{debug, warn};
use serde_json::json;
use shared::model::{permission::Permission, LibraryScanRequest, LibraryScanSummary, LibraryScanSummaryStatus, LibraryStatus};
use std::sync::Arc;

// Triggers a library scan
async fn scan_library(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::Json(request): axum::Json<LibraryScanRequest>,
) -> axum::response::Response {
    debug!("Library scan requested (force_rescan: {})", request.force_rescan);

    let Some(permit) = app_state.update_guard.try_library() else {
        warn!("Library update already in progress; update skipped.");
        let response = LibraryScanSummary {
            status: LibraryScanSummaryStatus::Error,
            message: "Library update already in progress.".to_string(),
            result: None,
        };
        let _ = app_state.event_manager.send_event(EventMessage::LibraryScanProgress(response));
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "Library update already in progress.".to_string()})),
        )
            .into_response();
    };

    // Check if Library is enabled
    let (lib_config, metadata_update_config, storage_dir) = {
        let config = app_state.app_config.config.load();
        match config.library.as_ref() {
            Some(lib) if lib.enabled => (lib.clone(), config.metadata_update.clone(), config.storage_dir.clone()),
            _ => {
                let response = LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Error,
                    message: "Library is not enabled".to_string(),
                    result: None,
                };
                let _ = app_state.event_manager.send_event(EventMessage::LibraryScanProgress(response));
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "Library is not enabled".to_string()})),
                )
                    .into_response();
            }
        }
    };
    let client = app_state.http_client.load_full().as_ref().clone();
    let event_manager = Arc::clone(&app_state.event_manager);
    spawn_library_scan(
        event_manager,
        lib_config,
        metadata_update_config,
        client,
        LibraryScanTaskOptions { force_rescan: request.force_rescan, message_prefix: "", storage_dir },
        permit,
    );

    axum::http::StatusCode::ACCEPTED.into_response()
}

/// Gets Library status
async fn get_library_status(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    let config_snapshot = app_state.app_config.config.load();
    if let Some(config) = config_snapshot.library.as_ref() {
        if config.enabled {
            let client = app_state.http_client.load_full().as_ref().clone();
            // Get statistics from processor
            let processor =
                LibraryProcessor::new(
                    config.clone(),
                    config_snapshot.metadata_update.as_ref(),
                    client,
                    &config_snapshot.storage_dir,
                );
            let entries = processor.get_all_entries().await;

            let movies = entries.iter().filter(|e| e.metadata.is_movie()).count();
            let series = entries.iter().filter(|e| e.metadata.is_series()).count();

            let response = LibraryStatus {
                enabled: true,
                total_items: entries.len(),
                movies,
                series,
                path: Some(
                    resolve_metadata_storage_path(config_snapshot.metadata_update.as_ref(), &config_snapshot.storage_dir)
                        .to_string_lossy()
                        .to_string(),
                ),
            };

            return axum::Json(response).into_response();
        }
    }

    let response = LibraryStatus::default();
    axum::Json(response).into_response()
}

async fn get_thumbnail(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let config_snapshot = app_state.app_config.config.load();
    let Some(library_config) = config_snapshot.library.as_ref().filter(|l| l.enabled) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    if !library_config.thumbnails.enabled {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    let storage_path = resolve_metadata_storage_path(
        config_snapshot.metadata_update.as_ref(),
        &config_snapshot.storage_dir,
    );
    let storage = MetadataStorage::new(storage_path);

    if let Some(entry) = storage.load_by_uuid(&id).await {
        if let Some(hash) = entry.thumbnail_hash.as_ref() {
            let mtime = entry.thumbnail_mtime.unwrap_or(0);
            let etag = format!("\"{hash}-{mtime}\"");
            return serve_thumbnail_hash(&storage, hash, etag, &headers).await;
        }
    }

    let etag = format!("\"{id}\"");
    serve_thumbnail_hash(&storage, &id, etag, &headers).await
}

async fn serve_thumbnail_hash(
    storage: &MetadataStorage,
    hash: &str,
    etag: String,
    headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    if let Some(if_none_match) = headers.get(axum::http::header::IF_NONE_MATCH) {
        if if_none_match.as_bytes() == etag.as_bytes() {
            return axum::http::StatusCode::NOT_MODIFIED.into_response();
        }
    }

    let thumb_path = storage.get_thumbnail_path(hash);
    match tokio::fs::read(&thumb_path).await {
        Ok(data) => {
            let headers = [
                (axum::http::header::CONTENT_TYPE, "image/jpeg".to_string()),
                (axum::http::header::CACHE_CONTROL, "max-age=86400, public".to_string()),
                (axum::http::header::ETAG, etag),
            ];
            (headers, data).into_response()
        }
        Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// Registers Library API routes.
pub fn library_api_register(
    router: axum::Router<Arc<AppState>>,
    app_state: Option<&Arc<AppState>>,
) -> axum::Router<Arc<AppState>> {
    match app_state {
        Some(app_state) => router
            .route(
                "/library/status",
                axum::routing::get(get_library_status).layer(permission_layer!(app_state, Permission::LibraryRead)),
            )
            .route(
                "/library/scan",
                axum::routing::post(scan_library).layer(permission_layer!(app_state, Permission::LibraryWrite)),
            )
            .route("/library/thumbnail/{uuid}", axum::routing::get(get_thumbnail)),
        None => router
            .route("/library/scan", axum::routing::post(scan_library))
            .route("/library/status", axum::routing::get(get_library_status))
            .route("/library/thumbnail/{uuid}", axum::routing::get(get_thumbnail)),
    }
}