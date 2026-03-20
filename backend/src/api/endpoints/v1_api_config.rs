use crate::{api::{
    api_utils::{internal_server_error, try_unwrap_body},
    config_file::ConfigFile,
    model::AppState,
}, auth::{verify_token, AuthBearer, permission_layer}, model::{validate_library_paths_from_dto, ApiProxyConfig, InputSource}, utils, utils::{
    persist_messaging_templates, prepare_sources_batch, prepare_users, read_api_proxy_file,
    request::download_text_content,
    xtream::{get_xtream_stream_url_base, xtream_login},
}};
use axum::{
    http::{header::IF_MATCH, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    Router,
};
use log::error;
use serde_json::json;
use shared::{
    error::TuliproxError,
    model::permission::{Permission, PermissionSet},
    model::{ApiProxyConfigDto, ConfigDto, SourcesConfigDto, XtreamLoginRequest},
    utils::{
        HEADER_CONFIG_API_PROXY_REVISION, HEADER_CONFIG_MAIN_REVISION, HEADER_CONFIG_SOURCES_REVISION, HEADER_IF_MATCH,
    },
};
use shared::model::InputFetchMethod;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn file_revision_from_bytes(bytes: &[u8]) -> String { blake3::hash(bytes).to_hex().to_string() }

async fn read_file_revision(path: &str) -> Result<String, std::io::Error> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(file_revision_from_bytes(&bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok("missing".to_string()),
        Err(err) => Err(err),
    }
}

fn response_with_revision_header(
    mut response: axum::response::Response,
    revision_header: &'static str,
    revision: &str,
) -> axum::response::Response {
    let Ok(header_name) = HeaderName::try_from(revision_header) else {
        return response;
    };
    let Ok(header_value) = HeaderValue::from_str(revision) else {
        return response;
    };
    response.headers_mut().insert(header_name, header_value);
    response
}

fn require_matching_revision(
    headers: &HeaderMap,
    current_revision: &str,
    revision_header: &'static str,
    file_label: &str,
) -> Option<axum::response::Response> {
    let if_match = headers.get(IF_MATCH).and_then(|value| value.to_str().ok()).map(str::trim);
    let Some(if_match) = if_match.filter(|value| !value.is_empty()) else {
        let response = (
            StatusCode::PRECONDITION_REQUIRED,
            axum::Json(json!({
                "error": format!("Missing required '{}' header for {file_label}", HEADER_IF_MATCH)
            })),
        )
            .into_response();
        return Some(response_with_revision_header(response, revision_header, current_revision));
    };
    if if_match != current_revision {
        let response = (
            StatusCode::CONFLICT,
            axum::Json(json!({
                "error": format!("{file_label} changed on server. Reload configuration and retry save."),
            })),
        )
            .into_response();
        return Some(response_with_revision_header(response, revision_header, current_revision));
    }
    None
}

fn has_any_permission(permissions: PermissionSet, required: &[Permission]) -> bool {
    required.iter().any(|permission| permissions.contains(*permission))
}

fn decode_permissions(app_state: &AppState, token: &str) -> Option<PermissionSet> {
    let config = app_state.app_config.config.load();
    let web_auth = config.web_ui.as_ref()?.auth.as_ref()?;
    verify_token(token, web_auth.secret.as_bytes()).map(|token_data| token_data.claims.permissions)
}

fn filter_api_proxy_by_permissions(api_proxy: &mut ApiProxyConfigDto, permissions: PermissionSet) {
    if !permissions.contains(Permission::ConfigRead) {
        api_proxy.server.clear();
    }
    if !permissions.contains(Permission::UserRead) {
        api_proxy.user.clear();
    }
}

fn filter_app_config_by_permissions(app_config: &mut shared::model::AppConfigDto, permissions: PermissionSet) {
    if !permissions.contains(Permission::ConfigRead) {
        app_config.config = ConfigDto::default();
        if let Some(api_proxy) = app_config.api_proxy.as_mut() {
            api_proxy.server.clear();
        }
    }

    if !permissions.contains(Permission::SourceRead) {
        app_config.sources = SourcesConfigDto::default();
        app_config.mappings = None;
        app_config.templates = None;
    }

    if let Some(api_proxy) = app_config.api_proxy.as_mut() {
        if !permissions.contains(Permission::UserRead) {
            api_proxy.user.clear();
        }
    }
}

pub(in crate::api::endpoints) async fn intern_save_config_api_proxy(
    backup_dir: &str,
    api_proxy: &ApiProxyConfigDto,
    file_path: &str,
) -> Option<TuliproxError> {
    match utils::save_api_proxy(file_path, backup_dir, api_proxy).await {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to save api_proxy.yml {err}");
            return Some(err);
        }
    }
    None
}

