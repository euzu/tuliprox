//! The plugin update routes: the check (`POST /plugins/updates/check`), the candidate list, the two
//! global update fields (`/plugins/updates/settings`), the last boot report,
//! the update of one plugin (`POST /plugins/{id}/update`) and its policy
//! (`PUT /plugins/{id}/update-policy`).
//!
//! The reads need `SystemRead`, the mutations `SystemWrite` (the layers are
//! `plugins_api`'s); none of them is an MCP tool. Every mutation takes the
//! plugin install lock, then (when it writes) the `config.yml` write lock, like
//! the other management routes, and runs `detached`. The check syncs only the
//! sources of installed marketplace plugins. The candidates and the settings
//! are computed from `config.yml` as written (`${env:VAR}` unresolved), and the
//! writes change only the update fields, so no resolved value reaches the file.
//! Plugins off answers `404`.

#[cfg(feature = "plugins")]
use super::{
    plugins_install_api::{detached, install_config},
    plugins_marketplace_api::sync_one,
    plugins_settings_api::{config_file_path, written_plugins_config},
};
use crate::api::model::AppState;
#[cfg(feature = "plugins")]
use crate::model::AppConfig;
#[cfg(feature = "plugins")]
use axum::extract::State;
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use shared::model::{PluginUpdatePolicyDto, PluginUpdateRequestDto, PluginUpdateSettingsDto};
#[cfg(feature = "plugins")]
use shared::{
    error::TuliproxError,
    model::{
        is_valid_plugin_id, EventMessage, PluginBootReportDto, PluginUpdateBlockerDto, PluginUpdateEvent,
        PluginUpdateOutcome, PluginUpdateResultDto, PluginUpdateResultKindDto, PluginUpdateSourceErrorDto,
        PluginUploadErrorDto, PluginsConfigDto, Prepare,
    },
};
use std::sync::Arc;
#[cfg(feature = "plugins")]
use tuliprox_core::model::PluginsConfig;
#[cfg(feature = "plugins")]
use tuliprox_plugins::{FetchPolicy, UpdateCandidate};

/// The routes that only read; `plugins_api` merges them under `SystemRead`.
pub(super) fn updates_read_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/updates", get(get_updates))
        .route("/plugins/updates/settings", get(get_update_settings))
        .route("/plugins/updates/boot-report", get(get_boot_report))
}

/// The routes that change things; `plugins_api` merges them under `SystemWrite`.
pub(super) fn updates_write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/updates/check", post(post_updates_check))
        .route("/plugins/updates/settings", put(put_update_settings))
        .route("/plugins/{id}/update", post(post_plugin_update))
        .route("/plugins/{id}/update-policy", put(put_plugin_update_policy))
}

#[cfg(feature = "plugins")]
fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(PluginUploadErrorDto { error: error.into(), problems: Vec::new() })).into_response()
}

/// `404` unless the plugin system runs and the `plugins:` section is loaded.
#[cfg(feature = "plugins")]
fn plugins_on(app_state: &AppState) -> Result<&tuliprox_plugins::PluginSystem, Box<Response>> {
    match app_state.plugins.as_ref() {
        Some(plugins) if app_state.app_config.plugins.load().is_some() => Ok(plugins),
        _ => Err(Box::new(StatusCode::NOT_FOUND.into_response())),
    }
}

