//! Axum authentication middleware.
//!
//! These handlers take `State<Arc<AppState>>` and are HTTP concerns, not
//! authentication primitives. They lived in `auth`, which made that module - and
//! everything depending on it - reach up into `api`. The JWT creation and
//! verification they call still lives in `auth::authenticator`.

use crate::{
    api::model::AppState,
    auth::{validate_password_version, validate_token_claims, verify_token, AuthBearer, AuthError},
};
use axum::{extract::FromRequestParts, http::request::Parts};
use log::warn;
use shared::model::{
    permission::{permission_to_name, Permission, PermissionSet},
    AuthAuditEvent, Claims, EventMessage, EventSink,
};
use std::sync::Arc;

/// The verified claims for the current request, parked in the request
/// extensions by whichever layer authenticated it.
///
/// A handler behind a permission layer used to have no way to reach the claims
/// the layer had just decoded, so it decoded the token again - a fourth time on
/// some paths. [`AuthorizedClaims`] reads this instead.
#[derive(Debug, Clone)]
pub struct VerifiedClaims(pub Claims);

/// Verify a bearer token and everything that must hold for its principal.
///
/// One decode, then: signature, expiry and issuer (inside `verify_token`),
/// schema version and subject id, and finally the principal's password
/// version. That last check is why a password change now invalidates live
/// tokens instead of leaving them valid until expiry.
fn authenticate(app_state: &Arc<AppState>, token: &str) -> Result<Claims, AuthError> {
    let config = app_state.app_config.config.load();
    let Some(web_auth_config) = config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) else {
        return Err(AuthError::InvalidToken);
    };
    let token_data = verify_token(token, web_auth_config.secret.as_bytes(), &web_auth_config.issuer)
        .ok_or(AuthError::InvalidToken)?;
    let claims = token_data.claims;
    validate_token_claims(&claims)?;

    // Only web users have a password on file here. A proxy API user
    // authenticates against `api_proxy.yml` and carries no password version, so
    // there is nothing to compare and nothing to enforce.
    if let Some(current) = web_auth_config.pwd_version_for(&claims.username) {
        validate_password_version(&claims, current)?;
    }

    Ok(claims)
}

/// The permissions that actually apply to this request.
///
/// The token's permission set is a snapshot from the moment it was minted.
/// Reading it alone meant revoking a group permission had no effect until the
/// token expired. The effective set is the intersection of the token's claim
/// with what the live config grants right now: a revocation takes effect
/// immediately, while a *new* grant still requires a refresh, because a token
/// must never end up with more authority than it was issued with.
///
/// A principal the web-auth config has never heard of - a proxy API user -
/// has no live set to intersect with, and keeps its claim.
fn effective_permissions(app_state: &Arc<AppState>, claims: &Claims) -> PermissionSet {
    let config = app_state.app_config.config.load();
    config
        .web_ui
        .as_ref()
        .and_then(|c| c.auth.as_ref())
        .and_then(|web_auth| web_auth.resolve_permissions_if_known(&claims.username))
        .map_or(claims.permissions, |live| claims.permissions & live)
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
    if status == StatusCode::UNAUTHORIZED {
        // A 401 without a challenge is not a well-formed 401.
        builder = builder.header(axum::http::header::WWW_AUTHENTICATE, "Bearer");
    }
    if err.is_token_refresh_required() {
        builder = builder.header("X-Token-Refresh", "required");
    }
    builder
        .body(axum::body::Body::from(err.to_string()))
        .unwrap_or_else(|_| axum::http::Response::new(axum::body::Body::empty()))
}

/// Authenticate, check the role, and park the claims for the handler.
///
/// `role_fn` is a plain `fn` pointer - static dispatch, no closure. It reads
/// the claims this function already decoded; it used to be a
/// `fn(&str, &[u8]) -> bool` that decoded the token all over again.
fn authorize_role(
    app_state: &Arc<AppState>,
    token: &str,
    request: &mut axum::extract::Request,
    role_fn: fn(&Claims) -> bool,
) -> Result<(), AuthError> {
    let claims = authenticate(app_state, token)?;
    if !role_fn(&claims) {
        return Err(AuthError::Forbidden);
    }
    request.extensions_mut().insert(VerifiedClaims(claims));
    Ok(())
}

pub async fn validator_admin(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match authorize_role(&app_state, &token, &mut request, Claims::is_admin) {
        Ok(()) => next.run(request).await,
        Err(err) => rejection_for(err),
    }
}

pub async fn validator_api_user(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let claims = match authenticate(&app_state, &token) {
        Ok(claims) => claims,
        Err(err) => return rejection_for(err),
    };
    // The UI gate reads the username off the claims we already decoded. It used
    // to re-decode the token through `get_username_from_auth_header` purely to
    // learn the name, and it ran *before* the token was validated at all.
    if let Some(user) = app_state.app_config.get_user_credentials(&claims.username) {
        if !user.ui_enabled {
            return rejection_for(AuthError::Forbidden);
        }
    }
    if !claims.is_api_user() {
        return rejection_for(AuthError::Forbidden);
    }
    request.extensions_mut().insert(VerifiedClaims(claims));
    next.run(request).await
}

