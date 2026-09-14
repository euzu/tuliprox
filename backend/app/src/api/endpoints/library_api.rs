use crate::{
    api::{
        auth_middleware::permission_layer,
        model::AppState,
        tasks::{spawn_library_scan, LibraryScanTaskOptions},
    },
    library::{resolve_metadata_storage_path, LibraryProcessor, MetadataStorage},
};
use axum::response::IntoResponse;
use log::{debug, warn};
use serde_json::json;
use shared::model::{permission::Permission, LibraryScanRequest, LibraryStatus, OperationRunAccepted};
use std::sync::Arc;

// Triggers a library scan
async fn scan_library(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::Json(request): axum::Json<LibraryScanRequest>,
) -> axum::response::Response {
    debug!("Library scan requested (force_rescan: {})", request.force_rescan);

    let Some(permit) = app_state.update_guard.try_library() else {
        warn!("Library update already in progress; update skipped.");
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

    (axum::http::StatusCode::ACCEPTED, axum::Json(OperationRunAccepted::default())).into_response()
}

/// Gets Library status
async fn get_library_status(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    let config_snapshot = app_state.app_config.config.load();
    match read_library_status(&config_snapshot, app_state.http_client.load_full().as_ref().clone()).await {
        Ok(status) => axum::Json(status).into_response(),
        Err(err) => {
            log::error!("Failed to read Library catalog status: {err}");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// Both counts and the exposed path refer to the scanner's canonical catalog.
async fn read_library_status(
    config: &tuliprox_core::model::Config,
    client: reqwest::Client,
) -> std::io::Result<LibraryStatus> {
    let Some(library) = config.library.as_ref().filter(|library| library.enabled) else {
        return Ok(LibraryStatus::default());
    };
    let processor =
        LibraryProcessor::new(library.clone(), config.metadata_update.as_ref(), client, &config.storage_dir);
    let mut status = processor.catalog_status().await?;
    status.path = Some(
        resolve_metadata_storage_path(config.metadata_update.as_ref(), &config.storage_dir)
            .to_string_lossy()
            .into_owned(),
    );
    Ok(status)
}

async fn get_thumbnail(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    // ids/hashes are hex-like tokens; anything else could traverse the storage path
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    let config_snapshot = app_state.app_config.config.load();
    let Some(library_config) = config_snapshot.library.as_ref().filter(|l| l.enabled) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    if !library_config.thumbnails.enabled {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    let storage_path =
        resolve_metadata_storage_path(config_snapshot.metadata_update.as_ref(), &config_snapshot.storage_dir);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn library_status_read_uses_canonical_catalog_with_episodes_without_a_new_scan() {
        use crate::library::{EpisodeMetadata, MediaMetadata, MetadataCacheEntry, SeriesMetadata};
        use shared::model::LibraryConfigDto;
        use tuliprox_core::model::{Config, LibraryConfig, MetadataUpdateConfig};
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_dir: temp.path().to_string_lossy().into_owned(),
            library: Some(LibraryConfig::from(&LibraryConfigDto { enabled: true, ..LibraryConfigDto::default() })),
            metadata_update: Some(MetadataUpdateConfig {
                cache_path: "canonical".into(),
                ..MetadataUpdateConfig::default()
            }),
            ..Config::default()
        };
        let storage =
            MetadataStorage::new(resolve_metadata_storage_path(config.metadata_update.as_ref(), &config.storage_dir));
        storage.initialize().await.unwrap();
        storage
            .store(&MetadataCacheEntry::new(
                "/not-scanned".into(),
                0,
                0,
                MediaMetadata::Series(SeriesMetadata {
                    episodes: Some(vec![EpisodeMetadata::default(); 3]),
                    number_of_episodes: 999,
                    ..SeriesMetadata::default()
                }),
            ))
            .await
            .unwrap();
        let status = read_library_status(&config, reqwest::Client::new()).await.unwrap();
        assert_eq!((status.movies, status.series, status.episodes, status.total_items), (0, 1, 3, 1));
        assert_eq!(status.path.as_deref(), temp.path().join("canonical").to_str());
        let response = axum::Json(status.clone()).into_response();
        let json: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(json["episodes"], 3);
        assert_eq!(read_library_status(&config, reqwest::Client::new()).await.unwrap(), status);
        assert!(!json.to_string().contains("files_scanned"));
        assert_eq!(
            read_library_status(&Config::default(), reqwest::Client::new()).await.unwrap(),
            LibraryStatus::default()
        );
    }
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
