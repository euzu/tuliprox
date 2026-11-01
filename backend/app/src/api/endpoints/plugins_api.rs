//! Plugin management API (plan §10): the list, stats, settings, tasks and logs
//! of the installed plugins under `SystemRead`, and under `SystemWrite` the
//! settings `PUT` (`plugins_settings_api`) and upload, approve, enable, disable
//! and uninstall (`plugins_install_api`).
//!
//! Plugins off (`plugins.enabled: false`, or a build without the `plugins`
//! feature) answers the list with `[]` and every id route with `404`.

use super::{
    plugins_install_api::install_routes, plugins_metrics_api::get_plugins_metrics,
    plugins_settings_api::put_plugin_settings, plugins_tasks_api::post_plugin_task_run,
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
}

fn write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/{id}/settings", put(put_plugin_settings))
        .route("/plugins/{id}/tasks/{task}/run", post(post_plugin_task_run))
        .merge(install_routes())
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
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // As written, so a `${env:VAR}` value shows as the reference (plan §9 *Returning*).
    match super::plugins_settings_api::written_plugins_config(&app_state.app_config).await {
        Ok(config) => found(plugins.api_settings(&id, Some(&config))),
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
