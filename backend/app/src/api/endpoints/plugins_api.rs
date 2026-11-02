//! Plugin management API: the list, stats, settings, tasks and logs
//! of the installed plugins under `SystemRead`, and under `SystemWrite` the
//! settings `PUT` (`plugins_settings_api`) and upload, approve, enable, disable
//! and uninstall (`plugins_install_api`). The marketplace sources and catalog
//! (`plugins_marketplace_api`) read under `SystemRead` and change under
//! `SystemWrite`.
//!
//! Plugins off (`plugins.enabled: false`, or a build without the `plugins`
//! feature) answers the list with `[]` and every id route with `404`.

#[cfg(feature = "plugins")]
use super::plugins_settings_api::read_plugin_settings;
use super::{
    plugins_install_api::install_routes,
    plugins_marketplace_api::{marketplace_read_routes, marketplace_write_routes},
    plugins_metrics_api::get_plugins_metrics,
    plugins_settings_api::put_plugin_settings,
    plugins_tasks_api::post_plugin_task_run,
    plugins_updates_api::{updates_read_routes, updates_write_routes},
};
use crate::api::{auth_middleware::permission_layer, model::AppState};
#[cfg(feature = "plugins")]
use axum::extract::State;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use shared::model::{Permission, PluginLogQueryDto, PluginSummaryDto};
use std::sync::Arc;

fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins", get(get_plugins))
        .route("/plugins/metrics", get(get_plugins_metrics))
        .route("/plugins/{id}/stats", get(get_plugin_stats))
        .route("/plugins/{id}/settings", get(get_plugin_settings))
        .route("/plugins/{id}/tasks", get(get_plugin_tasks))
        .route("/plugins/{id}/logs", get(get_plugin_logs))
        .merge(marketplace_read_routes())
        .merge(updates_read_routes())
}

fn write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/{id}/settings", put(put_plugin_settings))
        .route("/plugins/{id}/tasks/{task}/run", post(post_plugin_task_run))
        .merge(install_routes())
        .merge(marketplace_write_routes())
        .merge(updates_write_routes())
}

pub fn plugins_api_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .merge(routes().layer(permission_layer!(app_state, Permission::SystemRead)))
        .merge(write_routes().layer(permission_layer!(app_state, Permission::SystemWrite)))
}

pub fn plugins_api_register_unprotected() -> Router<Arc<AppState>> { routes().merge(write_routes()) }

/// `Some(body)` answers `200`, `None` (unknown id, or plugins off) `404`.
fn found<T: serde::Serialize>(body: Option<T>) -> Response {
    body.map_or_else(|| StatusCode::NOT_FOUND.into_response(), |body| Json(body).into_response())
}

#[cfg(feature = "plugins")]
async fn get_plugins(State(app_state): State<Arc<AppState>>) -> Json<Vec<PluginSummaryDto>> {
    let config = app_state.app_config.plugins.load();
    Json(app_state.plugins.as_ref().map(|plugins| plugins.api_list(config.as_deref())).unwrap_or_default())
}

#[cfg(feature = "plugins")]
async fn get_plugin_stats(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    found(app_state.plugins.as_ref().and_then(|plugins| plugins.api_stats(&id)))
}

#[cfg(feature = "plugins")]
async fn get_plugin_settings(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(plugins) = app_state.plugins.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // As written, so a `${env:VAR}` value shows as the reference.
    match super::plugins_settings_api::written_plugins_config(&app_state.app_config).await {
        // The schema files are read on the blocking pool.
        Ok(config) => found(read_plugin_settings(plugins, id, Some(Arc::new(config))).await),
        Err(err) => {
            log::error!("Cannot read config.yml to show the settings of plugin `{id}`: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(feature = "plugins")]
async fn get_plugin_tasks(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    found(app_state.plugins.as_ref().and_then(|plugins| plugins.api_tasks(&id)))
}

#[cfg(feature = "plugins")]
async fn get_plugin_logs(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<PluginLogQueryDto>,
) -> Response {
    found(app_state.plugins.as_ref().and_then(|plugins| plugins.api_logs(&id, &query)))
}

#[cfg(not(feature = "plugins"))]
async fn get_plugins() -> Json<Vec<PluginSummaryDto>> { Json(Vec::new()) }

#[cfg(not(feature = "plugins"))]
async fn get_plugin_stats(Path(_id): Path<String>) -> Response { found::<()>(None) }

#[cfg(not(feature = "plugins"))]
async fn get_plugin_settings(Path(_id): Path<String>) -> Response { found::<()>(None) }

#[cfg(not(feature = "plugins"))]
async fn get_plugin_tasks(Path(_id): Path<String>) -> Response { found::<()>(None) }

#[cfg(not(feature = "plugins"))]
async fn get_plugin_logs(Path(_id): Path<String>, Query(_query): Query<PluginLogQueryDto>) -> Response {
    found::<()>(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    async fn call(router: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let request = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        (status, response.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    fn router(dir: &TempDir) -> Router {
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        plugins_api_register_unprotected().with_state(create_test_app_state(config))
    }

    #[tokio::test]
    async fn plugins_off_lists_nothing() {
        let dir = TempDir::new().unwrap();
        let (status, body) = call(&router(&dir), "/plugins").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_slice::<Vec<PluginSummaryDto>>(&body).unwrap(), Vec::new());
    }

    #[cfg(feature = "plugins")]
    #[test]
    fn settings_read_the_schema_file_on_the_blocking_pool() {
        use crate::api::model::{counting_runtime, create_test_app_state_with_plugins, create_test_plugin_system};
        let dir = TempDir::new().unwrap();
        let (runtime, threads) = counting_runtime();
        runtime.block_on(async {
            let system = create_test_plugin_system(dir.path());
            let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
            let app_state = create_test_app_state_with_plugins(config, Some(system));
            // The settings GET reads `config.yml` as written, so the test needs one.
            let config_file = dir.path().join("config.yml");
            std::fs::write(&config_file, "api:\n  host: 127.0.0.1\n  port: 8901\n").unwrap();
            let mut paths = (**app_state.app_config.paths.load()).clone();
            paths.config_file_path = config_file.to_string_lossy().to_string();
            app_state.app_config.paths.store(std::sync::Arc::new(paths));
            let router = plugins_api_register_unprotected().with_state(app_state);
            let before = threads.load(std::sync::atomic::Ordering::SeqCst);
            let (status, body) = call(&router, "/plugins/demo/settings").await;
            assert_eq!(status, StatusCode::OK);
            let settings: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(settings["schema"], serde_json::json!({"type": "object"}));
            assert!(
                threads.load(std::sync::atomic::Ordering::SeqCst) > before,
                "the schema was read on the async thread"
            );
        });
    }

    #[tokio::test]
    async fn plugins_off_answers_every_id_route_with_404() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir);
        for route in ["stats", "settings", "tasks", "logs", "logs?level=info&limit=5"] {
            let (status, _) = call(&router, &format!("/plugins/demo/{route}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
        }
    }
}
