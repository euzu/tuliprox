//! `PUT /api/v1/plugins/{id}/settings` (plan §9 *Plugin secrets*, §10), under
//! `SystemWrite`.
//!
//! The save is validated and `init`-checked before anything is written
//! (`PluginSystem::api_prepare_settings`); only then does it go to `config.yml`
//! through the same writer as `POST /config/main` (lock, backup, atomic
//! replace), and the plugins reload so the registry picks up the settings.
//! The writer (`persist_entry`) also serves enable, disable and uninstall
//! (`plugins_install_api`).
//! Secret values are never logged and never echoed: the answer is the masked
//! `GET` view plus the warnings of the save.
//!
//! Plugins off (`plugins.enabled: false`, or a build without the `plugins`
//! feature) answers `404`, like every other id route.

#[cfg(feature = "plugins")]
use crate::api::model::{reload_plugins, AppState};
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
};
#[cfg(feature = "plugins")]
use axum::{extract::State, Json};
#[cfg(feature = "plugins")]
use serde_json::{Map, Value};
#[cfg(feature = "plugins")]
use shared::{
    error::TuliproxError,
    model::{PluginEntryDto, PluginSettingsErrorDto, PluginSettingsSaveDto, PluginsConfigDto},
};
#[cfg(feature = "plugins")]
use std::sync::Arc;
#[cfg(feature = "plugins")]
use tuliprox_core::model::{PluginEntry, PluginsConfig};

#[cfg(feature = "plugins")]
fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(PluginSettingsErrorDto { error: error.into(), fields: Vec::new() })).into_response()
}

/// A change to one `plugins.plugins[]` entry of `config.yml`.
#[cfg(feature = "plugins")]
pub(super) enum EntryChange<'a> {
    /// Replace the entry's settings. Creates the entry, disabled, when the
    /// operator listed none: under the trust on first use rule an
    /// `enabled: true` entry would approve the plugin.
    Settings(&'a Map<String, Value>),
    /// Switch the entry on or off. Switching on creates the entry; switching
    /// off with no entry changes nothing.
    Enabled(bool),
    /// Drop the entry (uninstall).
    Remove,
}

#[cfg(feature = "plugins")]
fn apply_to_dto(plugins: &mut PluginsConfigDto, id: &str, change: &EntryChange<'_>) {
    match change {
        EntryChange::Remove => plugins.plugins.retain(|entry| entry.id != id),
        EntryChange::Settings(settings) => {
            if let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == id) {
                entry.settings.clone_from(settings);
            } else {
                let mut entry = PluginEntryDto::new(id);
                entry.enabled = false;
                entry.settings.clone_from(settings);
                plugins.plugins.push(entry);
            }
        }
        EntryChange::Enabled(enabled) => {
            if let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == id) {
                entry.enabled = *enabled;
            } else if *enabled {
                plugins.plugins.push(PluginEntryDto::new(id));
            }
        }
    }
}

/// The runtime twin of [`apply_to_dto`].
#[cfg(feature = "plugins")]
fn apply_to_runtime(plugins: &mut PluginsConfig, id: &str, change: &EntryChange<'_>) {
    match change {
        EntryChange::Remove => plugins.plugins.retain(|entry| entry.id != id),
        EntryChange::Settings(settings) => {
            if let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == id) {
                entry.settings.clone_from(settings);
            } else {
                let mut entry = PluginEntry::from(&PluginEntryDto::new(id));
                entry.enabled = false;
                entry.settings.clone_from(settings);
                plugins.plugins.push(entry);
            }
        }
        EntryChange::Enabled(enabled) => {
            if let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == id) {
                entry.enabled = *enabled;
            } else if *enabled {
                plugins.plugins.push(PluginEntry::from(&PluginEntryDto::new(id)));
            }
        }
    }
}

/// The path of `config.yml`, whose write lock serializes config changes.
#[cfg(feature = "plugins")]
pub(super) fn config_file_path(app_state: &AppState) -> String {
    app_state.app_config.paths.load().config_file_path.clone()
}

