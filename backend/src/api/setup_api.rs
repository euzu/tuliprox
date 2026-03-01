use crate::{
    api::api_utils::serve_file,
    auth::generate_password_from_input,
    model::validate_library_paths_from_dto,
    utils::{
        file_exists, get_default_path, get_default_web_root_path, read_api_proxy_file, read_config_file,
        read_sources_file, read_templates_file, resolve_template_persist_file_path, sanitize_sources_for_persist,
    },
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Router,
};
use core::fmt;
use log::{error, info, warn};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared::{
    error::TuliproxError,
    foundation::prepare_templates,
    info_err,
    model::{
        ApiProxyConfigDto, ApiProxyServerInfoDto, AppConfigDto, ConfigApiDto, ConfigDto, ConfigPaths, PatternTemplate,
        SourcesConfigDto, TemplateDefinitionDto, TokenResponse, WebAuthConfigDto, WebUiConfigDto, TOKEN_NO_AUTH,
    },
    utils::{default_kick_secs, hex_encode, DEFAULT_PORT, DEFAULT_WORKING_DIR, USER_FILE},
};
use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    net::{SocketAddr, UdpSocket},
    path::{Component, Path as FsPath, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{oneshot, Mutex, RwLock};
use tower_http::services::ServeDir;

const DEFAULT_SETUP_HOST: &str = "0.0.0.0";
const DEFAULT_SETUP_CUSTOM_STREAM_RESPONSE_PATH: &str = "./resources";
const SETUP_REDACTED_SECRET_VALUE: &str = "__TULIPROX_SETUP_REDACTED__";

#[derive(Clone, Serialize, Deserialize)]
pub struct SetupWebUserCredentialDto {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for SetupWebUserCredentialDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupWebUserCredentialDto")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupCompleteRequestDto {
    pub app_config: AppConfigDto,
    #[serde(default)]
    pub web_users: Vec<SetupWebUserCredentialDto>,
}

struct SetupModeState {
    draft: Arc<RwLock<AppConfigDto>>,
    output_dir: PathBuf,
    config_file_path: PathBuf,
    source_file_path: PathBuf,
    api_proxy_file_path: PathBuf,
    user_file_path: PathBuf,
    web_dir: PathBuf,
    missing_files: Arc<Vec<String>>,
    completed: AtomicBool,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
}

fn detect_machine_ip() -> Option<String> {
    let skip_ip_detect = std::env::var("SKIP_IP_DETECT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"));
    if skip_ip_detect {
        info!("Setup mode: SKIP_IP_DETECT is enabled, skipping UDP host IP detection");
        return None;
    }

    for target in ["1.1.1.1:80", "8.8.8.8:80", "9.9.9.9:80"] {
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if socket.connect(target).is_err() {
            continue;
        }
        let Ok(local_addr) = socket.local_addr() else {
            continue;
        };
        let ip = local_addr.ip();
        if ip.is_ipv4() && !ip.is_unspecified() && !ip.is_loopback() {
            return Some(ip.to_string());
        }
    }
    warn!("Setup mode: unable to detect machine IP via UDP probe targets (1.1.1.1:80, 8.8.8.8:80, 9.9.9.9:80)");
    None
}

fn default_setup_api_server_host() -> String {
    detect_machine_ip().unwrap_or_else(|| {
        warn!("Setup mode: falling back to 127.0.0.1 for default API server host");
        "127.0.0.1".to_string()
    })
}

fn parse_iana_timezone(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.parse::<chrono_tz::Tz>().is_ok() {
        Some(value.to_string())
    } else {
        None
    }
}

fn default_setup_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if let Some(valid_tz) = parse_iana_timezone(&tz) {
            return valid_tz;
        }
        let tz = tz.trim();
        if !tz.is_empty() {
            warn!("Setup mode: ignoring non-IANA TZ value '{tz}'");
        }
    }

    match iana_time_zone::get_timezone() {
        Ok(system_tz) => {
            if let Some(valid_tz) = parse_iana_timezone(&system_tz) {
                return valid_tz;
            }
            warn!("Setup mode: detected system timezone '{system_tz}' is not a valid IANA timezone");
        }
        Err(err) => {
            warn!("Setup mode: failed to resolve system timezone: {err}");
        }
    }

    "UTC".to_string()
}

fn create_default_api_proxy_server() -> ApiProxyServerInfoDto {
    ApiProxyServerInfoDto {
        name: "default".to_string(),
        protocol: "http".to_string(),
        host: default_setup_api_server_host(),
        port: Some(DEFAULT_PORT.to_string()),
        timezone: default_setup_timezone(),
        message: "Welcome to tuliprox".to_string(),
        path: None,
    }
}