/// The `plugins:` section as `config.yml` has it, `500` when it cannot be read.
#[cfg(feature = "plugins")]
async fn written_config(app_state: &AppState) -> Result<PluginsConfig, Box<Response>> {
    written_plugins_config(&app_state.app_config).await.map_err(|err| {
        log::error!("Cannot read config.yml for the plugin updates: {err}");
        Box::new(error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot read config.yml"))
    })
}

/// `POST /api/v1/plugins/updates/check`: sync the sources of the installed
/// marketplace plugins (and only those), then answer the candidates. A source
/// that fails to sync is reported in `errors`; the others still count.
#[cfg(feature = "plugins")]
async fn post_updates_check(State(app_state): State<Arc<AppState>>) -> Response {
    let plugins = match plugins_on(&app_state) {
        Ok(plugins) => plugins,
        Err(response) => return *response,
    };
    let config = match written_config(&app_state).await {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let data_dir = plugins.marketplace_data_dir();
    let mut errors = Vec::new();
    for id in plugins.update_source_ids(&config) {
        let Some(source) = config.repositories.iter().find(|source| source.id == id) else {
            continue;
        };
        if let Err(err) = sync_one(&app_state, &data_dir, source).await {
            log::warn!("Update check: sync of marketplace source `{id}` failed: {err}");
            errors.push(PluginUpdateSourceErrorDto { source: id, error: err.to_string() });
        }
    }
    Json(plugins.updates_dto(&config, errors).await).into_response()
}

/// `GET /api/v1/plugins/updates`: the candidates from the cached catalogs; no
/// network.
#[cfg(feature = "plugins")]
async fn get_updates(State(app_state): State<Arc<AppState>>) -> Response {
    let plugins = match plugins_on(&app_state) {
        Ok(plugins) => plugins,
        Err(response) => return *response,
    };
    let config = match written_config(&app_state).await {
        Ok(config) => config,
        Err(response) => return *response,
    };
    Json(plugins.updates_dto(&config, Vec::new()).await).into_response()
}

/// `config.yml` as the file has it (`${env:VAR}` unresolved, nothing prepared).
#[cfg(feature = "plugins")]
async fn read_written_dto(app_config: &AppConfig) -> Result<shared::model::ConfigDto, TuliproxError> {
    let read_path = app_config.paths.load().config_file_path.clone();
    tokio::task::spawn_blocking(move || crate::config_loader::read_config_file(&read_path, false, false))
        .await
        .map_err(|err| TuliproxError::Config(format!("Could not read config.yml: {err}")))?
}

#[cfg(feature = "plugins")]
fn settings_of(plugins: &PluginsConfigDto) -> PluginUpdateSettingsDto {
    PluginUpdateSettingsDto {
        auto_update_on_boot: plugins.auto_update_on_boot,
        boot_update_budget: plugins.boot_update_budget.clone(),
    }
}

/// `GET /api/v1/plugins/updates/settings`: the two global update fields as
/// `config.yml` has them (the defaults when the file does not list them).
#[cfg(feature = "plugins")]
async fn get_update_settings(State(app_state): State<Arc<AppState>>) -> Response {
    if let Err(response) = plugins_on(&app_state) {
        return *response;
    }
    match read_written_dto(&app_state.app_config).await {
        Ok(dto) => Json(settings_of(&dto.plugins.unwrap_or_default())).into_response(),
        Err(err) => {
            log::error!("Cannot read config.yml for the update settings: {err}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot read config.yml")
        }
    }
}

/// The budget through the shared `plugins:` check (a duration with a unit, not
/// zero), returned normalised, or the problem text.
#[cfg(feature = "plugins")]
fn checked_settings(body: &PluginUpdateSettingsDto) -> Result<PluginsConfigDto, String> {
    let mut dto = PluginsConfigDto {
        auto_update_on_boot: body.auto_update_on_boot,
        boot_update_budget: body.boot_update_budget.clone(),
        ..PluginsConfigDto::default()
    };
    Prepare::prepare(&mut dto, ()).map_err(|err| err.to_string())?;
    Ok(dto)
}

/// Why an update field was not stored.
#[cfg(feature = "plugins")]
#[derive(Debug)]
enum UpdateStoreError {
    /// The plugin has no `plugins.plugins[]` entry.
    NotFound(String),
    /// The file could not be read or written.
    Store(TuliproxError),
}

#[cfg(feature = "plugins")]
impl From<TuliproxError> for UpdateStoreError {
    fn from(err: TuliproxError) -> Self { Self::Store(err) }
}

/// One edit of the update fields of the `plugins:` block.
#[cfg(feature = "plugins")]
enum UpdateChange {
    /// `auto_update_on_boot` and the (already checked) `boot_update_budget`.
    Globals(PluginUpdateSettingsDto),
    /// One entry's policy.
    Policy(String, shared::model::PluginUpdatePolicy),
}

/// Write `change` to `config.yml` as written (backup kept) and apply it to the
/// runtime config, touching nothing else; the same pattern as
/// `plugins_settings_api::persist_entry_in`. The caller holds the install lock
/// and the write lock of the file. Nothing is written for a refused change.
#[cfg(feature = "plugins")]
async fn persist_update_fields(
    app_config: &AppConfig,
    change: UpdateChange,
) -> Result<PluginsConfigDto, UpdateStoreError> {
    let (file_path, backup_dir) = {
        let paths = app_config.paths.load();
        let config = app_config.config.load();
        (paths.config_file_path.clone(), config.get_backup_dir().to_string())
    };
    let mut dto = read_written_dto(app_config).await?;
    let plugins = dto.plugins.get_or_insert_with(PluginsConfigDto::default);
    match &change {
        UpdateChange::Globals(settings) => {
            plugins.auto_update_on_boot = settings.auto_update_on_boot;
            plugins.boot_update_budget.clone_from(&settings.boot_update_budget);
        }
        UpdateChange::Policy(id, policy) => {
            let Some(entry) = plugins.plugins.iter_mut().find(|entry| entry.id == *id) else {
                return Err(UpdateStoreError::NotFound(id.clone()));
            };
            entry.update_policy = *policy;
        }
    }
    let written = plugins.clone();
    crate::config_loader::save_main_config(&file_path, &backup_dir, &dto).await?;

    let mut next = app_config.plugins.load_full().map(|current| (*current).clone()).unwrap_or_default();
    match &change {
        UpdateChange::Globals(_) => {
            let runtime = PluginsConfig::from(&written);
            next.auto_update_on_boot = runtime.auto_update_on_boot;
            next.boot_update_budget = runtime.boot_update_budget;
        }
        UpdateChange::Policy(id, policy) => {
            if let Some(entry) = next.plugins.iter_mut().find(|entry| entry.id == *id) {
                entry.update_policy = *policy;
            }
        }
    }
    app_config.plugins.store(Some(Arc::new(next.clone())));
    app_config.config.rcu(|current| {
        let mut config = current.as_ref().clone();
        config.plugins = Some(next.clone());
        Arc::new(config)
    });
    Ok(written)
}

fn store_failure(err: UpdateStoreError) -> Response {
    match err {
        UpdateStoreError::NotFound(id) => {
            error_response(StatusCode::NOT_FOUND, format!("plugin `{id}` has no entry in the plugins config"))
        }
        UpdateStoreError::Store(err) => {
            log::error!("Failed to save the plugin update settings: {err}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot save config.yml")
        }
    }
}

/// `PUT /api/v1/plugins/updates/settings`: save the two global fields. `422`
/// when the budget is not a duration with a unit or is zero; only those two
/// fields of `config.yml` change.
#[cfg(feature = "plugins")]
async fn put_update_settings(
    State(app_state): State<Arc<AppState>>,
    Json(body): Json<PluginUpdateSettingsDto>,
) -> Response {
    if let Err(response) = plugins_on(&app_state) {
        return *response;
    }
    let settings = match checked_settings(&body) {
        Ok(dto) => settings_of(&dto),
        Err(problem) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(PluginUploadErrorDto { error: "invalid update settings".to_string(), problems: vec![problem] }),
            )
                .into_response();
        }
    };
    // Detached, locks included: a dropped request must not stop between the
    // file write and the runtime update.
    detached(async move {
        let Some(plugins) = app_state.plugins.as_ref() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let _guard = plugins.install_lock().await;
        let file_path = config_file_path(&app_state);
        let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
        match persist_update_fields(&app_state.app_config, UpdateChange::Globals(settings.clone())).await {
            Ok(_) => {
                log::info!(
                    "Plugin update settings saved: auto_update_on_boot={}, boot_update_budget={}",
                    settings.auto_update_on_boot,
                    settings.boot_update_budget
                );
                Json(settings).into_response()
            }
            Err(err) => store_failure(err),
        }
    })
    .await
}

