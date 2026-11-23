//! Axum authentication middleware.
//!
//! These handlers take `State<Arc<AppState>>` and are HTTP concerns, not
//! authentication primitives. They lived in `auth`, which made that module - and
//! everything depending on it - reach up into `api`. The JWT creation and
//! verification they call still lives in `auth::authenticator`.

use crate::{
    api::model::AppState,
    auth::{validate_password_version, validate_token_claims, verify_token, AuthBearer, AuthError, Fingerprint},
    model::{ConfigTarget, ProxyUserCredentials},
};
use axum::{extract::FromRequestParts, http::request::Parts, response::IntoResponse};
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
pub(crate) async fn authenticate(app_state: &Arc<AppState>, token: &str) -> Result<Claims, AuthError> {
    let claims = verify_claims(app_state, token)?;
    // Last, because it is the only check that touches shared state: a token
    // that fails any of the cheap checks above never reaches it.
    if app_state.auth.token_revocations.is_revoked(&claims).await {
        return Err(AuthError::Revoked);
    }
    Ok(claims)
}

/// Everything about a token that can be decided from the token and the config
/// alone.
fn verify_claims(app_state: &Arc<AppState>, token: &str) -> Result<Claims, AuthError> {
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
    if !claims.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
        if let Some(current) = web_auth_config.pwd_version_for(&claims.username) {
            validate_password_version(&claims, current)?;
        }
    }

    Ok(claims)
}

/// The permissions that actually apply to this request.
///
/// For web users, the token's permission set is a snapshot from the moment it was
/// minted. The effective set is the intersection of the token's claim with what
/// the live config grants right now: a revocation takes effect immediately, while
/// a *new* grant still requires a refresh, because a token must never end up with
/// more authority than it was issued with.
///
/// For proxy API users (`subject_id` is an API subject), permissions are computed
/// live from the current config on every request via `api_user_permissions`.
/// Tokens minted for API users carry no permissions; their effective permissions
/// reflect their current plan and user `player_features` immediately upon config
/// update, allowing instant activation and revocation without re-minting tokens.
/// If the user is unknown or the subject does not match, an empty set is returned.
pub(crate) fn effective_permissions(app_state: &Arc<AppState>, claims: &Claims) -> PermissionSet {
    if claims.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
        if let Ok(user) = validate_api_user_account(app_state, claims, ApiUserAccess::Any) {
            if !user.permission_denied(&app_state.app_config) {
                return tuliprox_core::model::api_user_permissions(&user.player_features);
            }
        }
        return PermissionSet::new();
    }

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
pub(crate) fn rejection_for(err: AuthError) -> axum::response::Response {
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
async fn authorize_role(
    app_state: &Arc<AppState>,
    token: &str,
    request: &mut axum::extract::Request,
    role_fn: fn(&Claims) -> bool,
) -> Result<(), AuthError> {
    let claims = authenticate(app_state, token).await?;
    if !role_fn(&claims) {
        return Err(AuthError::Forbidden);
    }
    request.extensions_mut().insert(VerifiedClaims(claims));
    Ok(())
}

/// Decides what class of access an API user is being validated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiUserAccess {
    WebUi,
    PlayerApi,
    Any,
}

/// Decides whether an API user is permitted for the given access class.
#[must_use]
pub(crate) fn can_api_user_access(
    web_ui: Option<&tuliprox_core::model::WebUiConfig>,
    user: &ProxyUserCredentials,
    access: ApiUserAccess,
) -> bool {
    let Some(web_ui) = web_ui else {
        return false;
    };
    let web_ui_allowed = user.ui_enabled && web_ui.user_ui_enabled;
    let player_api_allowed = user.player_api_enabled() && web_ui.player_api_enabled();
    match access {
        ApiUserAccess::WebUi => web_ui_allowed,
        ApiUserAccess::PlayerApi => player_api_allowed,
        ApiUserAccess::Any => web_ui_allowed || player_api_allowed,
    }
}

/// Decides whether the Player API is reachable for this user under the current configuration.
///
/// Requires the Web UI to be enabled, Web authentication to be active, and the user to be permitted for the Player API.
#[must_use]
pub(crate) fn player_api_reachable(config: &tuliprox_core::model::Config, user: &ProxyUserCredentials) -> bool {
    let web_ui_enabled = config.web_ui.as_ref().is_some_and(|c| c.enabled);
    web_ui_enabled
        && crate::api::main_api::is_web_auth_enabled(config, web_ui_enabled)
        && can_api_user_access(config.web_ui.as_ref(), user, ApiUserAccess::PlayerApi)
}