fn create_default_config_dto() -> ConfigDto {
    let auth = WebAuthConfigDto {
        issuer: "tuliprox".to_string(),
        secret: generate_web_auth_secret(),
        userfile: Some(USER_FILE.to_string()),
        ..WebAuthConfigDto::default()
    };

    let web_ui = WebUiConfigDto { auth: Some(auth), kick_secs: default_kick_secs(), ..WebUiConfigDto::default() };

    ConfigDto {
        api: ConfigApiDto {
            host: DEFAULT_SETUP_HOST.to_string(),
            port: DEFAULT_PORT,
            web_root: get_default_web_root_path().display().to_string(),
        },
        working_dir: get_default_path(DEFAULT_WORKING_DIR).display().to_string(),
        custom_stream_response_path: Some(DEFAULT_SETUP_CUSTOM_STREAM_RESPONSE_PATH.to_string()),
        web_ui: Some(web_ui),
        ..ConfigDto::default()
    }
}

fn create_default_draft() -> AppConfigDto {
    AppConfigDto {
        config: create_default_config_dto(),
        sources: SourcesConfigDto::default(),
        mappings: None,
        templates: None,
        api_proxy: Some(ApiProxyConfigDto {
            server: vec![create_default_api_proxy_server()],
            user: vec![],
            use_user_db: false,
        }),
    }
}

fn build_initial_draft(paths: &ConfigPaths) -> AppConfigDto {
    let mut draft = create_default_draft();

    if file_exists(&paths.config_file_path) {
        match read_config_file(paths.config_file_path.as_str(), true, false) {
            Ok(cfg) => draft.config = cfg,
            Err(err) => warn!("Setup mode: failed to load existing config.yml: {err}"),
        }
    }

    if file_exists(&paths.sources_file_path) {
        match read_sources_file(
            paths.sources_file_path.as_str(),
            false,
            false,
            draft.config.get_hdhr_device_overview().as_ref(),
            None,
        ) {
            Ok(src) => draft.sources = src,
            Err(err) => warn!("Setup mode: failed to load existing source.yml: {err}"),
        }
    }

    if file_exists(&paths.api_proxy_file_path) {
        match read_api_proxy_file(paths.api_proxy_file_path.as_str(), true) {
            Ok(Some(api_proxy)) => draft.api_proxy = Some(api_proxy),
            Ok(None) => {}
            Err(err) => warn!("Setup mode: failed to load existing api-proxy.yml: {err}"),
        }
    }

    if draft.api_proxy.as_ref().is_some_and(|a| a.server.is_empty()) {
        draft.api_proxy = Some(ApiProxyConfigDto {
            server: vec![create_default_api_proxy_server()],
            user: vec![],
            use_user_db: false,
        });
    }
    draft
}

fn generate_web_auth_secret() -> String {
    let secret: [u8; 32] = rand::rng().random();
    hex_encode(&secret).to_lowercase()
}

fn ensure_setup_defaults(config: &mut ConfigDto) {
    if config.api.host.trim().is_empty() {
        config.api.host = DEFAULT_SETUP_HOST.to_string();
    }
    if config.api.port == 0 {
        config.api.port = DEFAULT_PORT;
    }
    if config.api.web_root.trim().is_empty() {
        config.api.web_root = get_default_web_root_path().display().to_string();
    }
    if config.working_dir.trim().is_empty() {
        config.working_dir = get_default_path(DEFAULT_WORKING_DIR).display().to_string();
    }
    if config.custom_stream_response_path.as_ref().is_none_or(|path| path.trim().is_empty()) {
        config.custom_stream_response_path = Some(DEFAULT_SETUP_CUSTOM_STREAM_RESPONSE_PATH.to_string());
    }

    if config.web_ui.is_none() {
        config.web_ui = Some(WebUiConfigDto::default());
    }
    if let Some(web_ui) = config.web_ui.as_mut() {
        if web_ui.auth.is_none() {
            web_ui.auth = Some(WebAuthConfigDto::default());
        }
        if let Some(auth) = web_ui.auth.as_mut() {
            auth.enabled = true;
            if auth.issuer.trim().is_empty() {
                auth.issuer = "tuliprox".to_string();
            }
            if auth.secret.trim().is_empty() {
                auth.secret = generate_web_auth_secret();
            }
            auth.userfile = Some(USER_FILE.to_string());
        }
    }
}

fn setup_bind_values(draft: &AppConfigDto) -> (String, u16, PathBuf) {
    let host = if draft.config.api.host.trim().is_empty() {
        DEFAULT_SETUP_HOST.to_string()
    } else {
        draft.config.api.host.clone()
    };
    let port = if draft.config.api.port == 0 { DEFAULT_PORT } else { draft.config.api.port };
    let web_root = if draft.config.api.web_root.trim().is_empty() {
        get_default_web_root_path()
    } else {
        PathBuf::from(&draft.config.api.web_root)
    };
    (host, port, web_root)
}

