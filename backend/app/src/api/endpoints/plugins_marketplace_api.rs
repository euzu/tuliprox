//! The repository marketplace routes: the source list (`/plugins/repositories…`), the catalog browser
//! (`GET /plugins/marketplace`) and the install of a catalog release
//! (`POST /plugins/marketplace/install`).
//!
//! The reads need `SystemRead`, the mutations `SystemWrite` (the layers are
//! `plugins_api`'s); none of them is an MCP tool. A mutation of the source list
//! takes the plugin install lock, then the `config.yml` write lock, like the
//! other management routes. A sync takes neither: the cache writes are atomic
//! and a per-source mutex only serialises the syncs of one source. Responses
//! show a source url as written (`${env:VAR}` unresolved). Plugins off answers
//! `404`.

#[cfg(feature = "plugins")]
use super::{
    plugins_install_api::detached,
    plugins_repositories_store::{persist_repositories, written_repositories, RepositoryChange, RepositoryStoreError},
    plugins_settings_api::config_file_path,
};
use crate::api::model::AppState;
#[cfg(feature = "plugins")]
use axum::extract::State;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
#[cfg(feature = "plugins")]
use shared::model::{is_valid_plugin_id, PluginOriginDto, PluginUploadErrorDto};
use shared::model::{MarketplaceInstallDto, MarketplaceQueryDto, PluginRepositoryDto};
use std::sync::Arc;
#[cfg(feature = "plugins")]
use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
};
#[cfg(feature = "plugins")]
use tuliprox_core::model::{marketplace_allow_http, validate_repository, PluginRepository};
#[cfg(feature = "plugins")]
use tuliprox_plugins::{
    browse, fetch_package, has_catalog, remove_cache, resolve_release, source_info, sync_source, FetchPolicy,
    MarketplaceExpect, ReleaseLookup, PACKAGE_MAX_BYTES,
};

/// The routes that only read; `plugins_api` merges them under `SystemRead`.
pub(super) fn marketplace_read_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/repositories", get(get_repositories))
        .route("/plugins/marketplace", get(get_marketplace))
}

/// The routes that change things; `plugins_api` merges them under `SystemWrite`.
pub(super) fn marketplace_write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/repositories", post(post_repository))
        .route("/plugins/repositories/{id}", put(put_repository).delete(delete_repository))
        .route("/plugins/repositories/{id}/sync", post(post_repository_sync))
        .route("/plugins/marketplace/install", post(post_marketplace_install))
}

/// One mutex per source id, so two syncs of a source never interleave their
/// fetch and cache write.
#[cfg(feature = "plugins")]
static SYNC_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Sources whose empty cache `GET /plugins/marketplace` already tried to fill in
/// this process: the first browse syncs once, later ones leave it to the admin.
#[cfg(feature = "plugins")]
static FIRST_SYNC_ATTEMPTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[cfg(feature = "plugins")]
fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(PluginUploadErrorDto { error: error.into(), problems: Vec::new() })).into_response()
}

