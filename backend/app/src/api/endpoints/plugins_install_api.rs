//! `POST /api/v1/plugins`, `PUT …/approve`, `…/enable`, `…/disable` and
//! `DELETE /api/v1/plugins/{id}`, and `POST …/rollback`.
//!
//! All of them need `SystemWrite` (the layer is `plugins_api`'s). Each one takes
//! the plugin system's install mutex before it touches anything, then the config
//! file lock where it changes `config.yml`, always in that order; the settings
//! save takes the same two. Plugins off (`plugins.enabled: false`, or a build
//! without the `plugins` feature) answers `404`.

#[cfg(feature = "plugins")]
use super::plugins_settings_api::{config_file_path, persist_entry, written_plugins_config, EntryChange};
#[cfg(feature = "plugins")]
use crate::api::model::reload_plugins;
use crate::api::model::AppState;
use axum::{
    extract::{DefaultBodyLimit, Path},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Router,
};
#[cfg(feature = "plugins")]
use axum::{
    extract::{Multipart, Query, State},
    Json,
};
#[cfg(feature = "plugins")]
use shared::model::{PluginApproveDto, PluginUninstallQueryDto, PluginUploadErrorDto};
use std::sync::Arc;

/// The request body cap of an upload (32 MiB, this route only).
const UPLOAD_LIMIT_BYTES: usize = 32 * 1024 * 1024;

/// The management routes that change things; `plugins_api` merges them under
/// its `SystemWrite` layer.
pub(super) fn install_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins", post(post_plugin_upload).layer(DefaultBodyLimit::max(UPLOAD_LIMIT_BYTES)))
        .route("/plugins/{id}/approve", put(put_plugin_approve))
        .route("/plugins/{id}/staged", get(get_plugin_staged))
        .route("/plugins/{id}/rollback", post(post_plugin_rollback))
        .route("/plugins/{id}/enable", put(put_plugin_enable))
        .route("/plugins/{id}/disable", put(put_plugin_disable))
        .route("/plugins/{id}", delete(delete_plugin))
}

#[cfg(feature = "plugins")]
fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(PluginUploadErrorDto { error: error.into(), problems: Vec::new() })).into_response()
}

/// The bytes of the multipart field `file`, or the status and message of why
/// they could not be read.
#[cfg(feature = "plugins")]
async fn read_package(mut multipart: Multipart) -> Result<Vec<u8>, (StatusCode, String)> {
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) if field.name() == Some("file") => {
                return field
                    .bytes()
                    .await
                    .map(|bytes| bytes.to_vec())
                    .map_err(|err| (err.status(), format!("cannot read the uploaded file: {}", err.body_text())));
            }
            Ok(Some(_)) => {}
            Ok(None) => return Err((StatusCode::BAD_REQUEST, "the multipart field `file` is missing".to_string())),
            Err(err) => return Err((err.status(), format!("cannot read the upload: {}", err.body_text()))),
        }
    }
}

/// `POST /api/v1/plugins`: multipart field `file` holding a `.tlpx`. Answers
/// `200 PluginUploadReviewDto` or `422 PluginUploadErrorDto`; nothing is
/// installed before `PUT …/approve`.
#[cfg(feature = "plugins")]
async fn post_plugin_upload(State(app_state): State<Arc<AppState>>, multipart: Multipart) -> Response {
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Read the whole body before taking the mutex: a slow upload must not block
    // the other management routes.
    let package = match read_package(multipart).await {
        Ok(package) => package,
        Err((status, message)) => return error_response(status, message),
    };
    let guard = plugins.install_lock().await;
    match plugins.api_upload(&guard, package).await {
        Ok(review) => Json(review).into_response(),
        Err(rejection) => (rejection.status, Json(rejection.body)).into_response(),
    }
}

/// `PUT /api/v1/plugins/{id}/approve`: `409` unless the hash and the capability
/// names equal those of the package the admin reviewed. Does not enable.
#[cfg(feature = "plugins")]
async fn put_plugin_approve(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginApproveDto>,
) -> Response {
    // Detached, locks included: a client that hangs up must not cancel an
    // update between its directory swap and its activation check, nor release
    // the install lock while the swap still runs.
    detached(async move {
        let Some(plugins) = app_state.plugins.as_ref() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let guard = plugins.install_lock().await;
        let config = match install_config(&app_state, &id).await {
            Ok(config) => config,
            Err(response) => return *response,
        };
        match plugins.api_approve(&guard, &id, &body, &config).await {
            Ok(summary) => Json(summary).into_response(),
            Err(rejection) => (rejection.status, Json(rejection.body)).into_response(),
        }
    })
    .await
}