fn resolve_setup_web_dir(web_root: &FsPath) -> Option<PathBuf> {
    if web_root.exists() && web_root.is_dir() {
        return Some(web_root.to_path_buf());
    }
    let fallback = get_default_web_root_path();
    if fallback.exists() && fallback.is_dir() {
        return Some(fallback);
    }
    None
}

fn api_proxy_or_default(draft: &AppConfigDto) -> ApiProxyConfigDto {
    draft.api_proxy.clone().unwrap_or_else(|| ApiProxyConfigDto {
        server: vec![create_default_api_proxy_server()],
        user: vec![],
        use_user_db: false,
    })
}

fn is_setup_redacted_value(value: &str) -> bool { value == SETUP_REDACTED_SECRET_VALUE }

fn redact_non_empty_secret(value: &mut String) {
    if !value.trim().is_empty() {
        *value = SETUP_REDACTED_SECRET_VALUE.to_string();
    }
}

fn redact_optional_secret(value: &mut Option<String>) {
    if value.as_ref().is_some_and(|entry| !entry.trim().is_empty()) {
        *value = Some(SETUP_REDACTED_SECRET_VALUE.to_string());
    }
}

fn redact_api_proxy_user_credentials(api_proxy: &mut ApiProxyConfigDto) {
    for target in &mut api_proxy.user {
        for user in &mut target.credentials {
            redact_non_empty_secret(&mut user.password);
            redact_optional_secret(&mut user.token);
        }
    }
}

fn redact_app_config_for_setup(mut app_config: AppConfigDto) -> AppConfigDto {
    if let Some(web_ui) = app_config.config.web_ui.as_mut() {
        if let Some(auth) = web_ui.auth.as_mut() {
            redact_non_empty_secret(&mut auth.secret);
        }
    }
    if let Some(api_proxy) = app_config.api_proxy.as_mut() {
        redact_api_proxy_user_credentials(api_proxy);
    }
    app_config
}

fn redact_api_proxy_for_setup(mut api_proxy: ApiProxyConfigDto) -> ApiProxyConfigDto {
    redact_api_proxy_user_credentials(&mut api_proxy);
    api_proxy
}

fn restore_redacted_web_auth_secret(app_config: &mut AppConfigDto, draft: &AppConfigDto) {
    let Some(web_ui) = app_config.config.web_ui.as_mut() else {
        return;
    };
    let Some(auth) = web_ui.auth.as_mut() else {
        return;
    };
    if !is_setup_redacted_value(&auth.secret) {
        return;
    }

    let draft_secret = draft
        .config
        .web_ui
        .as_ref()
        .and_then(|draft_web_ui| draft_web_ui.auth.as_ref())
        .map(|draft_auth| draft_auth.secret.trim().to_string())
        .filter(|secret| !secret.is_empty());
    if let Some(draft_secret) = draft_secret {
        auth.secret = draft_secret;
    }
}

fn restore_redacted_api_proxy_credentials(
    api_proxy: &mut ApiProxyConfigDto,
    draft_api_proxy: Option<&ApiProxyConfigDto>,
) {
    let mut credentials_by_username: HashMap<String, (String, Option<String>)> = HashMap::new();
    if let Some(draft) = draft_api_proxy {
        for target in &draft.user {
            for user in &target.credentials {
                credentials_by_username.insert(user.username.clone(), (user.password.clone(), user.token.clone()));
            }
        }
    }

    for target in &mut api_proxy.user {
        for user in &mut target.credentials {
            let draft_credentials = credentials_by_username.get(&user.username);
            if is_setup_redacted_value(&user.password) {
                if let Some((password, _)) = draft_credentials {
                    user.password = password.clone();
                }
            }
            if user.token.as_deref().is_some_and(is_setup_redacted_value) {
                if let Some((_, token)) = draft_credentials {
                    user.token = token.clone();
                }
            }
        }
    }
}

fn restore_redacted_setup_values(app_config: &mut AppConfigDto, draft: &AppConfigDto) {
    restore_redacted_web_auth_secret(app_config, draft);
    if let Some(api_proxy) = app_config.api_proxy.as_mut() {
        restore_redacted_api_proxy_credentials(api_proxy, draft.api_proxy.as_ref());
    }
}