/// The configured sources as `config.yml` has them (`${env:VAR}` unresolved; the
/// runtime config holds resolved urls, which must never be shown). `404` with
/// plugins off, `500` when the file cannot be read.
#[cfg(feature = "plugins")]
async fn written_sources(app_state: &AppState) -> Result<Vec<PluginRepository>, Box<Response>> {
    if app_state.plugins.is_none() || app_state.app_config.plugins.load().is_none() {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    match written_repositories(&app_state.app_config).await {
        Ok(list) => Ok(list.iter().map(PluginRepository::from).collect()),
        Err(err) => {
            log::error!("Cannot read the marketplace sources from config.yml: {err}");
            Err(Box::new(error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot read config.yml")))
        }
    }
}

/// Sync `source` now, one sync of a source at a time, and mark its first sync
/// of this process as done.
#[cfg(feature = "plugins")]
pub(super) async fn sync_one(
    app_state: &AppState,
    data_dir: &std::path::Path,
    source: &PluginRepository,
) -> Result<shared::model::PluginRepositoryInfoDto, tuliprox_plugins::MarketplaceError> {
    let lock = {
        let mut locks = SYNC_LOCKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(source.id.clone()).or_default())
    };
    let _serialised = lock.lock().await;
    FIRST_SYNC_ATTEMPTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(source.id.clone());
    let client = app_state.http_clients.no_redirect.load_full();
    sync_source(data_dir, &client, FetchPolicy::from_env(), source).await
}

/// Forget that `id` was synced in this process (its url changed or it was
/// removed), so a source added again under that id syncs on its first browse.
#[cfg(feature = "plugins")]
fn forget_first_sync(id: &str) {
    FIRST_SYNC_ATTEMPTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
}

/// `GET /api/v1/plugins/repositories`: every configured source with its sync state.
#[cfg(feature = "plugins")]
async fn get_repositories(State(app_state): State<Arc<AppState>>) -> Response {
    let sources = match written_sources(&app_state).await {
        Ok(sources) => sources,
        Err(response) => return *response,
    };
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let data_dir = plugins.marketplace_data_dir();
    let mut rows = Vec::with_capacity(sources.len());
    for source in &sources {
        rows.push(source_info(&data_dir, source).await);
    }
    Json(rows).into_response()
}

/// `GET /api/v1/plugins/marketplace?q=&category=&source=`: the plugins of every
/// enabled source. An enabled source without a catalog is synced once per boot
/// first (a failure only lands in its `lastError`).
#[cfg(feature = "plugins")]
async fn get_marketplace(State(app_state): State<Arc<AppState>>, Query(query): Query<MarketplaceQueryDto>) -> Response {
    let sources = match written_sources(&app_state).await {
        Ok(sources) => sources,
        Err(response) => return *response,
    };
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let data_dir = plugins.marketplace_data_dir();
    for source in sources.iter().filter(|source| source.enabled) {
        if has_catalog(&data_dir, source).await {
            continue;
        }
        let first =
            FIRST_SYNC_ATTEMPTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(source.id.clone());
        if first {
            if let Err(err) = sync_one(&app_state, &data_dir, source).await {
                log::warn!("First sync of marketplace source `{}` failed: {err}", source.id);
            }
        }
    }
    Json(browse(&data_dir, &sources, &plugins.marketplace_installed(), &query, crate::VERSION).await).into_response()
}

/// Trim and check a source from a request body, the way `PluginsConfigDto`'s
/// prepare does for the file; the problems of a rejected one, all at once.
#[cfg(feature = "plugins")]
fn validated(body: &PluginRepositoryDto) -> Result<PluginRepositoryDto, Vec<String>> {
    let dto = PluginRepositoryDto {
        id: body.id.trim().to_string(),
        name: body.name.trim().to_string(),
        url: body.url.trim().to_string(),
        enabled: body.enabled,
    };
    let mut problems = Vec::new();
    if !is_valid_plugin_id(&dto.id) {
        problems.push(format!(
            "id `{}` is invalid: use lowercase letters, digits and `-`, starting with a letter, 3 to 41 characters",
            dto.id
        ));
    }
    if dto.name.is_empty() {
        problems.push("name must not be blank".to_string());
    }
    if let Err(problem) = validate_repository(&PluginRepository::from(&dto), marketplace_allow_http()) {
        problems.push(problem);
    }
    if problems.is_empty() {
        Ok(dto)
    } else {
        Err(problems)
    }
}

#[cfg(feature = "plugins")]
fn invalid_response(problems: Vec<String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(PluginUploadErrorDto { error: "invalid marketplace source".to_string(), problems }),
    )
        .into_response()
}

/// Write `change` to `config.yml` and the runtime config; the caller holds the
/// install lock and the `config.yml` write lock.
#[cfg(feature = "plugins")]
async fn store_change(app_state: &AppState, change: RepositoryChange) -> Result<(), Box<Response>> {
    match persist_repositories(&app_state.app_config, change).await {
        Ok(_) => Ok(()),
        Err(err @ RepositoryStoreError::Duplicate(_)) => {
            Err(Box::new(error_response(StatusCode::CONFLICT, err.to_string())))
        }
        Err(err @ RepositoryStoreError::NotFound(_)) => {
            Err(Box::new(error_response(StatusCode::NOT_FOUND, err.to_string())))
        }
        Err(RepositoryStoreError::Store(err)) => {
            log::error!("Failed to save the marketplace sources: {err}");
            Err(Box::new(error_response(StatusCode::INTERNAL_SERVER_ERROR, "cannot save config.yml")))
        }
    }
}

/// Create (`replace` = `None`) or replace the source `replace`, answering its info row.
#[cfg(feature = "plugins")]
async fn save_repository(app_state: Arc<AppState>, body: PluginRepositoryDto, replace: Option<String>) -> Response {
    if replace.as_ref().is_some_and(|id| body.id.trim() != id) {
        return invalid_response(vec!["the body id must equal the id in the path".to_string()]);
    }
    let dto = match validated(&body) {
        Ok(dto) => dto,
        Err(problems) => return invalid_response(problems),
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
        let change = match &replace {
            Some(id) => RepositoryChange::Replace(id.clone(), dto.clone()),
            None => RepositoryChange::Add(dto.clone()),
        };
        if let Err(response) = store_change(&app_state, change).await {
            return *response;
        }
        forget_first_sync(&dto.id);
        log::info!("Marketplace source `{}` saved", dto.id);
        Json(source_info(&plugins.marketplace_data_dir(), &PluginRepository::from(&dto)).await).into_response()
    })
    .await
}