async fn intern_save_config_main(file_path: &str, backup_dir: &str, cfg: &ConfigDto) -> Option<TuliproxError> {
    match utils::save_main_config(file_path, backup_dir, cfg).await {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to save config.yml {err}");
            return Some(err);
        }
    }
    None
}

async fn save_config_main(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Json(mut cfg): axum::extract::Json<ConfigDto>,
) -> impl axum::response::IntoResponse + Send {
    let (file_path, backup_dir) = {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        (paths.config_file_path.clone(), config.get_backup_dir().to_string())
    };

    let _lock = app_state.app_config.file_locks.write_lock(Path::new(&file_path)).await;
    let current_revision = match read_file_revision(&file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for config.yml '{file_path}': {err}");
            return internal_server_error!();
        }
    };
    if let Some(response) =
        require_matching_revision(&headers, &current_revision, HEADER_CONFIG_MAIN_REVISION, "config.yml")
    {
        return response;
    }

    if let Err(err) = cfg.prepare(false) {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response();
    }
    if !cfg.is_valid() {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid content"}))).into_response()
    } else if let Err(err) = validate_library_paths_from_dto(&cfg) {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response()
    } else {
        if let Err(err) = persist_messaging_templates(&app_state, &mut cfg).await {
            error!("Failed to persist messaging templates: {err}");
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }

        if let Some(err) = intern_save_config_main(&file_path, &backup_dir, &cfg).await {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
        let updated_revision = match read_file_revision(&file_path).await {
            Ok(revision) => revision,
            Err(err) => {
                error!("Failed to read updated revision for config.yml '{file_path}': {err}");
                return internal_server_error!();
            }
        };
        response_with_revision_header(StatusCode::OK.into_response(), HEADER_CONFIG_MAIN_REVISION, &updated_revision)
    }
}

async fn save_config_sources(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Json(sources): axum::extract::Json<SourcesConfigDto>,
) -> impl axum::response::IntoResponse + Send {
    let sources_file_path = {
        let paths = app_state.app_config.paths.load();
        paths.sources_file_path.clone()
    };
    let _lock = app_state.app_config.file_locks.write_lock(Path::new(&sources_file_path)).await;

    let current_revision = match read_file_revision(&sources_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for source.yml '{sources_file_path}': {err}");
            return internal_server_error!();
        }
    };
    if let Some(response) =
        require_matching_revision(&headers, &current_revision, HEADER_CONFIG_SOURCES_REVISION, "source.yml")
    {
        return response;
    }

    let templates_to_persist = match utils::validate_source_config_for_persist(&app_state, &sources).await {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to validate source.yml {err}");
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    };

    if let Some(template_definition) = templates_to_persist.as_ref() {
        if let Err(err) = utils::persist_templates_config(&app_state, template_definition).await {
            error!("Failed to save template config {err}");
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    }

    match utils::persist_source_config(&app_state, None, sources).await {
        Ok(_) => {}
        Err(err) => {
            error!("Failed to persist source.yml {err}");
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    }

    // Reload from disk so runtime always uses fully prepared sources/mappings/templates.
    if let Err(err) = ConfigFile::load_sources(&app_state).await {
        error!("Failed to reload prepared sources after save {err}");
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
            .into_response();
    }

    app_state.active_provider.update_config(&app_state.app_config).await;
    let updated_revision = match read_file_revision(&sources_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read updated revision for source.yml '{sources_file_path}': {err}");
            return internal_server_error!();
        }
    };
    response_with_revision_header(StatusCode::OK.into_response(), HEADER_CONFIG_SOURCES_REVISION, &updated_revision)
}

