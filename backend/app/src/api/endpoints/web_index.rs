use crate::{
    api::auth_middleware::AuthorizedClaims,
    api::{
        api_utils::{serve_file, try_unwrap_body},
        model::AppState,
    },
    auth::{
        create_jwt_admin, create_jwt_api_user, create_jwt_web_user, validate_password_version, verify_password,
        verify_token, AuthBearer,
    },
    model::WebAuthConfig,
};
use axum::{body::Body, http::Request, response::IntoResponse};
use base64::Engine;
use log::{debug, error, warn};
use lol_html::{element, RewriteStrSettings};
use rand::{rngs::OsRng, RngCore, TryRngCore};
use serde_json::json;
use shared::{
    model::{AuthAuditEvent, EventMessage, EventSink, TokenResponse, UserCredential, TOKEN_NO_AUTH},
    utils::sanitize_sensitive_info,
    utils::{concat_path_leading_slash, CONSTANTS},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tower::{Service, ServiceExt};
use tower_http::services::ServeFile;

fn no_web_auth_token() -> impl axum::response::IntoResponse + Send {
    axum::Json(TokenResponse { token: TOKEN_NO_AUTH.to_string(), username: "admin".to_string() }).into_response()
}

fn api_user_can_access_web_ui(ui_enabled: bool) -> bool {
    ui_enabled
}

/// The stable subject id for a web user, allocating one on first sight.
///
/// `register` is get-or-create, so a user bootstrap already synced keeps the
/// id it has and one added since gets a fresh one. This used to be
/// `format!("web:{username}")`, which made the subject a function of the
/// display name: renaming a user reassigned everything the old subject owned.
async fn web_subject_id(app_state: &Arc<AppState>, username: &str) -> Option<shared::model::UserId> {
    match app_state.identity_registry.register(username).await {
        Ok(id) => Some(id),
        Err(err) => {
            error!("Cannot resolve a stable subject id for web user '{username}': {err}");
            None
        }
    }
}

/// The stable subject id for a proxy API user, allocating one on first sight.
async fn api_subject_id(app_state: &Arc<AppState>, username: &str) -> Option<shared::model::UserId> {
    match app_state.identity_registry.register_api_user(username).await {
        Ok(id) => Some(id),
        Err(err) => {
            error!("Cannot resolve a stable subject id for API user '{username}': {err}");
            None
        }
    }
}

/// Publish one authentication decision.
///
/// Sign-ins and their failures went to `warn!` and nowhere else, so nothing
/// that subscribes to the bus - a notification channel, a plugin, an audit
/// sink - could see the events that matter most for spotting an intrusion.
fn emit_auth_audit(app_state: &Arc<AppState>, event: AuthAuditEvent) {
    app_state.event_manager.emit(EventMessage::AuthAudit(event));
}

/// A 429 carrying the wait, so a client backs off instead of hammering.
fn too_many_attempts(retry_after: std::time::Duration) -> axum::response::Response {
    let seconds = retry_after.as_secs().max(1);
    axum::http::Response::builder()
        .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
        .header(axum::http::header::RETRY_AFTER, seconds.to_string())
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| axum::http::StatusCode::TOO_MANY_REQUESTS.into_response())
}

/// The outcome of one sign-in branch.
///
/// `Rejected` means "these are not valid credentials for this branch, try the
/// next one"; `Refused` is a decided answer the caller must return as-is,
/// which is what keeps a `ui_enabled: false` API user from silently falling
/// through to the generic 401.
enum SignInAttempt {
    Issued(String),
    Refused(axum::response::Response),
    Rejected,
}