/// The plugins config an update or rollback of `id` runs with: the runtime one,
/// but `id`'s settings (and their version) as `config.yml` has them. The runtime
/// config is env-resolved, and an update keeps this entry in the retained
/// prior's `prev.json`, which a rollback writes back: with the runtime entry a
/// resolved secret would reach `prev.json` and then `config.yml`.
#[cfg(feature = "plugins")]
pub(super) async fn install_config(
    app_state: &AppState,
    id: &str,
) -> Result<tuliprox_core::model::PluginsConfig, Box<Response>> {
    let Some(runtime) = app_state.app_config.plugins.load_full() else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    let written = written_plugins_config(&app_state.app_config).await.map_err(|err| {
        log::error!("Cannot read config.yml for plugin `{id}`: {err}");
        Box::new(error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot read config.yml"))
    })?;
    let mut config = (*runtime).clone();
    if let (Some(entry), Some(as_written)) = (config.plugins.iter_mut().find(|entry| entry.id == id), written.entry(id))
    {
        entry.settings.clone_from(&as_written.settings);
        entry.settings_version = as_written.settings_version;
    }
    Ok(config)
}

/// Run a mutating handler body on its own task, so dropping the request does
/// not cancel it halfway; the locks it takes live and die with the task.
#[cfg(feature = "plugins")]
pub(super) async fn detached(work: impl std::future::Future<Output = Response> + Send + 'static) -> Response {
    tokio::spawn(work).await.unwrap_or_else(|err| {
        log::error!("A plugin management task failed: {err}");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// `GET /api/v1/plugins/{id}/staged`: the capability review of the package
/// staged for `id` (an update waiting for approval). `404` when none is staged.
#[cfg(feature = "plugins")]
async fn get_plugin_staged(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let guard = plugins.install_lock().await;
    match plugins.api_staged_review(&guard, &id).await {
        Ok(review) => Json(review).into_response(),
        Err(rejection) => (rejection.status, Json(rejection.body)).into_response(),
    }
}

/// `POST /api/v1/plugins/{id}/rollback`: swap the installed package and the one
/// retained prior version (`SystemWrite`; never an MCP tool). The settings of the
/// prior's config entry are written to `config.yml` once the swap passed its
/// activation check. `404` without a prior, `409` when the check fails (the swap
/// is undone first).
#[cfg(feature = "plugins")]
async fn post_plugin_rollback(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    // Detached like `put_plugin_approve`.
    detached(async move {
        let Some(plugins) = app_state.plugins.as_ref() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let guard = plugins.install_lock().await;
        let file_path = config_file_path(&app_state);
        let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
        let config = match install_config(&app_state, &id).await {
            Ok(config) => config,
            Err(response) => return *response,
        };
        // Journaled until the settings write below is settled.
        let outcome = match plugins.api_rollback_journaled(&guard, &id, &config).await {
            Ok(outcome) => outcome,
            Err(rejection) => return (rejection.status, Json(rejection.body)).into_response(),
        };
        if let Some(settings) = &outcome.settings {
            if let Err(err) =
                persist_entry(&app_state, &id, EntryChange::Versioned(settings, outcome.settings_version)).await
            {
                log::error!("Failed to restore the settings of plugin `{id}` after its rollback: {err}");
                // `config.yml` still holds the settings of the version rolled
                // back from, so undo the rollback: package and settings then
                // agree once more.
                let undone = plugins.api_rollback_settled(&guard, &id, false, &config).await;
                reload_plugins(&app_state.plugins, &app_state.app_config).await;
                let message = match undone {
                    Ok(back) => format!(
                        "the settings of {} could not be saved ({err}); the rollback was undone, still on {back}",
                        outcome.version
                    ),
                    Err(rejection) => {
                        log::error!("Failed to undo the rollback of plugin `{id}`: {}", rejection.body.error);
                        format!(
                            "rolled back to {}, but its settings could not be saved ({err}) and undoing the rollback \
                             failed: {}",
                            outcome.version, rejection.body.error
                        )
                    }
                };
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, message);
            }
            if let Err(rejection) = plugins.api_rollback_settled(&guard, &id, true, &config).await {
                log::error!("Failed to settle the rollback of plugin `{id}`: {}", rejection.body.error);
            }
        }
        reload_plugins(&app_state.plugins, &app_state.app_config).await;
        log::info!("Plugin `{id}` rolled back to {}", outcome.version);
        let config = app_state.app_config.plugins.load();
        plugins
            .api_summary(&id, config.as_deref())
            .map_or_else(|| StatusCode::NOT_FOUND.into_response(), |summary| Json(summary).into_response())
    })
    .await
}

/// Set `enabled` on plugin `id`'s config entry and reload. Enabling a plugin
/// that is pending approval or invalid is a `409`; enabling a failed one re-arms
/// it (the reload gives it a fresh circuit breaker).
#[cfg(feature = "plugins")]
async fn set_enabled(app_state: Arc<AppState>, id: String, enabled: bool) -> Response {
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let guard = plugins.install_lock().await;
    if enabled {
        if let Err(rejection) = plugins.api_enable_check(&guard, &id) {
            return (rejection.status, Json(rejection.body)).into_response();
        }
    } else if plugins.api_summary(&id, None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file_path = config_file_path(&app_state);
    let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
    if let Err(err) = persist_entry(&app_state, &id, EntryChange::Enabled(enabled)).await {
        log::error!("Failed to {} plugin `{id}`: {err}", if enabled { "enable" } else { "disable" });
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
    }
    reload_plugins(&app_state.plugins, &app_state.app_config).await;
    log::info!("Plugin `{id}` {}", if enabled { "enabled" } else { "disabled" });
    let config = app_state.app_config.plugins.load();
    plugins
        .api_summary(&id, config.as_deref())
        .map_or_else(|| StatusCode::NOT_FOUND.into_response(), |summary| Json(summary).into_response())
}

#[cfg(feature = "plugins")]
async fn put_plugin_enable(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    set_enabled(app_state, id, true).await
}

#[cfg(feature = "plugins")]
async fn put_plugin_disable(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    set_enabled(app_state, id, false).await
}

/// `DELETE /api/v1/plugins/{id}?purge_kv=`: drop the config entry, the pin, the
/// plugin directory and any staged copy, reload, and with `purge_kv=true` delete
/// the KV store. Answers `204`.
#[cfg(feature = "plugins")]
async fn delete_plugin(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<PluginUninstallQueryDto>,
) -> Response {
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let guard = plugins.install_lock().await;
    if !plugins.api_exists(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file_path = config_file_path(&app_state);
    let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
    let config = match persist_entry(&app_state, &id, EntryChange::Remove).await {
        Ok(config) => config,
        Err(err) => {
            log::error!("Failed to remove plugin `{id}` from the config: {err}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
        }
    };
    match plugins.api_uninstall(&guard, &id, query.purge_kv, &config).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(rejection) => (rejection.status, Json(rejection.body)).into_response(),
    }
}

#[cfg(not(feature = "plugins"))]
async fn post_plugin_upload() -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn put_plugin_approve(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn get_plugin_staged(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn post_plugin_rollback(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn put_plugin_enable(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn put_plugin_disable(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn delete_plugin(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request};
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn router(dir: &TempDir) -> Router {
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        install_routes().with_state(create_test_app_state(config))
    }

    async fn status(router: &Router, method: &str, uri: &str, body: Body, content_type: &str) -> StatusCode {
        let request =
            Request::builder().method(method).uri(uri).header("content-type", content_type).body(body).unwrap();
        router.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn plugins_off_answers_every_management_route_with_404() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir);
        let multipart = "multipart/form-data; boundary=x";
        let upload =
            "--x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.tlpx\"\r\n\r\nabc\r\n--x--\r\n";
        assert_eq!(status(&router, "POST", "/plugins", Body::from(upload), multipart).await, StatusCode::NOT_FOUND);
        let approve = r#"{"capabilities":[]}"#;
        assert_eq!(
            status(&router, "PUT", "/plugins/demo/approve", Body::from(approve), "application/json").await,
            StatusCode::NOT_FOUND
        );
        for (method, uri) in [
            ("GET", "/plugins/demo/staged"),
            ("POST", "/plugins/demo/rollback"),
            ("PUT", "/plugins/demo/enable"),
            ("PUT", "/plugins/demo/disable"),
            ("DELETE", "/plugins/demo"),
            ("DELETE", "/plugins/demo?purge_kv=true"),
        ] {
            assert_eq!(status(&router, method, uri, Body::empty(), "text/plain").await, StatusCode::NOT_FOUND, "{uri}");
        }
    }
}