async fn get_config_api_proxy_config_public(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let paths = app_state.app_config.paths.load();
    let api_proxy_file_path = paths.api_proxy_file_path.clone();
    let revision = match read_file_revision(&api_proxy_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for api-proxy.yml '{api_proxy_file_path}': {err}");
            return internal_server_error!();
        }
    };
    match read_api_proxy_file(api_proxy_file_path.as_str(), true) {
        Ok(Some(mut api_proxy_dto)) => {
            api_proxy_dto.user = vec![];
            let response = axum::response::Json(api_proxy_dto).into_response();
            return response_with_revision_header(response, HEADER_CONFIG_API_PROXY_REVISION, &revision);
        }
        Ok(None) => {
            error!("Failed to read api proxy config");
        }
        Err(err) => {
            error!("Failed to read api proxy config: {err}");
        }
    }
    internal_server_error!()
}

async fn save_config_api_proxy_config(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Json(mut req_api_proxy): axum::extract::Json<ApiProxyConfigDto>,
) -> impl IntoResponse + Send {
    let (api_proxy_file_path, backup_dir) = {
        let paths = app_state.app_config.paths.load();
        let config = app_state.app_config.config.load();
        (paths.api_proxy_file_path.clone(), config.get_backup_dir().to_string())
    };
    let _lock = app_state.app_config.file_locks.write_lock(Path::new(&api_proxy_file_path)).await;

    let current_revision = match read_file_revision(&api_proxy_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for api-proxy.yml '{api_proxy_file_path}': {err}");
            return internal_server_error!();
        }
    };
    if let Some(response) = require_matching_revision(
        &headers,
        &current_revision,
        HEADER_CONFIG_API_PROXY_REVISION,
        "api-proxy.yml",
    ) {
        return response;
    }

    for server_info in &mut req_api_proxy.server {
        if !server_info.validate() {
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid content"})))
                .into_response();
        }
    }

    // TODO if hot reload is on, it is loaded twice, avoid this
    // Build the updated config without mutating global state yet
    let base = app_state.app_config.api_proxy.load().as_deref().cloned().unwrap_or_default();
    let updated_api_proxy = ApiProxyConfig {
        use_user_db: req_api_proxy.use_user_db,
        server: req_api_proxy.server.iter().map(Into::into).collect(),
        auth_error_status: req_api_proxy.auth_error_status,
        ..base
    };

    if let Some(err) = intern_save_config_api_proxy(
        &backup_dir,
        &ApiProxyConfigDto::from(&updated_api_proxy),
        &api_proxy_file_path,
    )
    .await
    {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
            .into_response();
    }
    // Persist succeeded — now update in‑memory state
    app_state.app_config.api_proxy.store(Some(Arc::new(updated_api_proxy)));

    let updated_revision = match read_file_revision(&api_proxy_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read updated revision for api-proxy.yml '{api_proxy_file_path}': {err}");
            return internal_server_error!();
        }
    };
    response_with_revision_header(StatusCode::OK.into_response(), HEADER_CONFIG_API_PROXY_REVISION, &updated_revision)
}