/// `GET /api/v1/plugins/updates/boot-report`: the last boot update run, or an
/// empty report (no `startedAt`, the configured budget) when none ran or the
/// file is unreadable.
#[cfg(feature = "plugins")]
async fn get_boot_report(State(app_state): State<Arc<AppState>>) -> Response {
    let plugins = match plugins_on(&app_state) {
        Ok(plugins) => plugins,
        Err(response) => return *response,
    };
    let report = tuliprox_plugins::read_boot_report(&plugins.marketplace_data_dir()).await;
    let budget_ms = if report.started_at.is_some() {
        report.budget_ms
    } else {
        app_state.app_config.plugins.load().as_ref().map_or(0, |config| config.boot_update_budget.0)
    };
    Json(PluginBootReportDto { budget_ms, ..report }).into_response()
}

/// `PUT /api/v1/plugins/{id}/update-policy`: set the policy of the plugin's
/// `config.yml` entry (as written; its settings are untouched). `404` when
/// the plugin has no entry.
#[cfg(feature = "plugins")]
async fn put_plugin_update_policy(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginUpdatePolicyDto>,
) -> Response {
    if let Err(response) = plugins_on(&app_state) {
        return *response;
    }
    detached(async move {
        let Some(plugins) = app_state.plugins.as_ref() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let _guard = plugins.install_lock().await;
        let file_path = config_file_path(&app_state);
        let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
        match persist_update_fields(&app_state.app_config, UpdateChange::Policy(id.clone(), body.policy)).await {
            Ok(_) => {
                log::info!("Plugin `{id}` update policy set to {:?}", body.policy);
                Json(body).into_response()
            }
            Err(err) => store_failure(err),
        }
    })
    .await
}