/// Returns the Player API base path prefix when the Player API is reachable for this user, or `None`.
///
/// Uses `/` when `web_ui.path` is empty or unset, or `/<web_ui.path>`. Returns `None` unless every
/// character of the final path is in `[A-Za-z0-9._~/-]`.
#[must_use]
pub(crate) fn player_api_path(config: &tuliprox_core::model::Config, user: &ProxyUserCredentials) -> Option<String> {
    if !player_api_reachable(config, user) {
        return None;
    }
    let raw_path = config.web_ui.as_ref().and_then(|w| w.path.as_deref()).unwrap_or("");
    let path = shared::utils::concat_path_leading_slash(raw_path, "");
    let final_path = if path.is_empty() { String::from("/") } else { path };
    if final_path.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'/' | b'-')) {
        Some(final_path)
    } else {
        None
    }
}

/// Validates that an authenticated API user is known, active, and permitted by configuration.
pub(crate) fn validate_api_user_account(
    app_state: &AppState,
    claims: &Claims,
    access: ApiUserAccess,
) -> Result<Arc<ProxyUserCredentials>, AuthError> {
    let subject = claims.subject_id.as_ref().ok_or(AuthError::InvalidToken)?;
    if !subject.is_api() {
        return Err(AuthError::Forbidden);
    }
    let user = app_state.app_config.get_user_credentials(&claims.username).ok_or(AuthError::InvalidToken)?;
    let config = app_state.app_config.config.load();
    if !can_api_user_access(config.web_ui.as_ref(), &user, access) {
        return Err(AuthError::Forbidden);
    }
    if *subject != shared::model::UserId::api(&user.username) {
        return Err(AuthError::InvalidToken);
    }
    Ok(user)
}

/// Validates that an authenticated API user is known, active, permitted by configuration, and allowed by network access.
pub(crate) fn validate_api_user(
    app_state: &AppState,
    claims: &Claims,
    access: ApiUserAccess,
    fingerprint: &Fingerprint,
) -> Result<Arc<ProxyUserCredentials>, AuthError> {
    let user = validate_api_user_account(app_state, claims, access)?;
    app_state.try_check_network_access(&user, fingerprint).map_err(|_| AuthError::Forbidden)?;
    Ok(user)
}

/// [`validate_api_user`] plus an active account and the user's resolved target, as the player routes require.
pub(crate) fn validate_api_user_target(
    app_state: &AppState,
    claims: &Claims,
    fingerprint: &Fingerprint,
) -> Result<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>), AuthError> {
    let user = validate_api_user(app_state, claims, ApiUserAccess::PlayerApi, fingerprint)?;
    if user.permission_denied(&app_state.app_config) {
        return Err(AuthError::Forbidden);
    }
    let (_, target) =
        app_state.app_config.get_target_for_user(&user.username, &user.password).ok_or(AuthError::InvalidToken)?;
    Ok((user, target))
}

pub(crate) fn settings_owner(
    app_state: &AppState,
    claims: Option<&Claims>,
    access: ApiUserAccess,
    fingerprint: &Fingerprint,
) -> Result<tuliprox_repository::user_settings_repository::SettingsOwner, AuthError> {
    use tuliprox_repository::user_settings_repository::SettingsOwner;
    let config = app_state.app_config.config.load();
    let Some(auth) = config.web_ui.as_ref().and_then(|ui| ui.auth.as_ref()).filter(|auth| auth.enabled) else {
        return Ok(SettingsOwner::local());
    };
    let claims = claims.ok_or(AuthError::InvalidToken)?;
    let subject = claims.subject_id.as_ref().ok_or(AuthError::InvalidToken)?;
    let username = if subject.is_api() {
        validate_api_user(app_state, claims, access, fingerprint)?.username.clone()
    } else {
        let user = auth
            .t_users
            .as_ref()
            .and_then(|users| users.iter().find(|user| user.username.eq_ignore_ascii_case(&claims.username)))
            .ok_or(AuthError::InvalidToken)?;
        let admin = user.groups.iter().any(|group| group.eq_ignore_ascii_case("admin"));
        if subject.is_builtin_admin() {
            if !admin {
                return Err(AuthError::Forbidden);
            }
        } else if *subject != shared::model::UserId::web(&user.username) {
            return Err(AuthError::InvalidToken);
        }
        user.username.clone()
    };
    SettingsOwner::authenticated(subject, &username).map_err(|_| AuthError::InvalidToken)
}

/// Request context for authenticated proxy API users.
#[derive(Debug, Clone)]
pub(crate) struct ApiUserContext {
    pub claims: Claims,
    pub user: Arc<ProxyUserCredentials>,
    pub target: Arc<ConfigTarget>,
}

/// Resolves an `ApiUserContext` from authenticated claims, validating the user and target and populating effective permissions.
pub(crate) fn api_user_context(
    app_state: &AppState,
    mut claims: Claims,
    fingerprint: &Fingerprint,
) -> Result<ApiUserContext, AuthError> {
    let (user, target) = validate_api_user_target(app_state, &claims, fingerprint)?;
    claims.permissions = tuliprox_core::model::api_user_permissions(&user.player_features);
    Ok(ApiUserContext { claims, user, target })
}

