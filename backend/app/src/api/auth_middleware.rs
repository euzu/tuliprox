//! Axum authentication middleware.
//!
//! These handlers take `State<Arc<AppState>>` and are HTTP concerns, not
//! authentication primitives. They lived in `auth`, which made that module - and
//! everything depending on it - reach up into `api`. The JWT creation and
//! verification they call still lives in `auth::authenticator`.

use crate::{
    api::{api_utils::get_username_from_auth_header, model::AppState},
    auth::{validate_token_claims, verify_token, AuthBearer, AuthError},
};
use log::warn;
use shared::model::permission::{permission_to_name, Permission};
use std::sync::Arc;

/// Decode once, then check the role against the decoded claims.
///
/// `role_fn` used to be a `fn(&str, &[u8]) -> bool` that re-decoded the token
/// from scratch, so every authenticated request paid for two JWT decodes (three
/// on the API-user path). It is still a plain `fn` pointer - static dispatch,
/// no closure - it just reads the `Claims` this function already has.
fn validate_request(
    app_state: &Arc<AppState>,
    token: &str,
    role_fn: fn(&shared::model::Claims) -> bool,
) -> Result<(), AuthError> {
    let config = app_state.app_config.config.load();
    let Some(web_auth_config) = config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) else {
        return Err(AuthError::InvalidToken);
    };
    let secret_key = web_auth_config.secret.as_ref();
    let token_data = verify_token(token, secret_key).ok_or(AuthError::InvalidToken)?;
    validate_token_claims(&token_data.claims)?;
    if !role_fn(&token_data.claims) {
        return Err(AuthError::Forbidden);
    }
    Ok(())
}

/// Build a stable, recognizable rejection response. Refresh-required
/// cases carry the `X-Token-Refresh: required` header so the
/// frontend can branch on it. `Forbidden` (a successful authentication
/// that nonetheless cannot perform the action) maps to 403 so it does
/// not collapse into the "you're not authenticated" 401 path.
fn rejection_for(err: AuthError) -> axum::response::Response {
    use axum::http::StatusCode;
    let status = match &err {
        AuthError::Forbidden => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    };
    let mut builder = axum::http::Response::builder().status(status);
    if err.is_token_refresh_required() {
        builder = builder.header("X-Token-Refresh", "required");
    }
    builder
        .body(axum::body::Body::from(err.to_string()))
        .unwrap_or_else(|_| axum::http::Response::new(axum::body::Body::empty()))
}

pub async fn validator_admin(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match validate_request(&app_state, &token, shared::model::Claims::is_admin) {
        Ok(()) => next.run(request).await,
        Err(err) => rejection_for(err),
    }
}

pub async fn validator_api_user(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(username) = get_username_from_auth_header(&token, &app_state) {
        if let Some(user) = app_state.app_config.get_user_credentials(&username) {
            if !user.ui_enabled {
                return axum::http::Response::builder()
                    .status(axum::http::StatusCode::FORBIDDEN)
                    .body(axum::body::Body::from("principal does not have the required role".to_string()))
                    .unwrap_or_else(|_| axum::http::Response::new(axum::body::Body::empty()));
            }
        }
    }
    match validate_request(&app_state, &token, shared::model::Claims::is_api_user) {
        Ok(()) => next.run(request).await,
        Err(err) => rejection_for(err),
    }
}

pub async fn require_permission_inner(
    permission: Permission,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let config = app_state.app_config.config.load();
    let Some(web_auth_config) = config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) else {
        return rejection_for(AuthError::InvalidToken);
    };

    let Some(token_data) = verify_token(&token, web_auth_config.secret.as_bytes()) else {
        return rejection_for(AuthError::InvalidToken);
    };
    if let Err(err) = validate_token_claims(&token_data.claims) {
        return rejection_for(err);
    }

    if !token_data.claims.permissions.contains(permission) {
        let denied_permission = permission_to_name(permission).unwrap_or("unknown");
        warn!("User '{}' denied permission '{denied_permission}'", token_data.claims.username);
        return rejection_for(AuthError::Forbidden);
    }

    next.run(request).await
}

/// Builds an Axum layer that enforces one permission.
///
/// Defined here rather than in `auth` because it expands to a call into this
/// module; keeping it there meant `auth` named `api`.
#[macro_export]
macro_rules! permission_layer {
    ($app_state:expr, $permission:expr ) => {{
        let app_state = ::std::sync::Arc::clone($app_state);
        ::axum::middleware::from_fn_with_state(app_state, move |state, auth, request, next| {
            $crate::api::auth_middleware::require_permission_inner($permission, state, auth, request, next)
        })
    }};
}
pub use permission_layer;