fn has_unresolved_redacted_setup_values(app_config: &AppConfigDto) -> bool {
    if app_config
        .config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.auth.as_ref())
        .is_some_and(|auth| is_setup_redacted_value(&auth.secret))
    {
        return true;
    }

    app_config.api_proxy.as_ref().is_some_and(|api_proxy| {
        api_proxy.user.iter().any(|target| {
            target.credentials.iter().any(|user| {
                is_setup_redacted_value(&user.password) || user.token.as_deref().is_some_and(is_setup_redacted_value)
            })
        })
    })
}

fn setup_templates_from_request(app_config: &AppConfigDto) -> Option<Vec<PatternTemplate>> {
    app_config
        .templates
        .as_ref()
        .and_then(|definition| (!definition.templates.is_empty()).then_some(definition.templates.clone()))
}

fn setup_templates_to_persist(app_config: &AppConfigDto) -> Option<TemplateDefinitionDto> {
    if let Some(templates) = setup_templates_from_request(app_config) {
        return Some(TemplateDefinitionDto { templates });
    }
    app_config
        .sources
        .templates
        .as_ref()
        .filter(|templates| !templates.is_empty())
        .map(|templates| TemplateDefinitionDto { templates: templates.clone() })
}

fn collect_setup_validation_templates(
    app_config: &AppConfigDto,
    template_file_path: &str,
) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
    let mut merged_templates: Vec<PatternTemplate> = Vec::new();
    if let Some(templates) = setup_templates_from_request(app_config) {
        merged_templates.extend(templates);
    }

    if let Some((_, definition)) = read_templates_file(template_file_path, true)? {
        merged_templates.extend(definition.templates);
    }
    if let Some(templates) = app_config.sources.templates.as_ref() {
        merged_templates.extend(templates.clone());
    }
    if let Some(templates) = app_config.mappings.as_ref().and_then(|mappings| mappings.mappings.templates.as_ref()) {
        merged_templates.extend(templates.clone());
    }

    if merged_templates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(merged_templates))
    }
}

fn prepare_setup_validation_templates(
    app_config: &AppConfigDto,
    template_file_path: &str,
) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
    let Some(mut templates) = collect_setup_validation_templates(app_config, template_file_path)? else {
        return Ok(None);
    };
    prepare_templates(&mut templates)?;
    Ok(Some(templates))
}

async fn setup_healthcheck(State(state): State<Arc<SetupModeState>>) -> impl IntoResponse + Send {
    axum::Json(json!({
        "status": "setup",
        "mode": "setup",
        "missing_files": state.missing_files.as_ref(),
        "output_dir": state.output_dir.display().to_string()
    }))
    .into_response()
}

async fn setup_token() -> impl IntoResponse + Send {
    axum::Json(TokenResponse { token: TOKEN_NO_AUTH.to_string(), username: "setup".to_string() }).into_response()
}

async fn setup_token_refresh() -> impl IntoResponse + Send {
    axum::Json(TokenResponse { token: TOKEN_NO_AUTH.to_string(), username: "setup".to_string() }).into_response()
}

async fn setup_get_config(State(state): State<Arc<SetupModeState>>) -> impl IntoResponse + Send {
    let draft = state.draft.read().await.clone();
    axum::Json(redact_app_config_for_setup(draft)).into_response()
}

async fn setup_get_api_proxy(State(state): State<Arc<SetupModeState>>) -> impl IntoResponse + Send {
    let draft = state.draft.read().await.clone();
    axum::Json(redact_api_proxy_for_setup(api_proxy_or_default(&draft))).into_response()
}

async fn setup_config_json(State(state): State<Arc<SetupModeState>>) -> impl IntoResponse + Send {
    let config_json_path = state.web_dir.join("config.json");
    match tokio::fs::read_to_string(&config_json_path).await {
        Ok(content) => {
            let mut json_data = match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json_data) => json_data,
                Err(err) => {
                    warn!(
                        "Setup mode: failed to parse config.json at {}: {err}. Falling back to empty object.",
                        config_json_path.display()
                    );
                    json!({})
                }
            };
            json_data["setupMode"] = json!(true);
            match serde_json::to_string(&json_data) {
                Ok(serialized) => {
                    return (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref())],
                        serialized,
                    )
                        .into_response();
                }
                Err(err) => {
                    error!("Setup mode: failed to serialize config.json: {err}");
                }
            }
        }
        Err(err) => {
            error!("Setup mode: failed to read config.json: {err}");
        }
    }
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn setup_index(State(state): State<Arc<SetupModeState>>) -> impl IntoResponse + Send {
    serve_file(&state.web_dir.join("index.html"), mime::TEXT_HTML_UTF_8.to_string(), None).await.into_response()
}