impl FromRequestParts<Arc<AppState>> for ApiUserContext {
    type Rejection = axum::response::Response;

    async fn from_request_parts(parts: &mut Parts, app_state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let fingerprint = Fingerprint::from_request_parts(parts, app_state)
            .await
            .map_err(axum::response::IntoResponse::into_response)?;

        let claims = if let Some(VerifiedClaims(claims)) = parts.extensions.remove::<VerifiedClaims>() {
            claims
        } else {
            let AuthBearer(token) =
                AuthBearer::from_headers(&parts.headers).map_err(|_| rejection_for(AuthError::InvalidToken))?;
            authenticate(app_state, &token).await.map_err(rejection_for)?
        };

        let ctx = api_user_context(app_state, claims, &fingerprint).map_err(rejection_for)?;
        parts.extensions.insert(VerifiedClaims(ctx.claims.clone()));
        Ok(ctx)
    }
}

pub async fn validator_authenticated(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let enabled = app_state
        .app_config
        .config
        .load()
        .web_ui
        .as_ref()
        .and_then(|ui| ui.auth.as_ref())
        .is_some_and(|auth| auth.enabled);
    if !enabled {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let fingerprint = match Fingerprint::from_request_parts(&mut parts, &app_state).await {
        Ok(fp) => fp,
        Err(err) => return err.into_response(),
    };
    let Ok(AuthBearer(token)) = AuthBearer::from_headers(&parts.headers) else {
        return rejection_for(AuthError::InvalidToken);
    };
    let claims = match authenticate(&app_state, &token).await {
        Ok(claims) => claims,
        Err(err) => return rejection_for(err),
    };
    if let Err(err) = settings_owner(&app_state, Some(&claims), ApiUserAccess::Any, &fingerprint) {
        return rejection_for(err);
    }
    parts.extensions.insert(VerifiedClaims(claims));
    parts.extensions.insert(fingerprint);
    let request = axum::extract::Request::from_parts(parts, body);
    next.run(request).await
}

pub async fn validator_admin(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match authorize_role(&app_state, &token, &mut request, Claims::is_admin).await {
        Ok(()) => next.run(request).await,
        Err(err) => rejection_for(err),
    }
}

pub async fn validator_api_user(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (mut parts, body) = request.into_parts();
    let fingerprint = match Fingerprint::from_request_parts(&mut parts, &app_state).await {
        Ok(fp) => fp,
        Err(err) => return err.into_response(),
    };
    let Ok(AuthBearer(token)) = AuthBearer::from_headers(&parts.headers) else {
        return rejection_for(AuthError::InvalidToken);
    };
    let claims = match authenticate(&app_state, &token).await {
        Ok(claims) => claims,
        Err(err) => return rejection_for(err),
    };
    if let Err(err) = validate_api_user(&app_state, &claims, ApiUserAccess::WebUi, &fingerprint) {
        return rejection_for(err);
    }
    parts.extensions.insert(VerifiedClaims(claims));
    parts.extensions.insert(fingerprint);
    let request = axum::extract::Request::from_parts(parts, body);
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
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let claims = match authenticate(&app_state, &token).await {
        Ok(claims) => claims,
        Err(err) => return rejection_for(err),
    };
    let (mut parts, body) = request.into_parts();
    if claims.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
        let fingerprint = match Fingerprint::from_request_parts(&mut parts, &app_state).await {
            Ok(fp) => fp,
            Err(err) => return err.into_response(),
        };
        if let Err(err) = validate_api_user(&app_state, &claims, ApiUserAccess::Any, &fingerprint) {
            return rejection_for(err);
        }
        parts.extensions.insert(fingerprint);
    }
    let client_ip = audit_client_ip(&parts.headers);
    if let Err(err) = check_permission::<P>(&app_state, &claims, client_ip.as_deref()) {
        return rejection_for(err);
    }
    parts.extensions.insert(VerifiedClaims(claims));
    let request = axum::extract::Request::from_parts(parts, body);
    next.run(request).await
}

