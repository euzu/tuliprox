use std::sync::Arc;
use chrono::{Local, Duration};
use jsonwebtoken::{Algorithm, DecodingKey, encode, decode, EncodingKey, Header, Validation, TokenData};
use log::warn;
use crate::api::api_utils::get_username_from_auth_header;
use crate::model::WebAuthConfig;
use crate::api::model::AppState;
use crate::auth::AuthBearer;
use shared::error::to_io_error;
use shared::model::permission::{permission_to_name, Permission, PermissionSet, PERM_ALL};
use shared::model::{Claims, ROLE_ADMIN, ROLE_API_USER, UserId, CURRENT_PERMISSION_SCHEMA_VERSION};

pub fn create_jwt_admin(web_auth_config: &WebAuthConfig, username: &str, pwd_version: u32) -> Result<String, std::io::Error> {
    create_jwt(
        web_auth_config,
        username,
        vec![ROLE_ADMIN.to_string()],
        PERM_ALL,
        pwd_version,
        Some(UserId::builtin_admin()),
    )
}

pub fn create_jwt_api_user(web_auth_config: &WebAuthConfig, username: &str) -> Result<String, std::io::Error> {
    create_jwt(
        web_auth_config,
        username,
        vec![ROLE_API_USER.to_string()],
        PermissionSet::new(),
        0,
        // The identity registry will eventually provide the API
        // user's stable `UserId`. Until then, the username-as-id
        // fallback is the only stable choice.
        Some(UserId::from(format!("api:{username}"))),
    )
}

pub fn create_jwt_web_user(
    web_auth_config: &WebAuthConfig,
    username: &str,
    permissions: PermissionSet,
    pwd_version: u32,
) -> Result<String, std::io::Error> {
    create_jwt(
        web_auth_config,
        username,
        Vec::new(),
        permissions,
        pwd_version,
        // The identity registry will eventually provide the web
        // user's stable `UserId`. Until then, the username-as-id
        // fallback is the only stable choice.
        Some(UserId::from(format!("web:{username}"))),
    )
}

fn create_jwt(
    web_auth_config: &WebAuthConfig,
    username: &str,
    roles: Vec<String>,
    permissions: PermissionSet,
    pwd_version: u32,
    subject_id: Option<UserId>,
) -> Result<String, std::io::Error> {
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some("JWT".to_string());
    let now = Local::now();
    let iat = now.timestamp();
    let duration = web_auth_config.token_ttl_mins;
    let exp = if duration > 0 {
       (now + Duration::minutes(i64::from(duration))).timestamp()
    } else {
        (now + Duration::days(365 * 100)).timestamp() // 100 years
    };
    let claims = Claims {
        username: username.to_string(),
        iss: web_auth_config.issuer.clone(),
        iat,
        exp,
        roles,
        permissions,
        pwd_version,
        subject_id,
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
    };
    match encode(&header, &claims, &EncodingKey::from_secret(web_auth_config.secret.as_bytes())) {
        Ok(jwt) => Ok(jwt),
        Err(err) => Err(to_io_error(err))
    }
}

pub(crate) fn verify_token(token: &str, secret_key: &[u8]) -> Option<TokenData<Claims>> {
    if let Ok(token_data) = decode::<Claims>(token, &DecodingKey::from_secret(secret_key), &Validation::new(Algorithm::HS256)) {
        return Some(token_data);
    }
    None
}

fn has_role(token_data: Option<TokenData<Claims>>, role: &str) -> bool {
    if let Some(data) = token_data {
        data.claims.roles.contains(&role.to_string())
    } else {
        false
    }
}

pub fn is_admin(token_data: Option<TokenData<Claims>>) -> bool {
    has_role(token_data, ROLE_ADMIN)
}

pub fn is_api_user(token_data: Option<TokenData<Claims>>) -> bool {
    has_role(token_data, ROLE_API_USER)
}

pub fn verify_token_admin(bearer: &str, secret_key: &[u8]) -> bool {
    has_role(verify_token(bearer, secret_key), ROLE_ADMIN)
}

pub fn verify_token_api_user(bearer: &str, secret_key: &[u8]) -> bool {
    has_role(verify_token(bearer, secret_key), ROLE_API_USER)
}