async fn setup_root_file(
    Path(filename): Path<String>,
    State(state): State<Arc<SetupModeState>>,
) -> impl IntoResponse + Send {
    if filename.contains('/') || filename.contains('\\') {
        return StatusCode::NOT_FOUND.into_response();
    }

    let requested_path = FsPath::new(&filename);
    if requested_path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    let canonical_web_dir = match tokio::fs::canonicalize(&state.web_dir).await {
        Ok(path) => path,
        Err(err) => {
            error!("Setup mode: failed to canonicalize web root '{}': {err}", state.web_dir.display());
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let file_path = state.web_dir.join(&filename);
    match tokio::fs::canonicalize(&file_path).await {
        Ok(canonical_file_path) => {
            if !canonical_file_path.starts_with(&canonical_web_dir) {
                return StatusCode::NOT_FOUND.into_response();
            }
            if canonical_file_path.is_file() {
                let mime_type = mime_guess::from_path(&canonical_file_path).first_or_octet_stream().to_string();
                return serve_file(&canonical_file_path, mime_type, None).await.into_response();
            }
        }
        Err(err) if err.kind() != ErrorKind::NotFound => {
            warn!("Setup mode: failed to canonicalize requested file '{}': {err}", file_path.display());
        }
        Err(_) => {}
    }

    setup_index(State(state)).await.into_response()
}

async fn api_not_found() -> impl IntoResponse + Send { StatusCode::NOT_FOUND.into_response() }

async fn persist_yaml_file<T: serde::Serialize>(file_path: &FsPath, payload: &T) -> Result<(), String> {
    let mut content = String::new();
    let options = serde_saphyr::SerializerOptions { prefer_block_scalars: false, ..Default::default() };
    serde_saphyr::to_fmt_writer_with_options(&mut content, payload, options).map_err(|err| err.to_string())?;
    tokio::fs::write(file_path, content).await.map_err(|err| err.to_string())
}

async fn ensure_parent_dir(file_path: &FsPath) -> Result<(), String> {
    if let Some(parent_dir) = file_path.parent() {
        tokio::fs::create_dir_all(parent_dir).await.map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn validate_web_users(users: &[SetupWebUserCredentialDto]) -> Result<Vec<(String, String)>, String> {
    if users.is_empty() {
        return Err("At least one WebUI user is required".to_string());
    }

    let mut usernames = HashSet::new();
    let mut normalized = Vec::with_capacity(users.len());
    for user in users {
        let username = user.username.trim().to_string();
        let password = user.password.clone();

        if username.is_empty() {
            return Err("WebUI username cannot be empty".to_string());
        }
        if password.is_empty() {
            return Err(format!("Password cannot be empty for user '{username}'"));
        }
        if password.len() < 8 {
            return Err(format!("Password for user '{username}' must be at least 8 characters"));
        }
        if !usernames.insert(username.to_lowercase()) {
            return Err(format!("Duplicate WebUI username '{username}'"));
        }
        normalized.push((username, password));
    }
    Ok(normalized)
}

fn create_setup_temp_path(file_path: &FsPath) -> PathBuf {
    let file_name =
        file_path.file_name().and_then(|value| value.to_str()).filter(|value| !value.is_empty()).unwrap_or("setup");
    let temp_name = format!(".{file_name}.tmp-{}-{}", std::process::id(), rand::rng().random::<u64>());
    match file_path.parent() {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    }
}

async fn replace_file_atomic(source: &FsPath, target: &FsPath) -> std::io::Result<()> {
    // Windows fallback note:
    // `rename(source, target)` may fail when `target` exists, so we try remove+rename there.
    // That sequence is not atomic and has a TOCTOU race window if another process/thread
    // touches `target` between the remove and rename calls. We currently accept this
    // limitation for setup mode writes; a stricter approach would use a platform-specific
    // atomic replacement API (for example Windows ReplaceFile) in a dedicated implementation.
    match tokio::fs::rename(source, target).await {
        Ok(()) => Ok(()),
        Err(err) => {
            #[cfg(windows)]
            {
                if target.exists() {
                    tokio::fs::remove_file(target).await?;
                    return tokio::fs::rename(source, target).await;
                }
            }
            Err(err)
        }
    }
}

async fn restore_file_snapshot(file_path: &FsPath, snapshot: Option<&[u8]>) -> Result<(), String> {
    match snapshot {
        Some(content) => tokio::fs::write(file_path, content).await.map_err(|err| err.to_string()),
        None => match tokio::fs::remove_file(file_path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        },
    }
}

async fn cleanup_temp_file(file_path: &FsPath) {
    if let Err(err) = tokio::fs::remove_file(file_path).await {
        if err.kind() != ErrorKind::NotFound {
            warn!("Setup mode: failed to remove temp file '{}': {err}", file_path.display());
        }
    }
}

enum SetupPersistAction<'a> {
    Config(&'a ConfigDto),
    Sources(&'a SourcesConfigDto),
    Templates(&'a TemplateDefinitionDto),
    ApiProxy(&'a ApiProxyConfigDto),
    UserFile(&'a str),
}

impl SetupPersistAction<'_> {
    async fn write(&self, temp_path: &FsPath) -> Result<(), String> {
        match self {
            Self::Config(payload) => persist_yaml_file(temp_path, *payload).await,
            Self::Sources(payload) => persist_yaml_file(temp_path, *payload).await,
            Self::Templates(payload) => persist_yaml_file(temp_path, *payload).await,
            Self::ApiProxy(payload) => persist_yaml_file(temp_path, *payload).await,
            Self::UserFile(content) => tokio::fs::write(temp_path, content).await.map_err(|err| err.to_string()),
        }
    }
}

async fn setup_complete(
    State(state): State<Arc<SetupModeState>>,
    axum::extract::Json(req): axum::extract::Json<SetupCompleteRequestDto>,
) -> impl IntoResponse + Send {
    if state.completed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return (StatusCode::CONFLICT, axum::Json(json!({ "error": "Setup has already been completed" })))
            .into_response();
    }

    let response = setup_complete_inner(state.clone(), req).await;
    if response.status().is_success() {
        let mut shutdown_tx = state.shutdown_tx.lock().await;
        if let Some(tx) = shutdown_tx.take() {
            let _ = tx.send(());
        }
    } else {
        state.completed.store(false, Ordering::Release);
    }
    response
}

#[allow(clippy::too_many_lines)]
async fn setup_complete_inner(
    state: Arc<SetupModeState>,
    mut req: SetupCompleteRequestDto,
) -> axum::response::Response {
    let draft_snapshot = state.draft.read().await.clone();
    restore_redacted_setup_values(&mut req.app_config, &draft_snapshot);
    if has_unresolved_redacted_setup_values(&req.app_config) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "One or more redacted secret fields could not be restored. Please re-enter secret values before saving setup."
            })),
        )
            .into_response();
    }

    ensure_setup_defaults(&mut req.app_config.config);

    if let Err(err) = req.app_config.config.prepare(false) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": err.to_string() }))).into_response();
    }
    if !req.app_config.config.is_valid() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "Invalid config.yml content" }))).into_response();
    }
    if let Err(err) = validate_library_paths_from_dto(&req.app_config.config) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": err.to_string() }))).into_response();
    }

    let template_file_path = PathBuf::from(resolve_template_persist_file_path(
        req.app_config.config.template_path.as_deref(),
        state.output_dir.to_string_lossy().as_ref(),
    ));
    let prepared_templates =
        match prepare_setup_validation_templates(&req.app_config, template_file_path.to_string_lossy().as_ref()) {
            Ok(templates) => templates,
            Err(err) => {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": err.to_string() }))).into_response();
            }
        };

    if let Err(err) = req.app_config.sources.prepare(
        false,
        req.app_config.config.get_hdhr_device_overview().as_ref(),
        prepared_templates.as_deref(),
    ) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": err.to_string() }))).into_response();
    }

    let mut api_proxy = req.app_config.api_proxy.clone().unwrap_or_default();
    if api_proxy.server.is_empty() {
        api_proxy.server.push(create_default_api_proxy_server());
    }
    if let Err(err) = api_proxy.prepare() {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": err.to_string() }))).into_response();
    }

    let users = match validate_web_users(&req.web_users) {
        Ok(users) => users,
        Err(err) => return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": err }))).into_response(),
    };

    let template_definition_to_persist = setup_templates_to_persist(&req.app_config);
    let mut persist_paths =
        vec![&state.config_file_path, &state.source_file_path, &state.api_proxy_file_path, &state.user_file_path];
    if template_definition_to_persist.is_some() {
        persist_paths.push(&template_file_path);
    }
    for file_path in persist_paths {
        if let Err(err) = ensure_parent_dir(file_path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(
                    json!({ "error": format!("Failed to create parent directory for {}: {err}", file_path.display()) }),
                ),
            )
                .into_response();
        }
    }

    req.app_config.sources = sanitize_sources_for_persist(req.app_config.sources.clone()).await;

    let mut user_lines: Vec<String> = Vec::with_capacity(users.len());
    for (username, password) in users {
        let password_for_hash = password;
        let hash_result = tokio::task::spawn_blocking(move || generate_password_from_input(&password_for_hash)).await;
        match hash_result {
            Ok(Ok(hash)) => user_lines.push(format!("{username}:{hash}")),
            Ok(Err(err)) => {
                let err_message = err.to_string();
                let status_code = if err_message == "Password too short min length 8" {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                return (
                    status_code,
                    axum::Json(
                        json!({ "error": format!("Failed to hash password for user '{username}': {err_message}") }),
                    ),
                )
                    .into_response();
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({ "error": format!("Failed to hash password for user '{username}': {err}") })),
                )
                    .into_response();
            }
        }
    }

    let user_file_content = format!("{}\n", user_lines.join("\n"));
    let mut pending_writes: Vec<(PathBuf, PathBuf, SetupPersistAction<'_>)> = vec![
        (
            state.config_file_path.clone(),
            create_setup_temp_path(&state.config_file_path),
            SetupPersistAction::Config(&req.app_config.config),
        ),
        (
            state.source_file_path.clone(),
            create_setup_temp_path(&state.source_file_path),
            SetupPersistAction::Sources(&req.app_config.sources),
        ),
        (
            state.api_proxy_file_path.clone(),
            create_setup_temp_path(&state.api_proxy_file_path),
            SetupPersistAction::ApiProxy(&api_proxy),
        ),
        (
            state.user_file_path.clone(),
            create_setup_temp_path(&state.user_file_path),
            SetupPersistAction::UserFile(&user_file_content),
        ),
    ];
    if let Some(template_definition) = template_definition_to_persist.as_ref() {
        pending_writes.push((
            template_file_path.clone(),
            create_setup_temp_path(&template_file_path),
            SetupPersistAction::Templates(template_definition),
        ));
    }

    for (index, (target_path, temp_path, action)) in pending_writes.iter().enumerate() {
        if let Err(err) = action.write(temp_path).await {
            for (_, cleanup_path, _) in pending_writes.iter().take(index + 1) {
                cleanup_temp_file(cleanup_path).await;
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("Failed to write {}: {err}", target_path.display()) })),
            )
                .into_response();
        }
    }

    let file_pairs: Vec<(PathBuf, PathBuf)> =
        pending_writes.into_iter().map(|(target_path, temp_path, _)| (target_path, temp_path)).collect();

    let mut snapshots: Vec<Option<Vec<u8>>> = Vec::with_capacity(file_pairs.len());
    for (target, _) in &file_pairs {
        let snapshot = match tokio::fs::read(target).await {
            Ok(content) => Some(content),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                for (_, temp_path) in &file_pairs {
                    cleanup_temp_file(temp_path).await;
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({ "error": format!("Failed to read current {}: {err}", target.display()) })),
                )
                    .into_response();
            }
        };
        snapshots.push(snapshot);
    }

    let mut committed_indices: Vec<usize> = Vec::with_capacity(file_pairs.len());
    for (index, (target, temp_path)) in file_pairs.iter().enumerate() {
        if let Err(err) = replace_file_atomic(temp_path, target).await {
            for committed_index in committed_indices.iter().rev() {
                let (committed_target, _) = &file_pairs[*committed_index];
                if let Err(restore_err) =
                    restore_file_snapshot(committed_target, snapshots[*committed_index].as_deref()).await
                {
                    error!(
                        "Setup mode: failed to rollback '{}' after write failure: {restore_err}",
                        committed_target.display()
                    );
                }
            }
            for (_, pending_temp) in file_pairs.iter().skip(index) {
                cleanup_temp_file(pending_temp).await;
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("Failed to write {}: {err}", target.display()) })),
            )
                .into_response();
        }
        committed_indices.push(index);
    }

    req.app_config.api_proxy = Some(api_proxy);
    {
        let mut draft = state.draft.write().await;
        *draft = req.app_config;
    }

    axum::Json(json!({
        "message": "Setup completed successfully. Restart the application to continue."
    }))
    .into_response()
}