async fn config_public(axum::extract::State(app_state): axum::extract::State<Arc<AppState>>) -> impl IntoResponse + Send {
    let (config_file_path, sources_file_path, api_proxy_file_path) = {
        let paths = app_state.app_config.paths.load();
        (
            paths.config_file_path.clone(),
            paths.sources_file_path.clone(),
            paths.api_proxy_file_path.clone(),
        )
    };

    let main_revision = match read_file_revision(&config_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for config.yml '{config_file_path}': {err}");
            return internal_server_error!();
        }
    };
    let sources_revision = match read_file_revision(&sources_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for source.yml '{sources_file_path}': {err}");
            return internal_server_error!();
        }
    };
    let api_proxy_revision = match read_file_revision(&api_proxy_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for api-proxy.yml '{api_proxy_file_path}': {err}");
            return internal_server_error!();
        }
    };

    let read_result = {
        let paths = app_state.app_config.paths.load();
        utils::read_app_config_dto(&paths, true, false).await
    };
    match read_result {
        Ok(mut app_config) => {
            if let Err(err) = prepare_sources_batch(&mut app_config.sources, false).await {
                error!("Failed to prepare sources batch: {err}");
                internal_server_error!()
            } else if let Err(err) = prepare_users(&mut app_config, &app_state.app_config).await {
                error!("Failed to prepare users: {err}");
                internal_server_error!()
            } else {
                let response = axum::response::Json(app_config).into_response();
                let response = response_with_revision_header(response, HEADER_CONFIG_MAIN_REVISION, &main_revision);
                let response = response_with_revision_header(response, HEADER_CONFIG_SOURCES_REVISION, &sources_revision);
                response_with_revision_header(response, HEADER_CONFIG_API_PROXY_REVISION, &api_proxy_revision)
            }
        }
        Err(err) => {
            error!("Failed to read config files: {err}");
            internal_server_error!()
        }
    }
}

async fn config(
    AuthBearer(token): AuthBearer,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let Some(permissions) = decode_permissions(&app_state, &token) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    if !has_any_permission(
        permissions,
        &[Permission::ConfigRead, Permission::SourceRead, Permission::UserRead],
    ) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let (config_file_path, sources_file_path, api_proxy_file_path) = {
        let paths = app_state.app_config.paths.load();
        (
            paths.config_file_path.clone(),
            paths.sources_file_path.clone(),
            paths.api_proxy_file_path.clone(),
        )
    };

    let main_revision = match read_file_revision(&config_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for config.yml '{config_file_path}': {err}");
            return internal_server_error!();
        }
    };
    let sources_revision = match read_file_revision(&sources_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for source.yml '{sources_file_path}': {err}");
            return internal_server_error!();
        }
    };
    let api_proxy_revision = match read_file_revision(&api_proxy_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for api-proxy.yml '{api_proxy_file_path}': {err}");
            return internal_server_error!();
        }
    };

    let read_result = {
        let paths = app_state.app_config.paths.load();
        utils::read_app_config_dto(&paths, true, false).await
    };
    match read_result {
        Ok(mut app_config) => {
            if let Err(err) = prepare_sources_batch(&mut app_config.sources, false).await {
                error!("Failed to prepare sources batch: {err}");
                internal_server_error!()
            } else if let Err(err) = prepare_users(&mut app_config, &app_state.app_config).await {
                error!("Failed to prepare users: {err}");
                internal_server_error!()
            } else {
                filter_app_config_by_permissions(&mut app_config, permissions);
                let response = axum::response::Json(app_config).into_response();
                let response = response_with_revision_header(response, HEADER_CONFIG_MAIN_REVISION, &main_revision);
                let response = response_with_revision_header(response, HEADER_CONFIG_SOURCES_REVISION, &sources_revision);
                response_with_revision_header(response, HEADER_CONFIG_API_PROXY_REVISION, &api_proxy_revision)
            }
        }
        Err(err) => {
            error!("Failed to read config files: {err}");
            internal_server_error!()
        }
    }
}