/// Tell the admins (event bus, notifiers) how an update ended.
#[cfg(feature = "plugins")]
fn emit(app_state: &AppState, outcome: PluginUpdateOutcome, candidate: &UpdateCandidate, reason: Option<String>) {
    app_state.event_manager.send_event(EventMessage::PluginUpdate(PluginUpdateEvent {
        outcome,
        plugin: candidate.id.clone(),
        from: candidate.installed_version.clone(),
        to: candidate.version.clone(),
        source: candidate.source.clone(),
        reason,
    }));
}

/// `POST /api/v1/plugins/{id}/update`: stage the requested release of an
/// installed marketplace plugin and, with `apply` and when the staged package
/// is eligible, activate it. Works for every policy. `404` when `{id}` has no
/// such candidate, the fetch error status when the download fails, the
/// rejection of the staging step (`409` for a staged package that is not an
/// update of this plugin, `422` for an invalid one). Otherwise `200` with
/// `applied`, `staged` (review and blockers, approve through `PUT …/approve`)
/// or `failed` (the installed package is unchanged).
#[cfg(feature = "plugins")]
async fn post_plugin_update(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginUpdateRequestDto>,
) -> Response {
    let plugins = match plugins_on(&app_state) {
        Ok(plugins) => plugins,
        Err(response) => return *response,
    };
    if !is_valid_plugin_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let written = match written_config(&app_state).await {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let candidate = plugins
        .update_candidates(&written)
        .await
        .into_iter()
        .find(|candidate| candidate.id == id && candidate.version == body.version);
    let Some(candidate) = candidate else {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("no update of `{id}` to {} is available from its marketplace source", body.version),
        );
    };
    // Download before the install lock, like the install route.
    let policy = FetchPolicy::from_env();
    let client = if policy.allow_http {
        app_state.http_clients.no_redirect.load_full()
    } else {
        app_state.http_clients.resource_public_no_redirect.load_full()
    };
    let package = match plugins.download_update(&client, policy, &candidate).await {
        Ok(package) => package,
        Err(err) => {
            // A failed download is a failed update, as in the boot run.
            emit(&app_state, PluginUpdateOutcome::Failed, &candidate, Some(format!("download failed: {err}")));
            return error_response(err.status(), err.to_string());
        }
    };
    // Detached, locks included: the activation must not be cancelled halfway
    // by a client that hangs up.
    detached(async move {
        let Some(plugins) = app_state.plugins.as_ref() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let guard = plugins.install_lock().await;
        let review = match plugins.stage_update(&guard, &candidate, package).await {
            Ok(review) => review,
            Err(rejection) => {
                // A package that does not stage (malformed, incompatible, not the
                // release asked for) is a failed update; a slot held by another
                // upload (409) is not.
                if rejection.status != StatusCode::CONFLICT {
                    let reason = format!("staging failed: {}", rejection.body.error);
                    emit(&app_state, PluginUpdateOutcome::Failed, &candidate, Some(reason));
                }
                return (rejection.status, Json(rejection.body)).into_response();
            }
        };
        // From the staged package this time: `verified` eligibility.
        let eligibility = plugins.evaluate_update(&written, &candidate).await;
        let blockers: Vec<PluginUpdateBlockerDto> = eligibility.blockers.iter().map(Into::into).collect();
        if !body.apply || !eligibility.is_eligible() {
            return Json(PluginUpdateResultDto {
                result: PluginUpdateResultKindDto::Staged,
                review: Some(review),
                blockers,
                reason: None,
            })
            .into_response();
        }
        let config = match install_config(&app_state, &candidate.id).await {
            Ok(config) => config,
            Err(response) => return *response,
        };
        let file_path = config_file_path(&app_state);
        let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
        match plugins.activate_staged_unattended(&guard, &candidate.id, &candidate.version, &config).await {
            Ok(activated) => {
                log::info!("Plugin `{}` updated from {} to {}", candidate.id, activated.from, activated.to);
                emit(&app_state, PluginUpdateOutcome::Applied, &candidate, None);
                Json(PluginUpdateResultDto {
                    result: PluginUpdateResultKindDto::Applied,
                    review: None,
                    blockers: Vec::new(),
                    reason: None,
                })
                .into_response()
            }
            Err(rejection) => {
                let reason = rejection.body.error;
                log::warn!("Update of plugin `{}` to {} failed: {reason}", candidate.id, candidate.version);
                emit(&app_state, PluginUpdateOutcome::Failed, &candidate, Some(reason.clone()));
                Json(PluginUpdateResultDto {
                    result: PluginUpdateResultKindDto::Failed,
                    review: None,
                    blockers: Vec::new(),
                    reason: Some(reason),
                })
                .into_response()
            }
        }
    })
    .await
}