fn create_cors_layer() -> tower_http::cors::CorsLayer {
    tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::OPTIONS,
            axum::http::Method::HEAD,
        ])
        .allow_headers(tower_http::cors::Any)
        .max_age(std::time::Duration::from_secs(3600))
}

fn create_compression_layer() -> tower_http::compression::CompressionLayer {
    tower_http::compression::CompressionLayer::new().br(true).deflate(true).gzip(true).zstd(true)
}

pub async fn start_setup_server(paths: &ConfigPaths, missing_files: &[String]) -> Result<(), TuliproxError> {
    let draft = build_initial_draft(paths);
    let (host, port, web_root) = setup_bind_values(&draft);
    let web_dir = resolve_setup_web_dir(&web_root)
        .ok_or_else(|| info_err!("Setup mode requires a web directory. Tried '{}'", web_root.display(),))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let state = Arc::new(SetupModeState {
        draft: Arc::new(RwLock::new(draft)),
        output_dir: PathBuf::from(&paths.config_path),
        config_file_path: PathBuf::from(&paths.config_file_path),
        source_file_path: PathBuf::from(&paths.sources_file_path),
        api_proxy_file_path: PathBuf::from(&paths.api_proxy_file_path),
        user_file_path: PathBuf::from(&paths.config_path).join(USER_FILE),
        web_dir: web_dir.clone(),
        missing_files: Arc::new(missing_files.to_vec()),
        completed: AtomicBool::new(false),
        shutdown_tx: Mutex::new(Some(shutdown_tx)),
    });

    info!("Setup mode enabled. Missing required config files: {}", missing_files.join(", "));
    info!("Setup output directory: {}", state.output_dir.display());
    info!("Setup web root: {}", state.web_dir.display());
    info!("Setup server running: http://{host}:{port}");

    let router = Router::new()
        .route("/healthcheck", axum::routing::get(setup_healthcheck))
        .nest(
            "/auth",
            Router::new()
                .route("/token", axum::routing::post(setup_token))
                .route("/refresh", axum::routing::post(setup_token_refresh)),
        )
        .nest(
            "/api/v1",
            Router::new()
                .route("/config", axum::routing::get(setup_get_config))
                .route("/config/apiproxy", axum::routing::get(setup_get_api_proxy))
                .route("/setup/complete", axum::routing::post(setup_complete)),
        )
        .route("/api/{*path}", axum::routing::get(api_not_found))
        .route("/ws", axum::routing::get(api_not_found))
        .route("/config.json", axum::routing::get(setup_config_json))
        .route("/", axum::routing::get(setup_index))
        .route("/{filename}", axum::routing::get(setup_root_file))
        .nest_service("/assets", ServeDir::new(web_dir.join("assets")))
        .nest_service("/static", ServeDir::new(web_dir.join("static")))
        .fallback(axum::routing::get(setup_index))
        .layer(create_cors_layer())
        .layer(create_compression_layer())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .map_err(|err| info_err!("Failed to bind setup server to {host}:{port}: {err}"))?;

    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
        .map_err(|err| info_err!("Setup server error: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        has_unresolved_redacted_setup_values, redact_app_config_for_setup, restore_redacted_setup_values,
        SETUP_REDACTED_SECRET_VALUE,
    };
    use shared::model::{ApiProxyConfigDto, AppConfigDto, TargetUserDto, WebAuthConfigDto, WebUiConfigDto};

    fn sample_app_config() -> AppConfigDto {
        AppConfigDto {
            config: shared::model::ConfigDto {
                web_ui: Some(WebUiConfigDto {
                    auth: Some(WebAuthConfigDto {
                        issuer: "tuliprox".to_string(),
                        secret: "very-secret-value".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            api_proxy: Some(ApiProxyConfigDto {
                user: vec![TargetUserDto {
                    target: "target-a".to_string(),
                    credentials: vec![shared::model::ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "alice-password".to_string(),
                        token: Some("alice-token".to_string()),
                        ..Default::default()
                    }],
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn setup_redaction_masks_web_auth_and_api_proxy_credentials() {
        let redacted = redact_app_config_for_setup(sample_app_config());

        let auth_secret =
            redacted.config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).map(|auth| auth.secret.as_str());
        assert_eq!(auth_secret, Some(SETUP_REDACTED_SECRET_VALUE));

        let creds = &redacted.api_proxy.expect("api_proxy should be present").user[0].credentials[0];
        assert_eq!(creds.password, SETUP_REDACTED_SECRET_VALUE);
        assert_eq!(creds.token.as_deref(), Some(SETUP_REDACTED_SECRET_VALUE));
    }

    #[test]
    fn setup_restore_replaces_redacted_values_from_existing_draft() {
        let draft = sample_app_config();
        let mut submitted = redact_app_config_for_setup(draft.clone());

        restore_redacted_setup_values(&mut submitted, &draft);

        let auth_secret =
            submitted.config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).map(|auth| auth.secret.as_str());
        assert_eq!(auth_secret, Some("very-secret-value"));

        let creds = &submitted.api_proxy.as_ref().expect("api_proxy should be present").user[0].credentials[0];
        assert_eq!(creds.password, "alice-password");
        assert_eq!(creds.token.as_deref(), Some("alice-token"));
        assert!(!has_unresolved_redacted_setup_values(&submitted));
    }

    #[test]
    fn setup_restore_keeps_unmatched_redacted_credentials_as_unresolved() {
        let draft = sample_app_config();
        let mut submitted = redact_app_config_for_setup(draft);
        if let Some(api_proxy) = submitted.api_proxy.as_mut() {
            api_proxy.user[0].credentials[0].username = "bob".to_string();
        }

        restore_redacted_setup_values(&mut submitted, &AppConfigDto::default());

        assert!(has_unresolved_redacted_setup_values(&submitted));
    }
}