/// Stable error type for the validators. A stable
/// "token-refresh-required" response lets the frontend sign out
/// without guessing. The HTTP layer returns 401 with an
/// `X-Token-Refresh: required` header for refresh-required cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// Token signature/issuer/exp invalid, malformed, or otherwise
    /// unverifiable. The frontend should sign the user out.
    InvalidToken,
    /// Token signature is valid but it carries an old or absent
    /// `permission_schema_version`. The frontend must refresh
    /// credentials to receive a token at the current schema.
    StaleSchema,
    /// Token signature is valid but it lacks a `subject_id`. The
    /// identity-registry-bound principal cannot be resolved.
    MissingSubject,
    /// Token signature is valid but the principal has the wrong
    /// role/permission for the requested endpoint.
    Forbidden,
}

impl AuthError {
    /// `true` when the frontend should request a fresh token before
    /// retrying the request. Stale-schema and missing-subject both
    /// qualify; a re-auth round-trip is required.
    pub fn is_token_refresh_required(self) -> bool {
        matches!(self, Self::StaleSchema | Self::MissingSubject)
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToken => f.write_str("token is invalid or expired"),
            Self::StaleSchema => f.write_str("token was issued for an older permission schema; refresh required"),
            Self::MissingSubject => f.write_str("token is missing a subject_id; refresh required"),
            Self::Forbidden => f.write_str("principal does not have the required role"),
        }
    }
}

/// Validate a verified token's `permission_schema_version` and
/// `subject_id`. Both must be present and current. Returns
/// [`AuthError::StaleSchema`] when the schema is below the current
/// version, and [`AuthError::MissingSubject`] when the
/// `subject_id` is `None`. A token that passes both checks is fit
/// for downstream permission and authorization checks.
fn validate_token_version(token_data: &TokenData<Claims>) -> Result<(), AuthError> {
    if token_data.claims.permission_schema_version < CURRENT_PERMISSION_SCHEMA_VERSION {
        return Err(AuthError::StaleSchema);
    }
    if token_data.claims.subject_id.is_none() {
        return Err(AuthError::MissingSubject);
    }
    Ok(())
}

fn validate_request(
    app_state: &Arc<AppState>,
    token: &str,
    verify_fn: fn(&str, &[u8]) -> bool,
) -> Result<(), AuthError> {
    let config = app_state.app_config.config.load();
    let Some(web_auth_config) = config.web_ui.as_ref().and_then(|c| c.auth.as_ref()) else {
        return Err(AuthError::InvalidToken);
    };
    let secret_key = web_auth_config.secret.as_ref();
    let token_data = verify_token(token, secret_key).ok_or(AuthError::InvalidToken)?;
    validate_token_version(&token_data)?;
    if !verify_fn(token, secret_key) {
        return Err(AuthError::Forbidden);
    }
    Ok(())
}