async fn get_config_api_proxy_config(
    AuthBearer(token): AuthBearer,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let Some(permissions) = decode_permissions(&app_state, &token) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    if !has_any_permission(permissions, &[Permission::ConfigRead, Permission::UserRead]) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let paths = app_state.app_config.paths.load();
    let api_proxy_file_path = paths.api_proxy_file_path.clone();
    let revision = match read_file_revision(&api_proxy_file_path).await {
        Ok(revision) => revision,
        Err(err) => {
            error!("Failed to read revision for api-proxy.yml '{api_proxy_file_path}': {err}");
            return internal_server_error!();
        }
    };
    match read_api_proxy_file(api_proxy_file_path.as_str(), true) {
        Ok(Some(mut api_proxy_dto)) => {
            filter_api_proxy_by_permissions(&mut api_proxy_dto, permissions);
            let response = axum::response::Json(api_proxy_dto).into_response();
            response_with_revision_header(response, HEADER_CONFIG_API_PROXY_REVISION, &revision)
        }
        Ok(None) => {
            error!("Failed to read api proxy config");
            internal_server_error!()
        }
        Err(err) => {
            error!("Failed to read api proxy config: {err}");
            internal_server_error!()
        }
    }
}

async fn config_batch_content(
    axum::extract::Path(input_id): axum::extract::Path<u16>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    if let Some(config_input) = app_state.app_config.get_input_by_id(input_id) {
        // The url is changed at this point, we need the raw url for the batch file
        if let Some(batch_url) = config_input.t_batch_url.as_ref() {
            let input_source = InputSource::from(&*config_input).with_url(batch_url.to_owned());
            return match download_text_content(
                &app_state.app_config,
                &app_state.http_client.load(),
                &input_source,
                None,
                None,
                false,
            )
            .await
            {
                Ok((content, _path)) => {
                    // Return CSV with explicit content-type
                    try_unwrap_body!(axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")
                        .body(content))
                }
                Err(err) => {
                    error!("Failed to read batch file: {err}");
                    internal_server_error!()
                }
            };
        }
    }
    (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"error": "Input not found or batch URL missing"})))
        .into_response()
}

async fn get_xtream_login_info(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<XtreamLoginRequest>,
) -> impl IntoResponse + Send {
    if request.url.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "URL is required"}))).into_response();
    }
    if request.username.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Username is required"}))).into_response();
    }
    if request.password.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Password is required"}))).into_response();
    }

    let base_url = get_xtream_stream_url_base(&request.url, &request.username, &request.password);
    let input_source = InputSource {
        name: "xtream_login".into(),
        url: base_url,
        provider: None,
        username: Some(request.username.clone()),
        password: Some(request.password.clone()),
        method: InputFetchMethod::GET,
        headers: HashMap::new(),
    };
    let http_client = app_state.http_client.load();
    match xtream_login(&app_state.app_config, &http_client, &input_source, &request.username).await {
        Ok(login_info) => axum::Json(login_info.unwrap_or_default()).into_response(),
        Err(err) => {
            error!("Failed to get xtream login info: {err}");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({"error": "Failed to get Xtream login info"})),
            )
                .into_response()
        }
    }
}

pub fn v1_api_config_register(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/config", axum::routing::get(config_public))
        .route("/config/batchContent/{input_id}", axum::routing::get(config_batch_content))
        .route("/config/xtream/login-info", axum::routing::post(get_xtream_login_info))
        .route("/config/main", axum::routing::post(save_config_main))
        .route("/config/sources", axum::routing::post(save_config_sources))
        .route("/config/apiproxy", axum::routing::get(get_config_api_proxy_config_public))
        .route("/config/apiproxy", axum::routing::put(save_config_api_proxy_config))
}
pub fn v1_api_config_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    let base_read = Router::new()
        .route("/config", axum::routing::get(config))
        .route("/config/apiproxy", axum::routing::get(get_config_api_proxy_config));

    // 2. Source Domain (Read & Write)
    let source_read = Router::new()
        .route("/config/batchContent/{input_id}", axum::routing::get(config_batch_content))
        .route("/config/xtream/login-info", axum::routing::post(get_xtream_login_info))
        .layer(permission_layer!(app_state, Permission::SourceRead));

    let source_write = Router::new()
        .route("/config/sources", axum::routing::post(save_config_sources))
        .layer(permission_layer!(app_state, Permission::SourceWrite));

    let config_write = Router::new()
        .route("/config/main", axum::routing::post(save_config_main))
        .route("/config/apiproxy", axum::routing::put(save_config_api_proxy_config))
        .layer(permission_layer!(app_state, Permission::ConfigWrite));

    Router::new()
        .merge(base_read)
        .merge(source_read)
        .merge(source_write)
        .merge(config_write)
}