/// Write `change` to plugin `id`'s entry in `config.yml` (backup kept) and
/// return the runtime `plugins:` section with it applied. The caller holds the
/// write lock of the file.
#[cfg(feature = "plugins")]
pub(super) async fn persist_entry(
    app_state: &AppState,
    id: &str,
    change: EntryChange<'_>,
) -> Result<PluginsConfig, TuliproxError> {
    let (file_path, backup_dir) = {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        (paths.config_file_path.clone(), config.get_backup_dir().to_string())
    };
    // The file as written, with `${env:VAR}` and every other value untouched,
    // so only this plugin's entry changes.
    let read_path = file_path.clone();
    let mut dto = tokio::task::spawn_blocking(move || crate::config_loader::read_config_file(&read_path, false, false))
        .await
        .map_err(|err| TuliproxError::Config(format!("Could not read config.yml: {err}")))??;
    apply_to_dto(dto.plugins.get_or_insert_with(PluginsConfigDto::default), id, &change);
    crate::config_loader::save_main_config(&file_path, &backup_dir, &dto).await?;

    let mut next = app_state.app_config.plugins.load_full().map(|current| (*current).clone()).unwrap_or_default();
    apply_to_runtime(&mut next, id, &change);
    app_state.app_config.plugins.store(Some(Arc::new(next.clone())));
    app_state.app_config.config.rcu(|current| {
        let mut config = current.as_ref().clone();
        config.plugins = Some(next.clone());
        Arc::new(config)
    });
    Ok(next)
}

#[cfg(feature = "plugins")]
pub(super) async fn put_plugin_settings(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginSettingsSaveDto>,
) -> Response {
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // One install-affecting change at a time (plan §18 *W6b seam freeze*), and
    // within it one config write at a time: the stored values the sentinel
    // restores come from the state this save then replaces. The install routes
    // take the locks in the same order.
    let _install = plugins.install_lock().await;
    let file_path = config_file_path(&app_state);
    let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;

    let current = app_state.app_config.plugins.load_full();
    let (settings, warnings) = match plugins.api_prepare_settings(&id, &body.settings, current.as_deref()).await {
        Ok(prepared) => prepared,
        Err((StatusCode::NOT_FOUND, _)) => return StatusCode::NOT_FOUND.into_response(),
        Err((status, error)) => return (status, Json(error)).into_response(),
    };

    let saved = match persist_entry(&app_state, &id, EntryChange::Settings(&settings)).await {
        Ok(saved) => saved,
        Err(err) => {
            log::error!("Failed to save the settings of plugin `{id}`: {err}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
        }
    };
    reload_plugins(&app_state.plugins, &app_state.app_config).await;
    log::info!("Settings of plugin `{id}` saved");

    let Some(mut answer) = plugins.api_settings(&id, Some(&saved)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    answer.warnings = warnings;
    Json(answer).into_response()
}

#[cfg(not(feature = "plugins"))]
pub(super) async fn put_plugin_settings(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request, routing::put, Router};
    use tempfile::TempDir;
    use tower::ServiceExt;

    async fn put_settings(dir: &TempDir, id: &str, body: &str) -> StatusCode {
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        let router: Router = Router::new()
            .route("/plugins/{id}/settings", put(put_plugin_settings))
            .with_state(create_test_app_state(config));
        let request = Request::builder()
            .method("PUT")
            .uri(format!("/plugins/{id}/settings"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        router.oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn plugins_off_answers_404() {
        let dir = TempDir::new().unwrap();
        assert_eq!(put_settings(&dir, "demo", r#"{"settings":{"token":"x"}}"#).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn plugins_off_answers_404_for_a_body_that_is_not_an_object_too() {
        let dir = TempDir::new().unwrap();
        assert_eq!(put_settings(&dir, "demo", r#"{"settings":"nope"}"#).await, StatusCode::NOT_FOUND);
    }
}