/// Enforce one permission, named as a const generic.
///
/// `P` is a [`Permission`] discriminant. The permission is therefore fixed at
/// monomorphisation rather than captured in a closure and read at runtime, so
/// each route's check folds to a constant mask.
pub async fn require_permission<const P: u32>(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let claims = match authenticate(&app_state, &token) {
        Ok(claims) => claims,
        Err(err) => return rejection_for(err),
    };
    let client_ip = audit_client_ip(request.headers());
    if let Err(err) = check_permission::<P>(&app_state, &claims, client_ip.as_deref()) {
        return rejection_for(err);
    }
    request.extensions_mut().insert(VerifiedClaims(claims));
    next.run(request).await
}

fn check_permission<const P: u32>(
    app_state: &Arc<AppState>,
    claims: &Claims,
    client_ip: Option<&str>,
) -> Result<(), AuthError> {
    // `P` is a discriminant, not a bit. Recovering the variant is a constant
    // fold at each monomorphisation; `None` cannot happen for a `P` the
    // `permission_layer!` macro produced.
    let Some(permission) = Permission::from_repr(P) else {
        return Err(AuthError::Forbidden);
    };
    if !effective_permissions(app_state, claims).contains(permission) {
        let denied_permission = permission_to_name(permission).unwrap_or("unknown");
        warn!("User '{}' denied permission '{denied_permission}'", claims.username);
        // A denial went to `warn!` and nowhere else, so nothing that
        // subscribes to the bus could see it.
        app_state.event_manager.emit(EventMessage::AuthAudit(AuthAuditEvent::permission_denied(
            Arc::from(claims.username.as_str()),
            Arc::from(client_ip.unwrap_or("unknown")),
            Arc::from(denied_permission),
        )));
        return Err(AuthError::Forbidden);
    }
    Ok(())
}

/// The client address for an audit record, best-effort.
///
/// The forwarding headers are read directly rather than through the
/// `Fingerprint` extractor: an audit record is not worth failing a request
/// over when the peer address is unavailable.
fn audit_client_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-real-ip")
        .or_else(|| headers.get("x-forwarded-for"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(|value| value.trim().to_string())
}

/// A handler-signature permission requirement.
///
/// `async fn handler(AuthorizedClaims::<{ Permission::ConfigRead as u32 }>(claims), ...)`
/// states the requirement in the type and hands the handler the verified
/// claims, so it never has to decode the token itself. Behind a
/// `permission_layer!` this is free - the layer already parked the claims in
/// the request extensions - and it authenticates on its own where there is no
/// layer.
pub struct AuthorizedClaims<const P: u32>(pub Claims);

/// Why [`AuthorizedClaims`] refused the request.
///
/// A typed rejection rather than a pre-rendered `Response`: both variants are
/// `Copy` and two words wide, so the extractor's `Result` stays small and the
/// caller can still match on the reason.
#[derive(Debug, Clone, Copy)]
pub enum AuthorizeRejection {
    /// The `Authorization` header itself was missing or malformed.
    Header(crate::auth::AuthRejection),
    /// The header was fine; the token or the principal was not.
    Token(AuthError),
}

impl axum::response::IntoResponse for AuthorizeRejection {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Header(rejection) => rejection.into_response(),
            Self::Token(err) => rejection_for(err),
        }
    }
}

impl<const P: u32> FromRequestParts<Arc<AppState>> for AuthorizedClaims<P> {
    type Rejection = AuthorizeRejection;

    // The whole check is synchronous once the request parts are in hand, so
    // this is a ready future rather than an `async fn` - the same shape the
    // `AuthBearer` and `AuthBasic` extractors use.
    fn from_request_parts(
        parts: &mut Parts,
        app_state: &Arc<AppState>,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Self::extract(parts, app_state))
    }
}

impl<const P: u32> AuthorizedClaims<P> {
    fn extract(parts: &mut Parts, app_state: &Arc<AppState>) -> Result<Self, AuthorizeRejection> {
        let claims = if let Some(VerifiedClaims(claims)) = parts.extensions.get::<VerifiedClaims>() {
            claims.clone()
        } else {
            let AuthBearer(token) = AuthBearer::from_headers(&parts.headers).map_err(AuthorizeRejection::Header)?;
            let claims = authenticate(app_state, &token).map_err(AuthorizeRejection::Token)?;
            parts.extensions.insert(VerifiedClaims(claims.clone()));
            claims
        };
        let client_ip = audit_client_ip(&parts.headers);
        check_permission::<P>(app_state, &claims, client_ip.as_deref()).map_err(AuthorizeRejection::Token)?;
        Ok(Self(claims))
    }
}

/// Builds an Axum layer that enforces one permission.
///
/// Defined here rather than in `auth` because it expands to a call into this
/// module; keeping it there meant `auth` named `api`.
///
/// The permission crosses into [`require_permission`] as a const generic, so
/// the layer is a bare `fn` item rather than a closure holding a runtime value.
#[macro_export]
macro_rules! permission_layer {
    ($app_state:expr, $permission:expr ) => {{
        let app_state = ::std::sync::Arc::clone($app_state);
        ::axum::middleware::from_fn_with_state(
            app_state,
            $crate::api::auth_middleware::require_permission::<{ $permission as u32 }>,
        )
    }};
}
pub use permission_layer;