/// `POST /api/v1/plugins/repositories`: add a source. `422` with the problems,
/// `409` for a duplicate id.
#[cfg(feature = "plugins")]
async fn post_repository(State(app_state): State<Arc<AppState>>, Json(body): Json<PluginRepositoryDto>) -> Response {
    if app_state.plugins.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_repository(app_state, body, None).await
}

/// `PUT /api/v1/plugins/repositories/{id}`: replace a source; the body id must
/// match the path. `404` for an unknown id.
#[cfg(feature = "plugins")]
async fn put_repository(
    State(app_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginRepositoryDto>,
) -> Response {
    if app_state.plugins.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_repository(app_state, body, Some(id)).await
}

/// `DELETE /api/v1/plugins/repositories/{id}`: drop the config entry and the
/// cache of the source; installed plugins stay. Answers `204`.
#[cfg(feature = "plugins")]
async fn delete_repository(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    detached(async move {
        let Some(plugins) = app_state.plugins.as_ref() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let _guard = plugins.install_lock().await;
        let file_path = config_file_path(&app_state);
        let _lock = app_state.app_config.file_locks.write_lock(std::path::Path::new(&file_path)).await;
        if let Err(response) = store_change(&app_state, RepositoryChange::Remove(id.clone())).await {
            return *response;
        }
        remove_cache(&plugins.marketplace_data_dir(), &id).await;
        forget_first_sync(&id);
        log::info!("Marketplace source `{id}` removed");
        StatusCode::NO_CONTENT.into_response()
    })
    .await
}

/// `POST /api/v1/plugins/repositories/{id}/sync`: fetch the catalog now. `404`
/// for an unknown id, the error status (`502`, `413`, `403`) with a
/// `PluginUploadErrorDto` when the fetch fails (the cache keeps the old catalog
/// and stores the error).
#[cfg(feature = "plugins")]
async fn post_repository_sync(State(app_state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let sources = match written_sources(&app_state).await {
        Ok(sources) => sources,
        Err(response) => return *response,
    };
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(source) = sources.iter().find(|source| source.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match sync_one(&app_state, &plugins.marketplace_data_dir(), source).await {
        Ok(info) => Json(info).into_response(),
        Err(err) => error_response(err.status(), err.to_string()),
    }
}

/// `POST /api/v1/plugins/marketplace/install`: download a catalog release and
/// stage it for review. `404` for an unknown or disabled source or an unknown
/// release, `409` for an incompatible one, the fetch error status when the
/// download fails, `422` when the package is not the requested plugin. Nothing
/// is approved or enabled.
#[cfg(feature = "plugins")]
async fn post_marketplace_install(
    State(app_state): State<Arc<AppState>>,
    Json(body): Json<MarketplaceInstallDto>,
) -> Response {
    let sources = match written_sources(&app_state).await {
        Ok(sources) => sources,
        Err(response) => return *response,
    };
    let Some(plugins) = app_state.plugins.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(source) = sources.iter().find(|source| source.id == body.source && source.enabled) else {
        return error_response(StatusCode::NOT_FOUND, format!("marketplace source `{}` is not available", body.source));
    };
    let data_dir = plugins.marketplace_data_dir();
    let release = match resolve_release(&data_dir, source, &body.id, &body.version, crate::VERSION).await {
        ReleaseLookup::Found(release) => release,
        ReleaseLookup::NotFound => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("`{}` {} is not in the catalog of `{}`", body.id, body.version, body.source),
            );
        }
        ReleaseLookup::Incompatible(reason) => return error_response(StatusCode::CONFLICT, reason),
    };
    // Download before the install lock, like the upload reads its body first.
    let policy = FetchPolicy::from_env();
    let client = if policy.allow_http {
        app_state.http_clients.no_redirect.load_full()
    } else {
        app_state.http_clients.resource_public_no_redirect.load_full()
    };
    let package = match fetch_package(&client, policy, &release.package_url, PACKAGE_MAX_BYTES).await {
        Ok(package) => package,
        Err(err) => return error_response(err.status(), err.to_string()),
    };
    let expect = MarketplaceExpect {
        id: body.id,
        version: body.version.clone(),
        origin: PluginOriginDto { source: body.source, version: body.version },
    };
    let guard = plugins.install_lock().await;
    match plugins.api_upload_from_marketplace(&guard, package, expect).await {
        Ok(review) => Json(review).into_response(),
        Err(rejection) => (rejection.status, Json(rejection.body)).into_response(),
    }
}

#[cfg(not(feature = "plugins"))]
async fn get_repositories() -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn get_marketplace(Query(_query): Query<MarketplaceQueryDto>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(not(feature = "plugins"))]
async fn post_repository(Json(_body): Json<PluginRepositoryDto>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn put_repository(Path(_id): Path<String>, Json(_body): Json<PluginRepositoryDto>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(not(feature = "plugins"))]
async fn delete_repository(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn post_repository_sync(Path(_id): Path<String>) -> Response { StatusCode::NOT_FOUND.into_response() }

#[cfg(not(feature = "plugins"))]
async fn post_marketplace_install(Json(_body): Json<MarketplaceInstallDto>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request};
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn router(dir: &TempDir) -> Router {
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        marketplace_read_routes().merge(marketplace_write_routes()).with_state(create_test_app_state(config))
    }

    async fn status(router: &Router, method: &str, uri: &str, body: &'static str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        router.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn plugins_off_answers_every_marketplace_route_with_404() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir);
        let repository = r#"{"id":"community","name":"Community","url":"https://example.com/marketplace.json"}"#;
        let install = r#"{"source":"community","id":"demo","version":"1.0.0"}"#;
        for (method, uri, body) in [
            ("GET", "/plugins/repositories", ""),
            ("POST", "/plugins/repositories", repository),
            ("PUT", "/plugins/repositories/community", repository),
            ("DELETE", "/plugins/repositories/community", ""),
            ("POST", "/plugins/repositories/community/sync", ""),
            ("GET", "/plugins/marketplace?q=demo&category=epg&source=community", ""),
            ("POST", "/plugins/marketplace/install", install),
        ] {
            assert_eq!(status(&router, method, uri, body).await, StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }

    #[cfg(feature = "plugins")]
    mod store {
        use super::*;
        use crate::model::AppConfig;

        fn app_config_for(dir: &TempDir) -> Arc<AppConfig> {
            let file = dir.path().join("config.yml");
            std::fs::write(&file, "api:\n  host: 127.0.0.1\n  port: 8901\n").unwrap();
            let config = Config {
                backup_dir: Some(dir.path().join("backup").to_string_lossy().to_string()),
                ..Config::default()
            };
            let app_config = create_test_app_state(config).app_config.clone();
            let mut paths = app_config.paths.load().as_ref().clone();
            paths.config_file_path = file.to_string_lossy().to_string();
            app_config.paths.store(Arc::new(paths));
            app_config
        }

        #[tokio::test]
        async fn an_add_keeps_the_env_reference_in_the_file_and_the_runtime_config() {
            let dir = TempDir::new().unwrap();
            let app_config = app_config_for(&dir);
            let dto = PluginRepositoryDto {
                id: "community".to_string(),
                name: "Community".to_string(),
                url: "${env:TP_TEST_MARKETPLACE_URL}".to_string(),
                enabled: true,
            };
            persist_repositories(&app_config, RepositoryChange::Add(dto.clone())).await.unwrap();
            let written =
                super::super::super::plugins_repositories_store::written_repositories(&app_config).await.unwrap();
            assert_eq!(written, vec![dto.clone()]);
            let file = std::fs::read_to_string(dir.path().join("config.yml")).unwrap();
            assert!(file.contains("${env:TP_TEST_MARKETPLACE_URL}"));
            let runtime = app_config.plugins.load_full().unwrap();
            assert_eq!(PluginRepositoryDto::from(&runtime.repositories[0]), dto);
            let duplicate = persist_repositories(&app_config, RepositoryChange::Add(dto)).await;
            assert!(matches!(duplicate, Err(RepositoryStoreError::Duplicate(_))));
        }

        #[test]
        fn validation_collects_every_problem_without_echoing_a_resolved_url() {
            let bad = PluginRepositoryDto {
                id: "X".to_string(),
                name: "  ".to_string(),
                url: "${env:TP_TEST_MARKETPLACE_UNSET}".to_string(),
                enabled: true,
            };
            let problems = validated(&bad).unwrap_err();
            assert_eq!(problems.len(), 3, "{problems:?}");
            let good = PluginRepositoryDto {
                id: " community ".to_string(),
                name: " Community ".to_string(),
                url: " https://example.com/marketplace.json ".to_string(),
                enabled: true,
            };
            let trimmed = validated(&good).unwrap();
            assert_eq!((trimmed.id.as_str(), trimmed.name.as_str()), ("community", "Community"));
        }
    }
}