pub(crate) fn check_permission<const P: u32>(
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

    async fn from_request_parts(parts: &mut Parts, app_state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let claims = if let Some(VerifiedClaims(claims)) = parts.extensions.get::<VerifiedClaims>() {
            // A layer already authenticated this request, revocation included.
            claims.clone()
        } else {
            let AuthBearer(token) = AuthBearer::from_headers(&parts.headers).map_err(AuthorizeRejection::Header)?;
            let claims = authenticate(app_state, &token).await.map_err(AuthorizeRejection::Token)?;
            parts.extensions.insert(VerifiedClaims(claims.clone()));
            claims
        };
        if claims.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
            let fingerprint = match Fingerprint::from_request_parts(parts, app_state).await {
                Ok(fp) => fp,
                Err(err) => return Err(AuthorizeRejection::Header(err)),
            };
            validate_api_user(app_state, &claims, ApiUserAccess::Any, &fingerprint)
                .map_err(AuthorizeRejection::Token)?;
            parts.extensions.insert(fingerprint);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::create_test_app_state;
    use shared::model::{
        permission::Permission, ApiProxyConfigDto, Claims, PlayerFeaturesDto, PlayerRecordingLevel,
        ProxyUserCredentialsDto, RoleSet, TargetUserDto, UserId, WebAuthConfigDto, WebUiConfigDto,
        CURRENT_PERMISSION_SCHEMA_VERSION,
    };
    use std::sync::Arc;
    use tuliprox_core::model::{ApiProxyConfig, Config};

    /// Parts built straight from a `Request` carry no peer address, which the
    /// fingerprint extractor rejects. Give those parts a loopback peer.
    fn parts_with_peer(req: axum::http::Request<()>) -> axum::http::request::Parts {
        let (mut parts, ()) = req.into_parts();
        parts.extensions.insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))));
        parts
    }

    fn configure_api_user_target(app_state: &AppState, username: &str, recording: Option<PlayerRecordingLevel>) {
        let target = Arc::new(ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = crate::model::SourcesConfig {
            inputs: Vec::new(),
            sources: vec![crate::model::ConfigSource { inputs: Vec::new(), targets: vec![target] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let api_proxy_dto = ApiProxyConfigDto {
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![ProxyUserCredentialsDto {
                    username: username.to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    player_features: recording
                        .map(|rec| PlayerFeaturesDto { recording: Some(rec), ..Default::default() }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let mut api_proxy = ApiProxyConfig::from(&api_proxy_dto);
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).unwrap();
    }

    fn setup_api_user_state(recording: Option<PlayerRecordingLevel>) -> (Arc<AppState>, Claims) {
        let config = Config {
            web_ui: Some(
                WebUiConfigDto {
                    enabled: true,
                    user_ui_enabled: true,
                    auth: Some(WebAuthConfigDto {
                        enabled: true,
                        secret: "0123456789abcdef0123456789abcdef".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
                .into(),
            ),
            ..Default::default()
        };
        let app_state = create_test_app_state(config);
        configure_api_user_target(&app_state, "alice", recording);

        let claims = Claims {
            username: "alice".to_string(),
            roles: RoleSet::API_USER,
            permissions: PermissionSet::new(),
            exp: i64::MAX,
            iss: "tuliprox".to_string(),
            iat: 0,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
            subject_id: Some(UserId::api("alice")),
            pwd_version: 0,
        };

        (app_state, claims)
    }

    fn update_api_user_recording(app_state: &AppState, recording: Option<PlayerRecordingLevel>) {
        configure_api_user_target(app_state, "alice", recording);
    }

    #[tokio::test]
    async fn test_effective_permissions_api_user_activation_and_upgrade() {
        let (app_state, claims) = setup_api_user_state(Some(PlayerRecordingLevel::None));
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());

        update_api_user_recording(&app_state, Some(PlayerRecordingLevel::Private));
        let private_expected = Permission::RecordingRead
            | Permission::RecordingCreate
            | Permission::RecordingManage
            | Permission::RecordingDelete;
        assert_eq!(effective_permissions(&app_state, &claims), private_expected);

        update_api_user_recording(&app_state, Some(PlayerRecordingLevel::Shared));
        let shared_expected = private_expected | Permission::RecordingShare;
        assert_eq!(effective_permissions(&app_state, &claims), shared_expected);
    }

    #[tokio::test]
    async fn test_effective_permissions_api_user_revocation() {
        let (app_state, claims) = setup_api_user_state(Some(PlayerRecordingLevel::Shared));
        let shared_expected = Permission::RecordingRead
            | Permission::RecordingCreate
            | Permission::RecordingManage
            | Permission::RecordingDelete
            | Permission::RecordingShare;
        assert_eq!(effective_permissions(&app_state, &claims), shared_expected);

        update_api_user_recording(&app_state, Some(PlayerRecordingLevel::Read));
        assert_eq!(effective_permissions(&app_state, &claims), Permission::RecordingRead.into());

        update_api_user_recording(&app_state, Some(PlayerRecordingLevel::None));
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());
    }

    #[tokio::test]
    async fn test_effective_permissions_api_user_denied_when_account_status_invalid() {
        use shared::model::ProxyUserStatus;

        let (app_state, claims) = setup_api_user_state(Some(PlayerRecordingLevel::Shared));
        {
            let mut cfg = (*app_state.app_config.config.load_full()).clone();
            cfg.user_access_control = true;
            app_state.app_config.config.store(Arc::new(cfg));
        }
        let shared_expected = Permission::RecordingRead
            | Permission::RecordingCreate
            | Permission::RecordingManage
            | Permission::RecordingDelete
            | Permission::RecordingShare;
        assert_eq!(effective_permissions(&app_state, &claims), shared_expected);

        // 1. Disabled status -> empty permissions
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            Arc::make_mut(&mut api_proxy.user[0].credentials[0]).status = Some(ProxyUserStatus::Disabled);
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());

        // 2. Expired user -> empty permissions
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            let cred = Arc::make_mut(&mut api_proxy.user[0].credentials[0]);
            cred.status = Some(ProxyUserStatus::Active);
            cred.exp_date = Some(1); // epoch timestamp 1 is far in the past
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());

        // 3. Unresolved plan -> empty permissions
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            let cred = Arc::make_mut(&mut api_proxy.user[0].credentials[0]);
            cred.exp_date = None;
            cred.plan = Some("unknown_plan".to_string());
            cred.t_has_unresolved_plan = true;
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());

        // 4. UI disabled on credentials -> empty permissions
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            let cred = Arc::make_mut(&mut api_proxy.user[0].credentials[0]);
            cred.plan = None;
            cred.t_has_unresolved_plan = false;
            cred.ui_enabled = false;
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        assert_eq!(
            effective_permissions(&app_state, &claims),
            PermissionSet::new(),
            "ui_enabled: false revokes DVR permissions"
        );

        // 5. Global user_ui_enabled false -> empty permissions
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            let cred = Arc::make_mut(&mut api_proxy.user[0].credentials[0]);
            cred.ui_enabled = true;
            app_state.app_config.set_api_proxy(api_proxy).unwrap();

            let mut cfg = (*app_state.app_config.config.load_full()).clone();
            if let Some(ref mut web_ui) = cfg.web_ui {
                web_ui.user_ui_enabled = false;
            }
            app_state.app_config.config.store(Arc::new(cfg));
        }
        assert_eq!(
            effective_permissions(&app_state, &claims),
            PermissionSet::new(),
            "user_ui_enabled: false revokes DVR permissions"
        );
    }

    #[tokio::test]
    async fn test_effective_permissions_api_user_unknown_user_empty() {
        let app_state = create_test_app_state(Config::default());
        let claims = Claims {
            username: "unknown".to_string(),
            roles: RoleSet::API_USER,
            permissions: PermissionSet::new(),
            exp: i64::MAX,
            iss: "tuliprox".to_string(),
            iat: 0,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
            subject_id: Some(UserId::api("unknown")),
            pwd_version: 0,
        };
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());
    }

    #[tokio::test]
    async fn test_effective_permissions_api_user_subject_mismatch_empty() {
        let app_state = create_test_app_state(Config::default());
        configure_api_user_target(&app_state, "alice", Some(PlayerRecordingLevel::Shared));

        // username is "alice", but subject is "bob"
        let claims = Claims {
            username: "alice".to_string(),
            roles: RoleSet::API_USER,
            permissions: PermissionSet::new(),
            exp: i64::MAX,
            iss: "tuliprox".to_string(),
            iat: 0,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
            subject_id: Some(UserId::api("bob")),
            pwd_version: 0,
        };
        assert_eq!(effective_permissions(&app_state, &claims), PermissionSet::new());
    }

    #[tokio::test]
    async fn test_effective_permissions_web_user_unchanged() {
        let config = Config {
            web_ui: Some(
                WebUiConfigDto {
                    auth: Some(WebAuthConfigDto { enabled: true, secret: "secret".to_string(), ..Default::default() }),
                    ..Default::default()
                }
                .into(),
            ),
            ..Default::default()
        };
        let app_state = create_test_app_state(config);

        // Claims minted with ConfigRead | ConfigWrite
        let claims = Claims {
            username: "admin".to_string(),
            roles: RoleSet::ADMIN,
            permissions: Permission::ConfigRead | Permission::ConfigWrite,
            exp: i64::MAX,
            iss: "tuliprox".to_string(),
            iat: 0,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
            subject_id: Some(UserId::web("admin")),
            pwd_version: 1,
        };

        assert_eq!(effective_permissions(&app_state, &claims), Permission::ConfigRead | Permission::ConfigWrite);
    }

    #[tokio::test]
    async fn test_api_user_context_extractor() {
        use crate::auth::{create_jwt_api_user, create_jwt_web_user};
        use axum::http::{Request, StatusCode};

        let (app_state, _claims) = setup_api_user_state(Some(PlayerRecordingLevel::Shared));
        let mut cfg = (**app_state.app_config.config.load()).clone();
        if let Some(ref mut web_ui) = cfg.web_ui {
            web_ui.user_ui_enabled = true;
        }
        app_state.app_config.set_config(cfg).unwrap();

        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();

        // 1. Web-user token -> 403
        let web_token =
            create_jwt_web_user(&web_auth, "admin", Permission::ConfigRead.into(), 0, UserId::web("admin")).unwrap();

        let req = Request::builder().header("Authorization", format!("Bearer {web_token}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let res = ApiUserContext::from_request_parts(&mut parts, &app_state).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::FORBIDDEN);

        // 2. API token with ui_enabled: false -> 403
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            Arc::make_mut(&mut api_proxy.user[0].credentials[0]).ui_enabled = false;
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        let api_token_alice = create_jwt_api_user(&web_auth, "alice", UserId::api("alice")).unwrap();
        let req = Request::builder().header("Authorization", format!("Bearer {api_token_alice}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let res = ApiUserContext::from_request_parts(&mut parts, &app_state).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::FORBIDDEN);

        // Enable ui_enabled for alice
        let target = Arc::new(ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = crate::model::SourcesConfig {
            inputs: Vec::new(),
            sources: vec![crate::model::ConfigSource { inputs: Vec::new(), targets: vec![target] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let api_proxy_dto = ApiProxyConfigDto {
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    player_features: Some(PlayerFeaturesDto {
                        recording: Some(PlayerRecordingLevel::Shared),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let mut api_proxy = ApiProxyConfig::from(&api_proxy_dto);
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).unwrap();

        // 3. Revoked token -> 401
        let revoked_token = create_jwt_api_user(&web_auth, "bob", UserId::api("bob")).unwrap();
        app_state.auth.token_revocations.revoke_subject(&UserId::api("bob"), i64::MAX).await.unwrap();

        let req = Request::builder().header("Authorization", format!("Bearer {revoked_token}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let res = ApiUserContext::from_request_parts(&mut parts, &app_state).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::UNAUTHORIZED);

        // 4. OK -> handler sees effective permissions
        let ok_token = create_jwt_api_user(&web_auth, "alice", UserId::api("alice")).unwrap();
        let req = Request::builder().header("Authorization", format!("Bearer {ok_token}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let ctx = ApiUserContext::from_request_parts(&mut parts, &app_state).await.expect("ok context");
        assert_eq!(ctx.user.username, "alice");
        assert_eq!(ctx.target.name, "main");
        let expected_perms = Permission::RecordingRead
            | Permission::RecordingCreate
            | Permission::RecordingManage
            | Permission::RecordingDelete
            | Permission::RecordingShare;
        assert_eq!(ctx.claims.permissions, expected_perms);
    }

    #[test]
    fn test_can_api_user_access_matrix() {
        use crate::api::auth_middleware::{can_api_user_access, ApiUserAccess};
        use tuliprox_core::model::{PlayerFeatures, ProxyUserCredentials, WebUiConfig};

        let make_cfg = |user_ui_enabled: bool, player_api_enabled: Option<bool>| WebUiConfig {
            enabled: true,
            user_ui_enabled,
            player_api_enabled,
            content_security_policy: None,
            path: None,
            auth: None,
            player_server: None,
            kick_secs: 60,
            combine_views_stats_streams: false,
            landing_page: None,
            stream_info: None,
        };

        let make_user = |ui_enabled: bool, api_enabled: Option<bool>| {
            let mut u = ProxyUserCredentials::default();
            u.ui_enabled = ui_enabled;
            u.player_features = PlayerFeatures { api_enabled, ..Default::default() };
            u
        };

        // None config -> always false
        let user = make_user(true, Some(true));
        assert!(!can_api_user_access(None, &user, ApiUserAccess::WebUi));
        assert!(!can_api_user_access(None, &user, ApiUserAccess::PlayerApi));
        assert!(!can_api_user_access(None, &user, ApiUserAccess::Any));

        // WebUi class
        assert!(can_api_user_access(Some(&make_cfg(true, None)), &make_user(true, None), ApiUserAccess::WebUi));
        assert!(!can_api_user_access(Some(&make_cfg(true, None)), &make_user(false, None), ApiUserAccess::WebUi));
        assert!(!can_api_user_access(Some(&make_cfg(false, None)), &make_user(true, None), ApiUserAccess::WebUi));
        assert!(!can_api_user_access(
            Some(&make_cfg(true, Some(true))),
            &make_user(false, Some(true)),
            ApiUserAccess::WebUi
        ));
        assert!(!can_api_user_access(
            Some(&make_cfg(false, Some(true))),
            &make_user(true, Some(true)),
            ApiUserAccess::WebUi
        ));

        // PlayerApi class: Defaults (unset follows ui_enabled / user_ui_enabled)
        assert!(can_api_user_access(Some(&make_cfg(true, None)), &make_user(true, None), ApiUserAccess::PlayerApi));
        assert!(!can_api_user_access(Some(&make_cfg(true, None)), &make_user(false, None), ApiUserAccess::PlayerApi));
        assert!(!can_api_user_access(Some(&make_cfg(false, None)), &make_user(true, None), ApiUserAccess::PlayerApi));

        // PlayerApi class: Overrides
        assert!(can_api_user_access(
            Some(&make_cfg(false, Some(true))),
            &make_user(false, Some(true)),
            ApiUserAccess::PlayerApi
        ));
        assert!(!can_api_user_access(
            Some(&make_cfg(true, Some(true))),
            &make_user(true, Some(false)),
            ApiUserAccess::PlayerApi
        ));
        assert!(!can_api_user_access(
            Some(&make_cfg(true, Some(false))),
            &make_user(true, Some(true)),
            ApiUserAccess::PlayerApi
        ));

        // Any class: WebUi || PlayerApi
        assert!(can_api_user_access(
            Some(&make_cfg(true, Some(false))),
            &make_user(true, Some(false)),
            ApiUserAccess::Any
        ));
        assert!(can_api_user_access(
            Some(&make_cfg(false, Some(true))),
            &make_user(false, Some(true)),
            ApiUserAccess::Any
        ));
        assert!(!can_api_user_access(
            Some(&make_cfg(false, Some(false))),
            &make_user(false, Some(false)),
            ApiUserAccess::Any
        ));
        assert!(can_api_user_access(
            Some(&make_cfg(true, Some(true))),
            &make_user(true, Some(true)),
            ApiUserAccess::Any
        ));
    }

    #[tokio::test]
    async fn test_api_user_context_player_only_and_forbidden_cases() {
        use crate::auth::create_jwt_api_user;
        use axum::http::{Request, StatusCode};

        let (app_state, _) = setup_api_user_state(Some(PlayerRecordingLevel::Shared));
        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();

        let target = Arc::new(ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = crate::model::SourcesConfig {
            inputs: Vec::new(),
            sources: vec![crate::model::ConfigSource { inputs: Vec::new(), targets: vec![target] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let update_user = |ui_enabled: bool, api_enabled: Option<bool>| {
            let api_proxy_dto = ApiProxyConfigDto {
                user: vec![TargetUserDto {
                    target: "main".to_string(),
                    credentials: vec![ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled,
                        player_features: Some(PlayerFeaturesDto {
                            recording: Some(PlayerRecordingLevel::Shared),
                            api_enabled,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                }],
                ..Default::default()
            };
            let mut api_proxy = ApiProxyConfig::from(&api_proxy_dto);
            for tu in &mut api_proxy.user {
                for cred in &mut tu.credentials {
                    Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
                }
            }
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        };

        let update_config = |user_ui_enabled: bool, player_api_enabled: Option<bool>| {
            let mut cfg = (**app_state.app_config.config.load()).clone();
            if let Some(ref mut web_ui) = cfg.web_ui {
                web_ui.user_ui_enabled = user_ui_enabled;
                web_ui.player_api_enabled = player_api_enabled;
            }
            app_state.app_config.set_config(cfg).unwrap();
        };

        let token = create_jwt_api_user(&web_auth, "alice", UserId::api("alice")).unwrap();

        // Case 1: Player-only user accepted (ui_enabled: false, api_enabled: Some(true))
        update_user(false, Some(true));
        update_config(false, Some(true));
        let req = Request::builder().header("Authorization", format!("Bearer {token}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let ctx = ApiUserContext::from_request_parts(&mut parts, &app_state)
            .await
            .expect("player-only user must be accepted");
        assert_eq!(ctx.user.username, "alice");

        // Case 2: Refused (403) with api_enabled: Some(false) while ui_enabled: true
        update_user(true, Some(false));
        update_config(true, Some(true));
        let req = Request::builder().header("Authorization", format!("Bearer {token}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let res = ApiUserContext::from_request_parts(&mut parts, &app_state).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::FORBIDDEN);

        // Case 3: Refused (403) with global player_api_enabled: Some(false)
        update_user(true, Some(true));
        update_config(true, Some(false));
        let req = Request::builder().header("Authorization", format!("Bearer {token}")).body(()).unwrap();
        let mut parts = parts_with_peer(req);
        let res = ApiUserContext::from_request_parts(&mut parts, &app_state).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn validator_api_user_rejects_unknown_user_with_401() {
        use crate::auth::create_jwt_api_user;
        use axum::{
            http::{Request, StatusCode},
            middleware::from_fn_with_state,
            routing::get,
            Router,
        };
        use tower::ServiceExt;

        let (app_state, _) = setup_api_user_state(None);
        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
        let unknown_token = create_jwt_api_user(&web_auth, "unknown_user", UserId::api("unknown_user")).unwrap();

        let app = Router::new()
            .route("/test", get(|| async { StatusCode::OK }))
            .route_layer(from_fn_with_state(Arc::clone(&app_state), validator_api_user))
            .layer(axum::extract::connect_info::MockConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))));

        let req = Request::builder()
            .uri("/test")
            .header("Authorization", format!("Bearer {unknown_token}"))
            .body(axum::body::Body::empty())
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_validator_api_user_enforces_network_access() {
        use crate::auth::create_jwt_api_user;
        use axum::{
            http::{Request, StatusCode},
            middleware::from_fn_with_state,
            routing::get,
            Router,
        };
        use shared::model::NetworkAccessDto;
        use tower::ServiceExt;

        let (app_state, _) = setup_api_user_state(None);
        // Add network_access for alice restricting to 10.0.0.0/8
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            Arc::make_mut(&mut api_proxy.user[0].credentials[0]).network_access =
                Some(tuliprox_core::model::NetworkAccess::from(&NetworkAccessDto {
                    allowed_countries: None,
                    allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
                }));
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
        let alice_token = create_jwt_api_user(&web_auth, "alice", UserId::api("alice")).unwrap();

        let app = Router::new()
            .route("/test", get(|| async { StatusCode::OK }))
            .route_layer(from_fn_with_state(Arc::clone(&app_state), validator_api_user));

        // Denied IP (192.168.1.1): 403
        let mut req_denied = Request::builder()
            .uri("/test")
            .header("Authorization", format!("Bearer {alice_token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        req_denied
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 1], 12345))));
        let resp_denied = app.clone().oneshot(req_denied).await.unwrap();
        assert_eq!(resp_denied.status(), StatusCode::FORBIDDEN);

        // Allowed IP (10.0.0.1): 200
        let mut req_allowed = Request::builder()
            .uri("/test")
            .header("Authorization", format!("Bearer {alice_token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        req_allowed
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([10, 0, 0, 1], 12345))));
        let resp_allowed = app.oneshot(req_allowed).await.unwrap();
        assert_eq!(resp_allowed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_require_permission_api_user_enforces_network_access() {
        use crate::auth::create_jwt_api_user;
        use axum::{
            http::{Request, StatusCode},
            routing::get,
            Router,
        };
        use shared::model::NetworkAccessDto;
        use tower::ServiceExt;

        let (app_state, _) = setup_api_user_state(Some(PlayerRecordingLevel::Shared));
        // Add network_access for alice restricting to 10.0.0.0/8
        {
            let mut api_proxy = (*app_state.app_config.api_proxy.load_full().unwrap()).clone();
            Arc::make_mut(&mut api_proxy.user[0].credentials[0]).network_access =
                Some(tuliprox_core::model::NetworkAccess::from(&NetworkAccessDto {
                    allowed_countries: None,
                    allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
                }));
            app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
        let alice_token = create_jwt_api_user(&web_auth, "alice", UserId::api("alice")).unwrap();

        let app = Router::new()
            .route("/perm_test", get(|| async { StatusCode::OK }))
            .layer(permission_layer!(&app_state, Permission::RecordingRead));

        // Denied IP (192.168.1.1): 403
        let mut req_denied = Request::builder()
            .uri("/perm_test")
            .header("Authorization", format!("Bearer {alice_token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        req_denied
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 1], 12345))));
        let resp_denied = app.clone().oneshot(req_denied).await.unwrap();
        assert_eq!(resp_denied.status(), StatusCode::FORBIDDEN);

        // Allowed IP (10.0.0.1): 200
        let mut req_allowed = Request::builder()
            .uri("/perm_test")
            .header("Authorization", format!("Bearer {alice_token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        req_allowed
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([10, 0, 0, 1], 12345))));
        let resp_allowed = app.oneshot(req_allowed).await.unwrap();
        assert_eq!(resp_allowed.status(), StatusCode::OK);
    }

    #[test]
    fn test_player_api_path() {
        let mut user = ProxyUserCredentials::default();
        user.ui_enabled = true;
        user.player_features.api_enabled = Some(true);

        let web_ui_dto = WebUiConfigDto {
            enabled: true,
            user_ui_enabled: true,
            player_api_enabled: Some(true),
            auth: Some(WebAuthConfigDto { enabled: true, ..Default::default() }),
            path: Some("ui".to_string()),
            ..Default::default()
        };
        let mut config =
            Config { web_ui: Some(tuliprox_core::model::WebUiConfig::from(&web_ui_dto)), ..Default::default() };

        // "ui" -> Some("/ui")
        assert_eq!(player_api_path(&config, &user), Some("/ui".to_string()));

        // empty -> Some("/")
        config.web_ui.as_mut().unwrap().path = Some(String::new());
        assert_eq!(player_api_path(&config, &user), Some("/".to_string()));

        config.web_ui.as_mut().unwrap().path = None;
        assert_eq!(player_api_path(&config, &user), Some("/".to_string()));

        // ui"x -> None
        config.web_ui.as_mut().unwrap().path = Some("ui\"x".to_string());
        assert_eq!(player_api_path(&config, &user), None);

        // ui\nx -> None
        config.web_ui.as_mut().unwrap().path = Some("ui\nx".to_string());
        assert_eq!(player_api_path(&config, &user), None);

        // ui x -> None
        config.web_ui.as_mut().unwrap().path = Some("ui x".to_string());
        assert_eq!(player_api_path(&config, &user), None);
    }
}
