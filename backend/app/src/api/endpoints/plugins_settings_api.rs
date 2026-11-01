//! `PUT /api/v1/plugins/{id}/settings` (plan §9 *Plugin secrets*, §10), under
//! `SystemWrite`.
//!
//! The save is validated and `init`-checked before anything is written
//! (`PluginSystem::api_prepare_settings`); only then does it go to `config.yml`
//! through the same writer as `POST /config/main` (lock, backup, atomic
//! replace), and the plugins reload so the registry picks up the settings.
//! The writer (`persist_entry`) also serves enable, disable and uninstall
//! (`plugins_install_api`).
//! `migrate_stored_settings` is the same write for a settings migration after a
//! plugin update (plan §18 *W8 seam freeze*): it takes the same two locks, and
//! writes the migrated settings together with their `settings_version`.
//! Secret values are never logged and never echoed: the answer is the masked
//! `GET` view plus the warnings of the save.
//!
//! Plugins off (`plugins.enabled: false`, or a build without the `plugins`
//! feature) answers `404`, like every other id route.

#[cfg(feature = "plugins")]
use crate::api::model::{reload_plugins, AppPlugins, AppState};
#[cfg(feature = "plugins")]
use crate::model::AppConfig;
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
    model::{
        PluginEntryDto, PluginSettingsErrorDto, PluginSettingsSaveDto, PluginsConfigDto,
        DEFAULT_PLUGIN_SETTINGS_VERSION,
    },
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
    /// Replace the entry's settings and the `settings_version` they were
    /// written for (a save, or a migration to the schema's version). A version
    /// of 1 is the default and is not written to the file. Creates the entry,
    /// disabled, when the operator listed none: under the trust on first use
    /// rule an `enabled: true` entry would approve the plugin.
    Versioned(&'a Map<String, Value>, u32),
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
        EntryChange::Versioned(settings, version) => {
            if let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == id) {
                entry.settings.clone_from(settings);
                entry.settings_version = *version;
            } else {
                let mut entry = PluginEntryDto::new(id);
                entry.enabled = false;
                entry.settings.clone_from(settings);
                entry.settings_version = *version;
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
        EntryChange::Versioned(settings, version) => {
            if let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == id) {
                entry.settings.clone_from(settings);
                entry.settings_version = *version;
            } else {
                let mut entry = PluginEntry::from(&PluginEntryDto::new(id));
                entry.enabled = false;
                entry.settings.clone_from(settings);
                entry.settings_version = *version;
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

/// The `plugins:` section as `config.yml` has it, `${env:VAR}` references
/// unresolved. The runtime config is read with env resolution, so its settings
/// hold resolved values: what the UI is shown, what an echoed secret sentinel
/// restores and what a migration writes back must start from this instead, or
/// a resolved secret reaches the UI or the file (W8 fix of the W4 settings save).
#[cfg(feature = "plugins")]
pub(in crate::api) async fn written_plugins_config(app_config: &AppConfig) -> Result<PluginsConfig, TuliproxError> {
    let read_path = app_config.paths.load().config_file_path.clone();
    let dto = tokio::task::spawn_blocking(move || crate::config_loader::read_config_file(&read_path, false, false))
        .await
        .map_err(|err| TuliproxError::Config(format!("Could not read config.yml: {err}")))??;
    Ok(dto.plugins.as_ref().map(PluginsConfig::from).unwrap_or_default())
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
    persist_entry_in(&app_state.app_config, id, change).await
}

/// [`persist_entry`] over the config alone, for the callers that have no
/// request state (the settings migration after a reload).
#[cfg(feature = "plugins")]
async fn persist_entry_in(
    app_config: &AppConfig,
    id: &str,
    change: EntryChange<'_>,
) -> Result<PluginsConfig, TuliproxError> {
    let (file_path, backup_dir) = {
        let paths = app_config.paths.load();
        let config = app_config.config.load();
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

    let mut next = app_config.plugins.load_full().map(|current| (*current).clone()).unwrap_or_default();
    apply_to_runtime(&mut next, id, &change);
    app_config.plugins.store(Some(Arc::new(next.clone())));
    app_config.config.rcu(|current| {
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

    // The stored values an echoed sentinel restores, as written (`${env:VAR}` kept).
    let written = match written_plugins_config(&app_state.app_config).await {
        Ok(written) => written,
        Err(err) => {
            log::error!("Cannot read config.yml to save the settings of plugin `{id}`: {err}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot read config.yml");
        }
    };
    let (settings, warnings) = match plugins.api_prepare_settings(&id, &body.settings, Some(&written)).await {
        Ok(prepared) => prepared,
        Err((StatusCode::NOT_FOUND, _)) => return StatusCode::NOT_FOUND.into_response(),
        Err((status, error)) => return (status, Json(error)).into_response(),
    };

    // A save is written for the schema version the package ships, which also
    // clears a `settings_review` (the stored version now matches).
    let version = plugins.settings_schema_version(&id).unwrap_or(DEFAULT_PLUGIN_SETTINGS_VERSION);
    let saved = match persist_entry(&app_state, &id, EntryChange::Versioned(&settings, version)).await {
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

/// The settings migration step after a registry reload (plan §18 *W8 seam
/// freeze*): migrates the stored settings of every plugin whose package moved
/// to a newer settings schema, writes them with their `settings_version`
/// through [`persist_entry_in`] and reloads once. Cheap when nothing is due, and
/// never blocks the reload that triggered it: it runs on its own task, under
/// the install lock and the `config.yml` write lock like a settings save.
#[cfg(feature = "plugins")]
pub(crate) async fn migrate_stored_settings(plugins: &AppPlugins, app_config: &AppConfig) {
    let Some(system) = plugins.as_ref() else {
        return;
    };
    let Some(current) = app_config.plugins.load_full() else {
        return;
    };
    if !system.settings_migration_pending(&current) {
        return;
    }
    let _install = system.install_lock().await;
    let file_path = app_config.paths.load().config_file_path.clone();
    let _lock = app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
    // The config may have changed while this waited for the locks.
    let Some(current) = app_config.plugins.load_full() else {
        return;
    };
    // The runtime config is env-resolved; the migration needs the settings as
    // written so it can put their `${env:VAR}` references back.
    let written: std::collections::HashMap<String, Map<String, Value>> = match written_plugins_config(app_config).await
    {
        Ok(written) => written.plugins.into_iter().map(|entry| (entry.id, entry.settings)).collect(),
        Err(err) => {
            log::error!("Cannot read config.yml for the plugin settings migration, not migrating: {err}");
            return;
        }
    };
    let migrated = system.migrate_settings_as_written(&current, &written).await;
    if migrated.is_empty() {
        return;
    }
    for item in &migrated {
        if let Err(err) =
            persist_entry_in(app_config, &item.id, EntryChange::Versioned(&item.settings, item.version)).await
        {
            log::error!("Failed to save the migrated settings of plugin `{}`: {err}", item.id);
        }
    }
    reload_plugins(plugins, app_config).await;
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

    #[cfg(feature = "plugins")]
    fn settings_of(value: &serde_json::Value) -> Map<String, Value> { value.as_object().cloned().unwrap() }

    #[cfg(feature = "plugins")]
    #[test]
    fn a_versioned_change_writes_the_settings_and_their_version() {
        let settings = settings_of(&serde_json::json!({"endpoint": "a"}));
        let mut dto =
            PluginsConfigDto { plugins: vec![PluginEntryDto::new("demo-plugin")], ..PluginsConfigDto::default() };
        apply_to_dto(&mut dto, "demo-plugin", &EntryChange::Versioned(&settings, 2));
        assert_eq!((dto.plugins[0].settings.clone(), dto.plugins[0].settings_version), (settings.clone(), 2));
        assert!(dto.plugins[0].enabled, "an existing entry keeps its switch");

        let mut runtime = PluginsConfig::from(&dto);
        let newer = settings_of(&serde_json::json!({"endpoint": "b"}));
        apply_to_runtime(&mut runtime, "demo-plugin", &EntryChange::Versioned(&newer, 3));
        assert_eq!(runtime.plugins[0].settings, newer);
        assert_eq!(runtime.plugins[0].settings_version, 3);

        // saved at version 1 again: the default is not written to the file
        apply_to_dto(&mut dto, "demo-plugin", &EntryChange::Versioned(&newer, 1));
        assert!(serde_json::to_value(&dto).unwrap()["plugins"][0].get("settings_version").is_none());
    }

    #[cfg(feature = "plugins")]
    #[test]
    fn a_versioned_change_for_an_unlisted_plugin_creates_a_disabled_entry() {
        let settings = settings_of(&serde_json::json!({"endpoint": "a"}));
        let mut dto = PluginsConfigDto::default();
        apply_to_dto(&mut dto, "demo-plugin", &EntryChange::Versioned(&settings, 2));
        assert_eq!((dto.plugins[0].enabled, dto.plugins[0].settings_version), (false, 2));
        let mut runtime = PluginsConfig::default();
        apply_to_runtime(&mut runtime, "demo-plugin", &EntryChange::Versioned(&settings, 2));
        assert_eq!((runtime.plugins[0].enabled, runtime.plugins[0].settings_version), (false, 2));
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