/// Sign in against `web_ui.auth`.
async fn web_user_sign_in(
    app_state: &Arc<AppState>,
    web_auth: &WebAuthConfig,
    username: &str,
    password: &str,
) -> SignInAttempt {
    let Some(hash) = web_auth.get_user_password(username) else {
        return SignInAttempt::Rejected;
    };
    if !verify_password(hash, password.as_bytes()) {
        return SignInAttempt::Rejected;
    }

    let pwd_version = WebAuthConfig::pwd_version_from_hash(hash);
    let permissions = web_auth.resolve_permissions(username);
    let user_entry = web_auth
        .t_users
        .as_ref()
        .and_then(|users| users.iter().find(|user| user.username.eq_ignore_ascii_case(username)));
    let is_admin = user_entry.is_some_and(|user| user.groups.iter().any(|group| group.eq_ignore_ascii_case("admin")));
    let user_groups = user_entry.map(|user| user.groups.clone()).unwrap_or_default();
    debug!(
        "Web login success candidate: username='{username}', groups={user_groups:?}, is_admin={is_admin}, permissions={permissions}",
    );

    let token_result = if is_admin {
        create_jwt_admin(web_auth, username, pwd_version)
    } else {
        let Some(subject_id) = web_subject_id(app_state, username).await else {
            return SignInAttempt::Refused(axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response());
        };
        create_jwt_web_user(web_auth, username, permissions, pwd_version, subject_id)
    };
    token_result.map_or(SignInAttempt::Rejected, SignInAttempt::Issued)
}

/// Sign in against the proxy API-user credentials.
async fn api_user_sign_in(
    app_state: &Arc<AppState>,
    web_auth: &WebAuthConfig,
    username: &str,
    password: &str,
) -> SignInAttempt {
    let Some(credentials) = app_state.app_config.get_user_credentials(username) else {
        return SignInAttempt::Rejected;
    };
    if !crate::auth::constant_time_eq(credentials.password.as_bytes(), password.as_bytes()) {
        return SignInAttempt::Rejected;
    }
    if !api_user_can_access_web_ui(credentials.ui_enabled) {
        // A decided answer, not a rejection: the credentials were correct and
        // this principal is still not allowed into the Web UI.
        return SignInAttempt::Refused(axum::http::StatusCode::FORBIDDEN.into_response());
    }
    let Some(subject_id) = api_subject_id(app_state, username).await else {
        return SignInAttempt::Refused(axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response());
    };
    create_jwt_api_user(web_auth, username, subject_id).map_or(SignInAttempt::Rejected, SignInAttempt::Issued)
}

async fn token(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    fingerprint: crate::auth::Fingerprint,
    axum::extract::Json(mut req): axum::extract::Json<UserCredential>,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config.config.load();
    let Some(web_auth) = config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) else {
        return no_web_auth_token().into_response();
    };
    if !web_auth.enabled {
        return no_web_auth_token().into_response();
    }

    let client_ip: Arc<str> = Arc::from(fingerprint.client_ip.as_str());
    let username = req.username.clone();

    // Before the argon2 verify, not after: the point is to stop a password
    // list, and an attacker who can still force the hash on every attempt has
    // not been slowed down.
    if let Some(retry_after) = app_state.login_throttle.retry_after(&username, &fingerprint.client_ip) {
        warn!(
            "Sign-in throttled for '{}' from {}; {}s remaining",
            sanitize_sensitive_info(&username),
            fingerprint.client_ip,
            retry_after.as_secs()
        );
        emit_auth_audit(&app_state, AuthAuditEvent::sign_in_throttled(Arc::from(username.as_str()), client_ip));
        req.zeroize();
        return too_many_attempts(retry_after);
    }

    if !(username.is_empty() || req.password.is_empty()) {
        // Sequential and short-circuiting: the API branch must not run once
        // the web branch has answered. It compares credentials and can
        // allocate a persisted subject id, neither of which a successful web
        // sign-in should trigger.
        let mut attempt = web_user_sign_in(&app_state, web_auth, &username, &req.password).await;
        if matches!(attempt, SignInAttempt::Rejected) {
            attempt = api_user_sign_in(&app_state, web_auth, &username, &req.password).await;
        }
        match attempt {
            SignInAttempt::Issued(token) => {
                app_state.login_throttle.record_success(&username, &fingerprint.client_ip);
                emit_auth_audit(&app_state, AuthAuditEvent::sign_in_succeeded(Arc::from(username.as_str()), client_ip));
                req.zeroize();
                return axum::Json(TokenResponse { token, username }).into_response();
            }
            SignInAttempt::Refused(response) => {
                req.zeroize();
                return response;
            }
            SignInAttempt::Rejected => {}
        }
    }

    app_state.login_throttle.record_failure(&username, &fingerprint.client_ip);
    emit_auth_audit(&app_state, AuthAuditEvent::sign_in_failed(Arc::from(username.as_str()), client_ip));
    warn!("Sign-in rejected for '{}' from {}", sanitize_sensitive_info(&username), fingerprint.client_ip);
    req.zeroize();
    axum::http::StatusCode::UNAUTHORIZED.into_response()
}

