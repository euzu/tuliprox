use crate::api::api_utils::serve_file;
use crate::api::api_utils::try_unwrap_body;
use crate::api::model::AppState;
use crate::auth::{create_jwt_admin, create_jwt_user, is_admin, verify_password, verify_token, AuthBearer};
use axum::response::IntoResponse;
use log::{error};
use serde_json::json;
use shared::model::{TokenResponse, UserCredential, TOKEN_NO_AUTH};
use shared::utils::{concat_path_leading_slash, CONSTANTS};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use axum::body::Body;
use axum::http::Request;
use base64::Engine;
//use base64::engine::general_purpose;
use openssl::rand::rand_bytes;
//use openssl::sha::{sha256};
use tower::{Service, ServiceExt};
use tower_http::services::ServeFile;
use lol_html::{element, RewriteStrSettings};

fn no_web_auth_token() -> impl axum::response::IntoResponse + Send {
    axum::Json(TokenResponse {
        token: TOKEN_NO_AUTH.to_string(),
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
                                    username: req.username.clone(),
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
                                    username: req.username.clone(),
                                }).into_response();
                        }
                    }
                }
            }

            req.zeroize();
            axum::http::StatusCode::UNAUTHORIZED.into_response()
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
                            username: username.clone(),
                        }).into_response();
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
            // 1) All <script> without nonce → add nonce
            element!("script:not([nonce])", move |el| {
                el.set_attribute("nonce", nonce_b64)?;
                Ok(())
            }),
            // 2) Remove meta CSP from HTML, if present
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
                    let mut the_content = CONSTANTS.re_base_href.replace_all(&content, |caps: &regex::Captures| {
                        format!(r#"{}="{}""#, &caps[1], concat_path_leading_slash(web_ui_path, &caps[2]))
                    }).to_string();

                    // replace wasm paths
                    the_content = CONSTANTS.re_base_href_wasm.replace_all(&the_content, |caps: &regex::Captures| {
                        format!("'{}", concat_path_leading_slash(web_ui_path, &caps[1]))
                    }).to_string();

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
            if let Err(e) = rand_bytes(&mut rnd) {
                error!("Failed to generate random bytes for nonce: {e}");
                // Fallback: without further manipulation back
                return try_unwrap_body!(axum::response::Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, mime::TEXT_HTML_UTF_8.as_ref())
                    .body(new_content));
            }
            let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(rnd);

            // let hash = sha256(&rnd);
            // let nonce_b64 = general_purpose::STANDARD_NO_PAD.encode(hash);

            // Insert calculated nonce
            // let script_tag = r#"<script type="module">"#;
            // if new_content.contains(script_tag) {
            //     let new_tag = format!(r#"<script type="module" nonce="{nonce_b64}">"#);
            //     new_content = new_content.replacen(script_tag, &new_tag, 1);
            // }

            new_content = inject_nonce_with_parser(new_content, &nonce_b64);

            let mut builder = axum::response::Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime::TEXT_HTML_UTF_8.as_ref());
            if let Some(csp) = config
                .web_ui
                .as_ref()
                .and_then(|w| w.content_security_policy.as_ref())
                .filter(|c| c.enabled)
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
                            let new_url  = concat_path_leading_slash(web_ui_path, url);
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
                            let new_url  = format!("/{web_ui_path}");
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
        .nest("/auth", axum::Router::new()
            .route("/token", axum::routing::post(token))
            .route("/refresh", axum::routing::post(token_refresh)))
        .merge(axum::Router::new()
            .route("/", axum::routing::get(index))
            .fallback(axum::routing::get_service(tower_http::services::ServeDir::new(web_dir_path))))
}

pub fn index_register_with_path(web_dir_path: &Path, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    let web_dir_path_clone = PathBuf::from(web_dir_path);
    let web_ui_router = axum::Router::new()
            .route("/", axum::routing::get(index))
            .route("/config.json", axum::routing::get(index_config))
            .route("/{filename}",  axum::routing::get(async move
                |axum::extract::Path(filename): axum::extract::Path<String>| {
                 let full_path = web_dir_path_clone.join(&filename);
                 let svc = ServeFile::new(full_path);
                 svc.oneshot(Request::new(Body::empty())).await
            }))
            .fallback({
                let mut serve_dir = tower_http::services::ServeDir::new(web_dir_path);
                let path_prefix = format!("/{web_ui_path}");
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
                        .body(req.into_body()).unwrap();

                    serve_dir.call(new_req)
                }
            });

    let auth_router = axum::Router::new()
        .route("/token", axum::routing::post(token))
        .route("/refresh", axum::routing::post(token_refresh));

    let web_ui_path_clone = web_ui_path.to_string();
    axum::Router::new()
        .nest(&concat_path_leading_slash(web_ui_path, "auth"), auth_router)
        .route(&format!("/{web_ui_path}"), axum::routing::get(|| async move {
            axum::response::Redirect::permanent(&format!("/{web_ui_path_clone}/"))
        }))
        .nest(&format!("/{web_ui_path}/"), web_ui_router)
}
