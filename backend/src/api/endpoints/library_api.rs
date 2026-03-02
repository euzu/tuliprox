use crate::{
    api::{
        library_scan::spawn_library_scan,
        model::{AppState, EventMessage},
    },
    library::LibraryProcessor,
};
use axum::response::IntoResponse;
use log::{debug, warn};
use serde_json::json;
use shared::model::{LibraryScanRequest, LibraryScanSummary, LibraryScanSummaryStatus, LibraryStatus};
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
    let (lib_config, metadata_update_config) = {
        let config = app_state.app_config.config.load();
        match config.library.as_ref() {
            Some(lib) if lib.enabled => (lib.clone(), config.metadata_update.clone()),
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
    spawn_library_scan(event_manager, lib_config, metadata_update_config, client, request.force_rescan, "", permit);

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
            let processor = LibraryProcessor::new(config.clone(), config_snapshot.metadata_update.as_ref(), client);
            let entries = processor.get_all_entries().await;

            let movies = entries.iter().filter(|e| e.metadata.is_movie()).count();
            let series = entries.iter().filter(|e| e.metadata.is_series()).count();

            let response = LibraryStatus {
                enabled: true,
                total_items: entries.len(),
                movies,
                series,
                path: config_snapshot.metadata_update.as_ref().map(|m| m.cache_path.clone()),
            };

            return axum::Json(response).into_response();
        }
    }

    let response = LibraryStatus::default();
    axum::Json(response).into_response()
}

/// Registers Library API routes
pub fn library_api_register(router: axum::Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/library/scan", axum::routing::post(scan_library))
        .route("/library/status", axum::routing::get(get_library_status))
}