/// Build a stable, recognizable rejection response. Refresh-required
/// cases carry the `X-Token-Refresh: required` header so the
/// frontend can branch on it.
fn rejection_for(err: AuthError) -> axum::response::Response {
    use axum::http::StatusCode;
    let status = StatusCode::UNAUTHORIZED;
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
    match validate_request(&app_state, &token, verify_token_admin) {
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
    match validate_request(&app_state, &token, verify_token_api_user) {
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
    if let Err(err) = validate_token_version(&token_data) {
        return rejection_for(err);
    }

    if !token_data.claims.permissions.contains(permission) {
        let denied_permission = permission_to_name(permission).unwrap_or("unknown");
        warn!(
            "User '{}' denied permission '{denied_permission}'",
            token_data.claims.username
        );
        return rejection_for(AuthError::Forbidden);
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WebAuthConfig;
    use shared::model::permission::Permission;
    use shared::model::{Claims, CURRENT_PERMISSION_SCHEMA_VERSION, ROLE_ADMIN, ROLE_API_USER, UserId};

    fn test_web_auth_config() -> WebAuthConfig {
        WebAuthConfig {
            enabled: true,
            issuer: "tuliprox-test".to_string(),
            secret: "test-secret".to_string(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: None,
            t_groups: None,
        }
    }

    #[test]
    fn admin_jwt_carries_builtin_admin_subject_id_and_current_schema_version() {
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 1).expect("admin jwt");
        let secret = cfg.secret.as_bytes();
        let data = verify_token(&jwt, secret).expect("verify");
        assert_eq!(data.claims.username, "any");
        assert_eq!(data.claims.subject_id, Some(UserId::builtin_admin()));
        assert!(data.claims.roles.contains(&ROLE_ADMIN.to_string()));
        assert!(data.claims.permissions.contains(Permission::ConfigRead));
        assert!(data.claims.permissions.contains(Permission::RecordingRead));
        assert!(data.claims.permissions.contains(Permission::RecordingWrite));
        assert_eq!(data.claims.permission_schema_version, CURRENT_PERMISSION_SCHEMA_VERSION);
    }

    #[test]
    fn web_user_jwt_carries_web_namespaced_subject_id() {
        let cfg = test_web_auth_config();
        let jwt = create_jwt_web_user(
            &cfg,
            "alice",
            Permission::ConfigRead | Permission::RecordingRead,
            0,
        )
        .expect("web jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes()).expect("verify");
        assert_eq!(data.claims.username, "alice");
        assert_eq!(data.claims.subject_id, Some(UserId::from("web:alice")));
        assert!(!data.claims.roles.contains(&ROLE_ADMIN.to_string()));
        assert!(data.claims.permissions.contains(Permission::RecordingRead));
        assert!(!data.claims.permissions.contains(Permission::RecordingWrite));
    }

    #[test]
    fn api_user_jwt_carries_api_namespaced_subject_id() {
        let cfg = test_web_auth_config();
        let jwt = create_jwt_api_user(&cfg, "bob").expect("api jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes()).expect("verify");
        assert_eq!(data.claims.subject_id, Some(UserId::from("api:bob")));
        assert!(data.claims.roles.contains(&ROLE_API_USER.to_string()));
        assert!(data.claims.permissions.is_empty());
    }

    #[test]
    fn stale_schema_token_is_rejected() {
        // Manually craft a Claims payload that simulates a token issued
        // before the schema bump. The validator must mark it
        // refresh-required.
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 0).expect("admin jwt");
        let mut data = verify_token(&jwt, cfg.secret.as_bytes()).expect("verify");
        data.claims.permission_schema_version = 0; // stale
        let err = validate_token_version(&data).unwrap_err();
        assert!(matches!(err, AuthError::StaleSchema));
        assert!(err.is_token_refresh_required());
    }

    #[test]
    fn missing_subject_token_is_rejected() {
        // A token that survives signature verification but has no
        // `subject_id` must be rejected. The validator flags it as
        // refresh-required.
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 0).expect("admin jwt");
        let mut data = verify_token(&jwt, cfg.secret.as_bytes()).expect("verify");
        data.claims.subject_id = None;
        let err = validate_token_version(&data).unwrap_err();
        assert!(matches!(err, AuthError::MissingSubject));
        assert!(err.is_token_refresh_required());
    }

    #[test]
    fn forged_signature_is_invalid_token() {
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 0).expect("admin jwt");
        let mut tampered = jwt.clone();
        // Flip a character in the signature segment.
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(verify_token(&tampered, cfg.secret.as_bytes()).is_none());
    }

    #[test]
    fn owner_id_never_serialized_into_token_payloads() {
        // No request body may carry an `owner_id`. This test asserts
        // the inverse: a JWT payload never exposes an `owner_id`
        // field directly. The owner
        // identity is captured only via `subject_id` and the
        // permission set.
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 0).expect("admin jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes()).expect("verify");
        let json = serde_json::to_string(&data.claims).expect("serialize");
        assert!(!json.contains("owner_id"), "JWT must not carry an owner_id field; got: {json}");
        assert!(json.contains("subject_id"), "JWT must carry subject_id: {json}");
    }

    #[test]
    fn is_token_refresh_required_classifier() {
        assert!(AuthError::StaleSchema.is_token_refresh_required());
        assert!(AuthError::MissingSubject.is_token_refresh_required());
        assert!(!AuthError::InvalidToken.is_token_refresh_required());
        assert!(!AuthError::Forbidden.is_token_refresh_required());
    }

    #[test]
    fn claims_pwd_version_and_subject_id_round_trip() {
        // The round-trip preserves the new fields through the
        // shared `Claims` type, so a token issued today can be
        // validated and the schema info surfaced to the validator.
        let claims = Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 100,
            exp: 200,
            roles: vec!["user".to_string()],
            permissions: Permission::ConfigRead.into(),
            pwd_version: 7,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let json = serde_json::to_string(&claims).expect("serialize");
        let restored: Claims = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.subject_id, claims.subject_id);
        assert_eq!(restored.permission_schema_version, claims.permission_schema_version);
        assert_eq!(restored.pwd_version, 7);
    }
}