#[cfg(not(feature = "plugins"))]
async fn post_updates_check() -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn get_updates() -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn get_update_settings() -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn put_update_settings(Json(_body): Json<PluginUpdateSettingsDto>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(not(feature = "plugins"))]
async fn get_boot_report() -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn put_plugin_update_policy(Path(_id): Path<String>, Json(_body): Json<PluginUpdatePolicyDto>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(not(feature = "plugins"))]
async fn post_plugin_update(Path(_id): Path<String>, Json(_body): Json<PluginUpdateRequestDto>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request};
    use tempfile::TempDir;
    use tower::ServiceExt;

    async fn status(router: &Router, method: &str, uri: &str, body: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        router.clone().oneshot(request).await.unwrap().status()
    }

    fn config_in(dir: &TempDir) -> Config {
        Config {
            storage_dir: dir.path().to_string_lossy().to_string(),
            backup_dir: Some(dir.path().join("backup").to_string_lossy().to_string()),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn the_read_and_write_routers_split_by_method() {
        let dir = TempDir::new().unwrap();
        let state = create_test_app_state(config_in(&dir));
        let read: Router = updates_read_routes().with_state(Arc::clone(&state));
        let write: Router = updates_write_routes().with_state(state);
        // The read router answers no mutation, the write router no read
        // (plugins off answers 404 for what a router does serve).
        for (method, uri) in [
            ("POST", "/plugins/updates/check"),
            ("PUT", "/plugins/updates/settings"),
            ("POST", "/plugins/demo/update"),
            ("PUT", "/plugins/demo/update-policy"),
        ] {
            assert!(
                matches!(
                    status(&read, method, uri, "{}").await,
                    StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
                ),
                "the read router must not serve {method} {uri}"
            );
            assert_ne!(status(&write, method, uri, "{}").await, StatusCode::METHOD_NOT_ALLOWED, "{method} {uri}");
        }
        for uri in ["/plugins/updates", "/plugins/updates/settings", "/plugins/updates/boot-report"] {
            assert_ne!(status(&read, "GET", uri, "").await, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
            assert!(
                matches!(status(&write, "GET", uri, "").await, StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND),
                "the write router must not serve GET {uri}"
            );
        }
    }

    #[tokio::test]
    async fn plugins_off_answers_every_update_route_with_404() {
        let dir = TempDir::new().unwrap();
        let router: Router =
            updates_read_routes().merge(updates_write_routes()).with_state(create_test_app_state(config_in(&dir)));
        for uri in ["/plugins/updates", "/plugins/updates/settings", "/plugins/updates/boot-report"] {
            assert_eq!(status(&router, "GET", uri, "").await, StatusCode::NOT_FOUND, "{uri}");
        }
        let settings = r#"{"autoUpdateOnBoot":true,"bootUpdateBudget":"30s"}"#;
        assert_eq!(status(&router, "POST", "/plugins/updates/check", "").await, StatusCode::NOT_FOUND);
        assert_eq!(status(&router, "PUT", "/plugins/updates/settings", settings).await, StatusCode::NOT_FOUND);
        assert_eq!(
            status(&router, "POST", "/plugins/demo/update", r#"{"version":"1.1.0","apply":true}"#).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(&router, "PUT", "/plugins/demo/update-policy", r#"{"policy":"pinned"}"#).await,
            StatusCode::NOT_FOUND
        );
    }

    #[cfg(feature = "plugins")]
    mod with_plugins {
        use super::*;
        use crate::api::model::{create_test_app_state_with_plugins, create_test_plugin_system};
        use shared::model::PluginUpdatePolicy;

        const BASE: &str = "api:\n  host: ${env:TP_TEST_UPD_HOST}\n  port: 8901\n";

        /// An app state with a loaded plugin system, a `config.yml` holding
        /// `yaml` and a runtime `plugins:` section read from it as written.
        fn app_state_for(dir: &TempDir, yaml: &str) -> (Arc<AppState>, std::path::PathBuf) {
            let file = dir.path().join("config.yml");
            std::fs::write(&file, yaml).unwrap();
            let system = create_test_plugin_system(dir.path());
            let app_state = create_test_app_state_with_plugins(config_in(dir), Some(system));
            let mut paths = app_state.app_config.paths.load().as_ref().clone();
            paths.config_file_path = file.to_string_lossy().to_string();
            app_state.app_config.paths.store(Arc::new(paths));
            let written = crate::config_loader::read_config_file(&file.to_string_lossy(), false, false).unwrap();
            let runtime = written.plugins.as_ref().map(PluginsConfig::from).unwrap_or_default();
            app_state.app_config.plugins.store(Some(Arc::new(runtime)));
            (app_state, file)
        }

        fn router(app_state: &Arc<AppState>) -> Router {
            updates_read_routes().merge(updates_write_routes()).with_state(Arc::clone(app_state))
        }

        #[tokio::test]
        async fn a_policy_is_persisted_as_written() {
            let dir = TempDir::new().unwrap();
            let yaml = format!(
                "{BASE}plugins:\n  enabled: true\n  plugins:\n    - id: demo\n      settings:\n        token: ${{env:TP_TEST_UPD_SECRET}}\n"
            );
            let (app_state, file) = app_state_for(&dir, &yaml);
            let router = router(&app_state);
            let code = status(&router, "PUT", "/plugins/demo/update-policy", r#"{"policy":"pinned"}"#).await;
            assert_eq!(code, StatusCode::OK);

            let text = std::fs::read_to_string(&file).unwrap();
            assert!(text.contains("update_policy: pinned"), "{text}");
            assert!(text.contains("${env:TP_TEST_UPD_SECRET}"), "{text}");
            assert!(text.contains("${env:TP_TEST_UPD_HOST}"), "{text}");
            let runtime = app_state.app_config.plugins.load_full().unwrap();
            assert_eq!(runtime.entry("demo").unwrap().update_policy, PluginUpdatePolicy::Pinned);

            // `auto` is the default and is not written back
            let code = status(&router, "PUT", "/plugins/demo/update-policy", r#"{"policy":"auto"}"#).await;
            assert_eq!(code, StatusCode::OK);
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(!text.contains("update_policy"), "{text}");
            assert!(text.contains("${env:TP_TEST_UPD_SECRET}"), "{text}");
        }

        #[tokio::test]
        async fn a_policy_for_an_unlisted_plugin_is_404_and_writes_nothing() {
            let dir = TempDir::new().unwrap();
            let yaml = format!("{BASE}plugins:\n  enabled: true\n");
            let (app_state, file) = app_state_for(&dir, &yaml);
            let before = std::fs::read_to_string(&file).unwrap();
            let code =
                status(&router(&app_state), "PUT", "/plugins/ghost-plugin/update-policy", r#"{"policy":"manual"}"#)
                    .await;
            assert_eq!(code, StatusCode::NOT_FOUND);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        }

        #[tokio::test]
        async fn the_settings_round_trip_as_written_and_change_only_their_two_fields() {
            let dir = TempDir::new().unwrap();
            let yaml = format!(
                "{BASE}plugins:\n  enabled: true\n  plugins:\n    - id: demo\n      settings:\n        token: ${{env:TP_TEST_UPD_SECRET}}\n"
            );
            let (app_state, file) = app_state_for(&dir, &yaml);
            let router = router(&app_state);
            let get = |router: Router| async move {
                let request = Request::builder().uri("/plugins/updates/settings").body(Body::empty()).unwrap();
                let response = router.oneshot(request).await.unwrap();
                let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
                serde_json::from_slice::<PluginUpdateSettingsDto>(&bytes).unwrap()
            };
            let defaults = get(router.clone()).await;
            assert_eq!((defaults.auto_update_on_boot, defaults.boot_update_budget.as_str()), (false, "30s"));

            let body = r#"{"autoUpdateOnBoot":true,"bootUpdateBudget":" 45s "}"#;
            assert_eq!(status(&router, "PUT", "/plugins/updates/settings", body).await, StatusCode::OK);
            let saved = get(router.clone()).await;
            assert_eq!((saved.auto_update_on_boot, saved.boot_update_budget.as_str()), (true, "45s"));
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(text.contains("auto_update_on_boot: true"), "{text}");
            assert!(text.contains("${env:TP_TEST_UPD_SECRET}") && text.contains("${env:TP_TEST_UPD_HOST}"), "{text}");
            let runtime = app_state.app_config.plugins.load_full().unwrap();
            assert!(runtime.auto_update_on_boot);
            assert_eq!(runtime.boot_update_budget.0, 45_000);
        }

        #[tokio::test]
        async fn a_bad_budget_is_422_and_writes_nothing() {
            let dir = TempDir::new().unwrap();
            let yaml = format!("{BASE}plugins:\n  enabled: true\n");
            let (app_state, file) = app_state_for(&dir, &yaml);
            let router = router(&app_state);
            let before = std::fs::read_to_string(&file).unwrap();
            for budget in ["soon", "30", "0s", ""] {
                let body = format!(r#"{{"autoUpdateOnBoot":true,"bootUpdateBudget":"{budget}"}}"#);
                let code = status(&router, "PUT", "/plugins/updates/settings", &body).await;
                // a blank value falls back to the default, like the file does
                let expected = if budget.is_empty() { StatusCode::OK } else { StatusCode::UNPROCESSABLE_ENTITY };
                assert_eq!(code, expected, "budget `{budget}`");
                if expected != StatusCode::OK {
                    assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "budget `{budget}`");
                }
            }
        }

        #[tokio::test]
        async fn an_update_of_an_unknown_plugin_is_404() {
            let dir = TempDir::new().unwrap();
            let yaml = format!("{BASE}plugins:\n  enabled: true\n");
            let (app_state, _) = app_state_for(&dir, &yaml);
            let router = router(&app_state);
            let body = r#"{"version":"9.9.9","apply":true}"#;
            assert_eq!(status(&router, "POST", "/plugins/ghost-plugin/update", body).await, StatusCode::NOT_FOUND);
            // an installed plugin without a marketplace origin has no candidate either
            assert_eq!(status(&router, "POST", "/plugins/demo/update", body).await, StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn the_boot_report_defaults_when_absent_and_is_read_when_present() {
            let dir = TempDir::new().unwrap();
            let yaml = format!("{BASE}plugins:\n  enabled: true\n  boot_update_budget: 20s\n");
            let (app_state, _) = app_state_for(&dir, &yaml);
            let router = router(&app_state);
            let fetch = |router: Router| async move {
                let request = Request::builder().uri("/plugins/updates/boot-report").body(Body::empty()).unwrap();
                let response = router.oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
                serde_json::from_slice::<PluginBootReportDto>(&bytes).unwrap()
            };
            let empty = fetch(router.clone()).await;
            assert_eq!((empty.started_at, empty.budget_ms, empty.entries.len()), (None, 20_000, 0));

            let report = PluginBootReportDto {
                started_at: Some(10),
                finished_at: Some(12),
                budget_ms: 20_000,
                budget_exceeded: true,
                recovered: vec!["demo".to_string()],
                entries: Vec::new(),
            };
            let path = tuliprox_plugins::boot_report_path(&app_state.plugins.as_ref().unwrap().marketplace_data_dir());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_vec(&report).unwrap()).unwrap();
            assert_eq!(fetch(router).await, report);
        }
    }
}