/// The permission required to end somebody else's sessions.
const REVOKE_PERMISSION: u32 = shared::model::permission::Permission::UserWrite as u32;

/// Revoke every token already issued to one principal.
///
/// The requirement is in the signature rather than a router layer, because the
/// `/auth` routes are mounted before any state is available to build one.
async fn revoke_user_tokens(
    AuthorizedClaims(actor): AuthorizedClaims<REVOKE_PERMISSION>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse + Send {
    // Both namespaces: an operator names a principal, not a namespace, and a
    // username can exist in either.
    let subjects: Vec<_> = [
        app_state.identity_registry.lookup_by_username(&username).await,
        app_state.identity_registry.lookup_api_by_username(&username).await,
    ]
    .into_iter()
    .flatten()
    .collect();

    if subjects.is_empty() {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    let now = chrono::Utc::now().timestamp();
    for subject in &subjects {
        if let Err(err) = app_state.token_revocations.revoke_subject(subject, now).await {
            error!("Cannot persist token revocation for '{username}': {err}");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    warn!("'{}' revoked every token issued to '{username}'", actor.username);
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// Revoke every token issued to every principal.
///
/// The response to a compromise. Short of rotating the signing secret this was
/// not possible at all, and rotating the secret is not reversible or auditable.
async fn revoke_all_tokens(
    AuthorizedClaims(actor): AuthorizedClaims<REVOKE_PERMISSION>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let now = chrono::Utc::now().timestamp();
    if let Err(err) = app_state.token_revocations.revoke_all(now).await {
        error!("Cannot persist a global token revocation: {err}");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    warn!("'{}' revoked every token issued to every principal", actor.username);
    axum::http::StatusCode::NO_CONTENT.into_response()
}

async fn token_refresh(
    AuthBearer(token): AuthBearer,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config.config.load();
    match &config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) {
        None => no_web_auth_token().into_response(),
        Some(web_auth) => {
            if !web_auth.enabled {
                return no_web_auth_token().into_response();
            }
            let secret_key = web_auth.secret.as_ref();
            let maybe_token_data = verify_token(&token, secret_key, &web_auth.issuer);
            if let Some(token_data) = maybe_token_data {
                let claims = token_data.claims;
                // A revoked token must not be exchangeable for a fresh one -
                // that would make revocation a formality.
                if app_state.token_revocations.is_revoked(&claims).await {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                let username = claims.username.as_str();

                if claims.is_api_user() {
                    let Some(user) = app_state.app_config.get_user_credentials(username) else {
                        return axum::http::StatusCode::UNAUTHORIZED.into_response();
                    };
                    if !user.ui_enabled {
                        return axum::http::StatusCode::FORBIDDEN.into_response();
                    }
                    let Some(subject_id) = api_subject_id(&app_state, username).await else {
                        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    if let Ok(token) = create_jwt_api_user(web_auth, username, subject_id) {
                        return axum::Json(TokenResponse { token, username: claims.username }).into_response();
                    }
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }

                let Some(users) = web_auth.t_users.as_ref() else {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                };
                let Some(user) = users.iter().find(|candidate| candidate.username.eq_ignore_ascii_case(username))
                else {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                };

                let current_pwd_version = WebAuthConfig::pwd_version_from_hash(&user.password_hash);
                // This used to read `pwd_version != 0 && pwd_version != current`,
                // so a token carrying `0` skipped the check entirely.
                if validate_password_version(&claims, current_pwd_version).is_err() {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }

                let is_admin = user.groups.iter().any(|group| group.eq_ignore_ascii_case("admin"));
                let resolved_permissions = web_auth.resolve_permissions(username);
                debug!(
                    "Web token refresh: username='{}', groups={:?}, is_admin={}, permissions={}",
                    username, user.groups, is_admin, resolved_permissions
                );
                let new_token = if is_admin {
                    create_jwt_admin(web_auth, username, current_pwd_version)
                } else {
                    let Some(subject_id) = web_subject_id(&app_state, username).await else {
                        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    create_jwt_web_user(web_auth, username, resolved_permissions, current_pwd_version, subject_id)
                };
                if let Ok(token) = new_token {
                    return axum::Json(TokenResponse { token, username: user.username.clone() }).into_response();
                }
            }
            axum::http::StatusCode::UNAUTHORIZED.into_response()
        }
    }
}

/// Adds `nonce` to all <script> tags that do not yet have one.
/// Also removes any existing <meta http-equiv="Content-Security-Policy"> tags.
fn inject_nonce_with_parser(html: String, nonce_b64: &str) -> String {
    let settings = RewriteStrSettings {
        element_content_handlers: vec![
            // 1) All <script> without nonce -> add nonce
            element!("script:not([nonce])", move |el| {
                el.set_attribute("nonce", nonce_b64)?;
                Ok(())
            }),
            // 2) All <style> without nonce -> add nonce
            element!("style:not([nonce])", move |el| {
                el.set_attribute("nonce", nonce_b64)?;
                Ok(())
            }),
            // 3) Remove meta CSP from HTML, if present
            element!("meta[http-equiv='Content-Security-Policy']", |el| {
                el.remove();
                Ok(())
            }),
        ],
        ..RewriteStrSettings::default()
    };

    lol_html::rewrite_str(&html, settings).unwrap_or(html)
}

async fn index(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config.config.load();
    let path: PathBuf = [&config.api.web_root, "index.html"].iter().collect();
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let mut new_content = {
                if let Some(web_ui_path) = &config.web_ui.as_ref().and_then(|c| c.path.as_ref()) {
                    // modify all url or src attributes in the html file
                    let mut the_content = CONSTANTS
                        .re_base_href
                        .replace_all(&content, |caps: &regex::Captures| {
                            format!(r#"{}="{}""#, &caps[1], concat_path_leading_slash(web_ui_path, &caps[2]))
                        })
                        .to_string();

                    // replace wasm paths
                    the_content = CONSTANTS
                        .re_base_href_wasm
                        .replace_all(&the_content, |caps: &regex::Captures| {
                            format!("'{}", concat_path_leading_slash(web_ui_path, &caps[1]))
                        })
                        .to_string();

                    let new_base = format!(r#"<base href="/{web_ui_path}/">"#);

                    if let Some(base_href_match) = CONSTANTS.re_base_href_tag.find(&the_content) {
                        let abs_start = base_href_match.start();
                        let abs_end = base_href_match.end();
                        the_content.replace_range(abs_start..abs_end, &new_base);
                    } else {
                        // replace base_href tag
                        let base_href = format!("<head>{new_base}");
                        if let Some(pos) = the_content.find("<head>") {
                            the_content.replace_range(pos..pos + 6, &base_href);
                        }
                    }
                    the_content
                } else {
                    content
                }
            };

            // ContentSecurityPolicy nonce
            let mut rnd = [0u8; 32];
            if OsRng.try_fill_bytes(&mut rnd).is_err() {
                rand::rng().fill_bytes(&mut rnd);
            }
            let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(rnd);

            new_content = inject_nonce_with_parser(new_content, &nonce_b64);

            let mut builder = axum::response::Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime::TEXT_HTML_UTF_8.as_ref());
            if let Some(csp) =
                config.web_ui.as_ref().and_then(|w| w.content_security_policy.as_ref()).filter(|c| c.enabled)
            {
                let mut attrs = vec![
                    "default-src 'self'".to_string(),
                    format!("script-src 'self' 'wasm-unsafe-eval' 'nonce-{nonce_b64}'"),
                    "frame-ancestors 'none'".to_string(),
                ];

                if let Some(custom) = &csp.custom_attributes {
                    attrs.extend(custom.clone());
                }

                for attr in &mut attrs {
                    *attr = attr.replace("{nonce_b64}", &nonce_b64);
                }
                builder = builder.header("Content-Security-Policy", attrs.join("; "));
            }
            return try_unwrap_body!(builder.body(new_content));
        }
        Err(err) => {
            error!("Failed to read web ui index.html: {err}");
        }
    }
    serve_file(&path, mime::TEXT_HTML_UTF_8.to_string(), None).await.into_response()
}

async fn index_config(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config.config.load();
    let path: PathBuf = [&config.api.web_root, "config.json"].iter().collect();
    if let Some(web_ui_path) = &config.web_ui.as_ref().and_then(|c| c.path.as_ref()) {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                if let Ok(mut json_data) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(api) = json_data.get_mut("api") {
                        if let Some(api_url) = api.get_mut("apiUrl") {
                            if let Some(url) = api_url.as_str() {
                                let new_url = concat_path_leading_slash(web_ui_path, url);
                                *api_url = json!(new_url);
                            }
                        }
                        if let Some(auth_url) = api.get_mut("authUrl") {
                            if let Some(url) = auth_url.as_str() {
                                let new_url = concat_path_leading_slash(web_ui_path, url);
                                *auth_url = json!(new_url);
                            }
                        }
                    }
                    if let Some(app_logo) = json_data.get_mut("appLogo") {
                        if let Some(url) = app_logo.as_str() {
                            let new_url = concat_path_leading_slash(web_ui_path, url);
                            *app_logo = json!(new_url);
                        }
                    }
                    if let Some(ws_url) = json_data.get_mut("wsUrl") {
                        if let Some(url) = ws_url.as_str() {
                            let new_url = concat_path_leading_slash(web_ui_path, url);
                            *ws_url = json!(new_url);
                        }
                    }

                    if let Some(web_path) = json_data.get_mut("webPath") {
                        if let Some(_path) = web_path.as_str() {
                            let new_url = format!("/{web_ui_path}");
                            *web_path = json!(new_url);
                        }
                    } else {
                        json_data["webPath"] = json!(format!("/{web_ui_path}"));
                    }

                    if let Ok(json_content) = serde_json::to_string(&json_data) {
                        return try_unwrap_body!(axum::response::Response::builder()
                            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref())
                            .body(axum::body::Body::from(json_content)));
                    }
                }
            }
            Err(err) => {
                error!("Failed to read web ui config.json: {err}");
            }
        }
    }
    serve_file(&path, mime::APPLICATION_JSON.to_string(), None).await.into_response()
}

pub fn index_register_without_path(web_dir_path: &Path) -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .nest(
            "/auth",
            axum::Router::new()
                .route("/token", axum::routing::post(token))
                .route("/refresh", axum::routing::post(token_refresh))
                .route("/revoke/{username}", axum::routing::post(revoke_user_tokens))
                .route("/revoke", axum::routing::post(revoke_all_tokens)),
        )
        .merge(
            axum::Router::new()
                .route("/", axum::routing::get(index))
                .fallback(axum::routing::get_service(tower_http::services::ServeDir::new(web_dir_path))),
        )
}

pub fn index_register_with_path(web_dir_path: &Path, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    let web_dir_path_clone = PathBuf::from(web_dir_path);
    let web_ui_router = axum::Router::new()
        .route("/", axum::routing::get(index))
        .route("/config.json", axum::routing::get(index_config))
        .route(
            "/{filename}",
            axum::routing::get(async move |axum::extract::Path(filename): axum::extract::Path<String>| {
                let full_path = web_dir_path_clone.join(&filename);
                let svc = ServeFile::new(full_path);
                svc.oneshot(Request::new(Body::empty())).await
            }),
        )
        .fallback({
            let mut serve_dir = tower_http::services::ServeDir::new(web_dir_path);
            let path_prefix = format!("/{web_ui_path}");
            move |req: axum::http::Request<_>| {
                let mut path = req.uri().path().to_string();

                if path.starts_with(&path_prefix) {
                    path = path[path_prefix.len()..].to_string();
                }
                if path.is_empty() {
                    path = "/".to_string();
                }

                let mut builder = axum::http::Uri::builder();
                if let Some(scheme) = req.uri().scheme() {
                    builder = builder.scheme(scheme.clone());
                }
                if let Some(authority) = req.uri().authority() {
                    builder = builder.authority(authority.clone());
                }
                // A malformed rewritten path must not panic the connection task; serve the original request instead
                let new_req = match builder.path_and_query(path).build() {
                    Ok(new_uri) => {
                        match axum::http::Request::builder().method(req.method()).uri(new_uri).body(req.into_body()) {
                            Ok(new_req) => new_req,
                            Err(err) => {
                                log::warn!("Failed to rebuild web ui fallback request: {err}");
                                return serve_dir.call(axum::http::Request::new(axum::body::Body::empty()));
                            }
                        }
                    }
                    Err(err) => {
                        log::warn!("Failed to rebuild web ui fallback uri: {err}");
                        return serve_dir.call(axum::http::Request::new(axum::body::Body::empty()));
                    }
                };

                serve_dir.call(new_req)
            }
        });

    let auth_router = axum::Router::new()
        .route("/token", axum::routing::post(token))
        .route("/refresh", axum::routing::post(token_refresh))
        .route("/revoke/{username}", axum::routing::post(revoke_user_tokens))
        .route("/revoke", axum::routing::post(revoke_all_tokens));

    let web_ui_path_clone = web_ui_path.to_string();
    axum::Router::new()
        .nest(&concat_path_leading_slash(web_ui_path, "auth"), auth_router)
        .route(
            &format!("/{web_ui_path}"),
            axum::routing::get(
                || async move { axum::response::Redirect::permanent(&format!("/{web_ui_path_clone}/")) },
            ),
        )
        .nest(&format!("/{web_ui_path}/"), web_ui_router)
}

#[cfg(test)]
mod tests {
    use super::api_user_can_access_web_ui;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
        routing::{get, post},
        Router,
    };
    use shared::utils::concat_path_leading_slash;
    use tower::ServiceExt;

    #[test]
    fn rejects_api_user_when_ui_is_disabled() {
        assert!(!api_user_can_access_web_ui(false));
    }

    #[test]
    fn allows_api_user_when_ui_is_enabled() {
        assert!(api_user_can_access_web_ui(true));
    }

    #[tokio::test]
    async fn auth_route_remains_reachable_when_web_ui_uses_sub_path() {
        let web_ui_path = "tuli";
        let auth_router = Router::new().route("/token", post(|| async { StatusCode::OK }));
        let web_ui_router = Router::new().fallback(get(|| async { StatusCode::NOT_FOUND }));
        let router = Router::new()
            .nest(&concat_path_leading_slash(web_ui_path, "auth"), auth_router)
            .nest(&format!("/{web_ui_path}/"), web_ui_router);

        let response = router
            .oneshot(Request::builder().method(Method::POST).uri("/tuli/auth/token").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
