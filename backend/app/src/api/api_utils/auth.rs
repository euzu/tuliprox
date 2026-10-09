use crate::{
    api::model::{AppState, UserApiRequest},
    model::{AppConfig, ConfigInput, ConfigTarget, ProxyUserCredentials},
};
use shared::model::ProxyType;
use std::sync::Arc;

pub fn get_user_target_by_username(
    username: &str,
    app_state: &Arc<AppState>,
) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
    if !username.is_empty() {
        return app_state.app_config.get_target_for_username(username);
    }
    None
}

pub fn get_user_target_by_credentials<'a>(
    username: &str,
    password: &str,
    api_req: &'a UserApiRequest,
    app_state: &'a AppState,
) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
    if !username.is_empty() && !password.is_empty() {
        app_state.app_config.get_target_for_user(username, password)
    } else {
        let token = api_req.token.as_str().trim();
        if token.is_empty() {
            None
        } else {
            app_state.app_config.get_target_for_user_by_token(token)
        }
    }
}

pub fn get_user_target<'a>(
    api_req: &'a UserApiRequest,
    app_state: &'a AppState,
) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
    let username = api_req.username.as_str().trim();
    let password = api_req.password.as_str().trim();
    get_user_target_by_credentials(username, password, api_req, app_state)
}

pub fn get_username_from_auth_header(token: &str, app_state: &Arc<AppState>) -> Option<String> {
    let config = app_state.app_config.config.load();
    let web_auth_config = config.web_ui.as_ref()?.auth.as_ref()?;
    // This hand-rolled its own `decode` with a bare `Validation::new`, which
    // checks `exp` and nothing else - no issuer.
    crate::auth::verify_token(token, web_auth_config.secret.as_bytes(), &web_auth_config.issuer)
        .map(|token_data| token_data.claims.username)
}

pub fn create_api_proxy_user(app_state: &Arc<AppState>) -> ProxyUserCredentials {
    let config = app_state.app_config.config.load();

    let server = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());

    ProxyUserCredentials {
        username: "api_user".to_string(),
        password: "api_user".to_string(),
        token: None,
        proxy: ProxyType::Reverse(None),
        server: Some(server.to_string()),
        epg_timeshift: None,
        epg_request_timeshift: None,
        created_at: None,
        exp_date: None,
        max_connections: 0,
        status: None,
        output_clusters: shared::model::ClusterFlags::all(),
        ui_enabled: false,
        comment: None,
        priority: 0,
        soft_connections: 0,
        soft_priority: 0,
        t_is_api_user: true,
        network_access: None,
        plan: None,
        filter: None,
        raw_output_clusters: None,
        raw_max_connections: 0,
        raw_soft_connections: 0,
        raw_proxy: Some(ProxyType::Reverse(None)),
        t_filter: None,
        t_has_unresolved_plan: false,
        t_has_invalid_filter: false,
    }
}

/// The user internal recording playback runs as. Its server resolves to the
/// local API listener (`AppConfig::get_user_server_info`), and its HLS URLs
/// carry this identity to every follow-up request.
pub fn create_recording_proxy_user(app_state: &Arc<AppState>) -> ProxyUserCredentials {
    use base64::Engine;
    use sha2::Digest;
    let mut user = create_api_proxy_user(app_state);
    user.username = crate::model::RECORDING_PROXY_USERNAME.to_string();
    let digest = sha2::Sha256::new()
        .chain_update(b"tuliprox/internal-recording-user")
        .chain_update(app_state.app_config.access_token_secret)
        .finalize();
    user.password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..]);
    user.server = None;
    user
}

/// Recording headers configure the upstream capture. They are layered under
/// the input's own headers, which keep precedence, and above the worker's
/// request headers. The worker's `User-Agent` stays on the local request so
/// the stream remains identifiable as a recording.
pub(crate) fn with_recording_headers(app_config: &AppConfig, input: Arc<ConfigInput>) -> Arc<ConfigInput> {
    let config = app_config.config.load();
    let Some(recording) = config.recording() else { return input };
    let recording_headers = recording.origin_headers(&config);
    if recording_headers.keys().all(|name| input.headers.keys().any(|key| key.eq_ignore_ascii_case(name.as_str()))) {
        return input;
    }
    let mut merged = input.as_ref().clone();
    for (name, value) in recording_headers {
        let Ok(value) = value.to_str() else { continue };
        if !merged.headers.keys().any(|key| key.eq_ignore_ascii_case(name.as_str())) {
            merged.headers.insert(name.as_str().to_string(), value.to_string());
        }
    }
    Arc::new(merged)
}

/// The input as seen by `user`: recording playback adds the recording headers.
pub(crate) fn input_for_user(
    app_config: &AppConfig,
    user: &ProxyUserCredentials,
    input: Arc<ConfigInput>,
) -> Arc<ConfigInput> {
    if user.is_recording_proxy_user() {
        with_recording_headers(app_config, input)
    } else {
        input
    }
}