#[cfg(test)]
mod tests {
    use super::{filter_api_proxy_by_permissions, filter_app_config_by_permissions, require_matching_revision};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use shared::{
        model::{
            ApiProxyConfigDto, ApiProxyServerInfoDto, AppConfigDto, ConfigDto, Permission, PermissionSet, SourcesConfigDto,
            TargetUserDto,
        },
        utils::{HEADER_CONFIG_SOURCES_REVISION, HEADER_IF_MATCH},
    };

    #[test]
    fn require_matching_revision_rejects_missing_if_match_header() {
        let headers = HeaderMap::new();
        let response = require_matching_revision(
            &headers,
            "rev-a",
            HEADER_CONFIG_SOURCES_REVISION,
            "source.yml",
        )
        .expect("missing if-match header must fail");
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
    }

    #[test]
    fn require_matching_revision_accepts_exact_match() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_IF_MATCH,
            HeaderValue::from_str("rev-a").expect("header value should be valid"),
        );
        let result = require_matching_revision(
            &headers,
            "rev-a",
            HEADER_CONFIG_SOURCES_REVISION,
            "source.yml",
        );
        assert!(result.is_none(), "exact revision match should be accepted");
    }

    #[test]
    fn filter_app_config_clears_unauthorized_sections() {
        let permissions: PermissionSet = Permission::ConfigRead.into();
        let mut app_config = AppConfigDto {
            config: ConfigDto {
                storage_dir: Some(String::from("storage")),
                ..ConfigDto::default()
            },
            sources: SourcesConfigDto {
                inputs: vec![],
                sources: vec![],
                provider: Some(vec![]),
                templates: Some(vec![]),
            },
            mappings: None,
            templates: Some(Default::default()),
            api_proxy: Some(ApiProxyConfigDto {
                server: vec![ApiProxyServerInfoDto {
                    name: String::from("main"),
                    protocol: String::from("http"),
                    host: String::from("localhost"),
                    port: None,
                    timezone: String::from("UTC"),
                    message: String::from("hello"),
                    path: None,
                }],
                user: vec![TargetUserDto {
                    target: String::from("target-a"),
                    credentials: vec![],
                }],
                use_user_db: true,
                auth_error_status: 401,
            }),
        };

        filter_app_config_by_permissions(&mut app_config, permissions);

        assert_eq!(app_config.config.storage_dir.as_deref(), Some("storage"));
        assert_eq!(app_config.sources, SourcesConfigDto::default());
        assert!(app_config.templates.is_none());

        let api_proxy = app_config.api_proxy.expect("api proxy should remain present");
        assert_eq!(api_proxy.server.len(), 1);
        assert!(api_proxy.user.is_empty());
        assert!(api_proxy.use_user_db);
    }

    #[test]
    fn filter_api_proxy_keeps_user_section_only_with_user_read() {
        let permissions: PermissionSet = Permission::UserRead.into();
        let mut api_proxy = ApiProxyConfigDto {
            server: vec![ApiProxyServerInfoDto {
                name: String::from("main"),
                protocol: String::from("http"),
                host: String::from("localhost"),
                port: None,
                timezone: String::from("UTC"),
                message: String::from("hello"),
                path: None,
            }],
            user: vec![TargetUserDto {
                target: String::from("target-a"),
                credentials: vec![],
            }],
            use_user_db: true,
            auth_error_status: 401,
        };

        filter_api_proxy_by_permissions(&mut api_proxy, permissions);

        assert!(api_proxy.server.is_empty());
        assert_eq!(api_proxy.user.len(), 1);
        assert!(api_proxy.use_user_db);
    }
}
