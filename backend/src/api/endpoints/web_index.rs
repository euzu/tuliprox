use crate::api::api_utils::serve_file;
use crate::api::model::app_state::AppState;
use crate::auth::{AuthBearer, verify_password, create_jwt_admin, create_jwt_user, is_admin, verify_token};
use axum::response::IntoResponse;
use log::error;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc};
use tower::Service;
use shared::model::{TokenResponse, UserCredential};
use shared::utils::CONSTANTS;

fn no_web_auth_token() -> impl axum::response::IntoResponse + Send {
    axum::Json(TokenResponse {
        token: "authorized".to_string(),
        username: "admin".to_string(),
    }).into_response()
}

async fn token(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(mut req): axum::extract::Json<UserCredential>,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config.config.load();
    match config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) {
        None => no_web_auth_token().into_response(),
        Some(web_auth) => {
            if !web_auth.enabled {
                return no_web_auth_token().into_response();
            }
            let username = req.username.as_str();
            let password = req.password.as_str();

            if !(username.is_empty() || password.is_empty()) {
                if let Some(hash) = web_auth.get_user_password(username) {
                    if verify_password(hash, password.as_bytes()) {
                        if let Ok(token) = create_jwt_admin(web_auth, username) {
                            req.zeroize();
                            return axum::Json(
                                TokenResponse {
                                    token,
                                    username: req.username.to_string(),
                                }).into_response();
                        }
                    }
                }
                if let Some(credentials) = app_state.app_config.get_user_credentials(username) {
                    if credentials.password == password {
                        if let Ok(token) = create_jwt_user(web_auth, username) {
                            req.zeroize();
                            return axum::Json(
                                TokenResponse {
                                    token,
                                    username: req.username.to_string(),
                                }).into_response();
                        }
                    }
                }
            }

            req.zeroize();
            axum::http::StatusCode::BAD_REQUEST.into_response()
        }
    }
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
            let maybe_token_data = verify_token(&token, secret_key);
            if let Some(token_data) = maybe_token_data {
                let username = token_data.claims.username.clone();
                let new_token = if is_admin(Some(token_data)) {
                    create_jwt_admin(web_auth, &username)
                } else {
                    create_jwt_user(web_auth, &username)
                };
                if let Ok(token) = new_token {
                    return axum::Json(
                        TokenResponse {
                            token,
                            username: username.to_string(),
                        }).into_response();
                }
            }
            axum::http::StatusCode::BAD_REQUEST.into_response()
        }
    }
}

async fn index(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config.config.load();
    let path: PathBuf = [&config.api.web_root, "index.html"].iter().collect();
    if let Some(web_ui_path) = &config.web_ui.as_ref().and_then(|c| c.path.as_ref()) {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let mut new_content = CONSTANTS.re_base_href.replace_all(&content, |caps: &regex::Captures| {
                    format!(r#"{}="/{web_ui_path}/{}""#, &caps[1], &caps[2])
                }).to_string();

                let base_href = format!(r#"<head><base href="/{web_ui_path}/">"#);
                if let Some(pos) = new_content.find("<head>") {
                    new_content.replace_range(pos..pos + 6, &base_href);
                }

                return axum::response::Response::builder()
                    .header("Content-Type", mime::TEXT_HTML_UTF_8.as_ref())
                    .body(new_content.into())
                    .unwrap();
            }
            Err(err) => {
                error!("Failed to read web ui index.hml: {err}");
            }
        }
    }
    serve_file(&path, mime::TEXT_HTML_UTF_8).await.into_response()
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
                                let new_url = format!("/{web_ui_path}{url}");
                                *api_url = json!(new_url);
                            }
                        }
                        if let Some(auth_url) = api.get_mut("authUrl") {
                            if let Some(url) = auth_url.as_str() {
                                let new_url = format!("/{web_ui_path}{url}");
                                *auth_url = json!(new_url);
                            }
                        }
                    }
                    if let Ok(json_content) = serde_json::to_string(&json_data) {
                        return axum::response::Response::builder()
                            .header("Content-Type", mime::APPLICATION_JSON.as_ref())
                            .body(axum::body::Body::from(json_content))
                            .unwrap();
                    }
                }
            }
            Err(err) => {
                error!("Failed to read web ui config.json: {err}");
            }
        }
    }
    serve_file(&path, mime::APPLICATION_JSON).await.into_response()
}

pub fn index_register_without_path(web_dir_path: &Path) -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .nest("/auth", axum::Router::new()
            .route("/token", axum::routing::post(token))
            .route("/refresh", axum::routing::post(token_refresh)))
        .merge(axum::Router::new()
            .route("/", axum::routing::get(index))
            .fallback(axum::routing::get_service(tower_http::services::ServeDir::new(web_dir_path))))
}

pub fn index_register_with_path(web_dir_path: &Path, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .nest(&format!("{web_ui_path}/auth"), axum::Router::new()
            .route("/token", axum::routing::post(token))
            .route("/refresh", axum::routing::post(token_refresh)))
        .merge(axum::Router::new()
            .route(&format!("{web_ui_path}/"), axum::routing::get(index))
            .route(&format!("{web_ui_path}/config.json"), axum::routing::get(index_config))
            .fallback({
                let mut serve_dir = tower_http::services::ServeDir::new(web_dir_path);
                let path_prefix = web_ui_path.to_string();
                move |req: axum::http::Request<_>| {
                    let mut path = req.uri().path().to_string();

                    if path.starts_with(&path_prefix) {
                        path = path[path_prefix.len()..].to_string();
                    }

                    let mut builder = axum::http::Uri::builder();
                    if let Some(scheme) = req.uri().scheme() {
                        builder = builder.scheme(scheme.clone());
                    }
                    if let Some(authority) = req.uri().authority() {
                        builder = builder.authority(authority.clone());
                    }
                    let new_uri = builder.path_and_query(path)
                        .build()
                        .unwrap();

                    let new_req = axum::http::Request::builder()
                        .method(req.method())
                        .uri(new_uri)
                        .body(req.into_body())
                        .unwrap();

                    serve_dir.call(new_req)
                }
            }))
}
