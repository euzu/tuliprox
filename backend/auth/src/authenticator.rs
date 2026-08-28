use chrono::{Duration, Local};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation};
use log::warn;
use shared::{
    error::to_io_error,
    model::{
        permission::{PermissionSet, PERM_ALL},
        Claims, Role, RoleSet, UserId, CURRENT_PERMISSION_SCHEMA_VERSION,
    },
};
use tuliprox_core::model::WebAuthConfig;

/// Hard ceiling on a minted token's lifetime.
///
/// A token this server issues is a bearer credential with no revocation list
/// behind it, so its lifetime is the entire blast radius of a leak.
pub const MAX_TOKEN_TTL_MINS: u32 = 60 * 24 * 30; // 30 days

/// Lifetime used when the config asks for an unbounded token.
///
/// `token_ttl_mins: 0` used to mean "expire in 100 years", which is a
/// permanent credential written as if it were a configuration convenience.
pub const DEFAULT_TOKEN_TTL_MINS: u32 = 60 * 24; // 24 hours

/// Clamp a configured TTL into something a bearer token may actually carry.
fn effective_ttl_mins(configured: u32) -> u32 {
    if configured == 0 {
        warn!(
            "web_ui.auth.token_ttl_mins is 0; issuing {DEFAULT_TOKEN_TTL_MINS}-minute tokens instead of \
             non-expiring ones. Set an explicit value to silence this."
        );
        DEFAULT_TOKEN_TTL_MINS
    } else if configured > MAX_TOKEN_TTL_MINS {
        warn!("web_ui.auth.token_ttl_mins of {configured} exceeds the {MAX_TOKEN_TTL_MINS} minute ceiling; clamped.");
        MAX_TOKEN_TTL_MINS
    } else {
        configured
    }
}

pub fn create_jwt_admin(
    web_auth_config: &WebAuthConfig,
    username: &str,
    pwd_version: u32,
) -> Result<String, std::io::Error> {
    create_jwt(web_auth_config, username, RoleSet::ADMIN, PERM_ALL, pwd_version, Some(UserId::builtin_admin()))
}

/// `subject_id` comes from the identity registry.
///
/// It used to be `format!("api:{username}")`, which made the subject a
/// function of the display name: renaming a user reassigned every recording
/// they owned to a principal that did not exist, and two deployments that
/// happened to share a username shared an identity.
pub fn create_jwt_api_user(
    web_auth_config: &WebAuthConfig,
    username: &str,
    subject_id: UserId,
) -> Result<String, std::io::Error> {
    create_jwt(web_auth_config, username, RoleSet::API_USER, PermissionSet::new(), 0, Some(subject_id))
}

/// `subject_id` comes from the identity registry. See
/// [`create_jwt_api_user`] for why it is no longer derived from the username.
pub fn create_jwt_web_user(
    web_auth_config: &WebAuthConfig,
    username: &str,
    permissions: PermissionSet,
    pwd_version: u32,
    subject_id: UserId,
) -> Result<String, std::io::Error> {
    create_jwt(web_auth_config, username, RoleSet::new(), permissions, pwd_version, Some(subject_id))
}

fn create_jwt(
    web_auth_config: &WebAuthConfig,
    username: &str,
    roles: RoleSet,
    permissions: PermissionSet,
    pwd_version: u32,
    subject_id: Option<UserId>,
) -> Result<String, std::io::Error> {
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some("JWT".to_string());
    let now = Local::now();
    let iat = now.timestamp();
    let exp = (now + Duration::minutes(i64::from(effective_ttl_mins(web_auth_config.token_ttl_mins)))).timestamp();
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
        Err(err) => Err(to_io_error(err)),
    }
}

/// Verify a token's signature, expiry **and issuer**.
///
/// `iss` was written into every token and then never looked at:
/// `Validation::new` checks `exp` and nothing else. A token minted by a
/// different deployment that happened to share the secret verified cleanly.
pub fn verify_token(token: &str, secret_key: &[u8], issuer: &str) -> Option<TokenData<Claims>> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[issuer]);
    decode::<Claims>(token, &DecodingKey::from_secret(secret_key), &validation).ok()
}

/// The material needed to verify a token, for call sites that hold it across
/// an await or a task boundary and so cannot borrow the live config.
///
/// The WebSocket paths used to carry a bare `Vec<u8>` secret, which is exactly
/// why they could not check the issuer.
#[derive(Debug, Clone)]
pub struct TokenVerifier {
    secret: Vec<u8>,
    issuer: String,
}

impl TokenVerifier {
    pub fn new(secret: impl Into<Vec<u8>>, issuer: impl Into<String>) -> Self {
        Self { secret: secret.into(), issuer: issuer.into() }
    }

    pub fn from_config(config: &WebAuthConfig) -> Self {
        Self::new(config.secret.as_bytes(), config.issuer.clone())
    }

    pub fn verify(&self, token: &str) -> Option<TokenData<Claims>> {
        verify_token(token, &self.secret, &self.issuer)
    }
}

fn has_role(token_data: Option<TokenData<Claims>>, role: Role) -> bool {
    token_data.is_some_and(|data| data.claims.roles.contains(role))
}

pub fn is_admin(token_data: Option<TokenData<Claims>>) -> bool {
    has_role(token_data, Role::Admin)
}

pub fn is_api_user(token_data: Option<TokenData<Claims>>) -> bool {
    has_role(token_data, Role::ApiUser)
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
    /// Token signature is valid but it was minted against a password that has
    /// since changed - or it carries no password version at all. Either way
    /// the principal must authenticate again; a refresh cannot help, because
    /// the refresh endpoint applies the same check.
    PasswordChanged,
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
            Self::PasswordChanged => f.write_str("token was issued for a different password; sign in again"),
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
pub fn validate_token_claims(claims: &Claims) -> Result<(), AuthError> {
    if claims.permission_schema_version < CURRENT_PERMISSION_SCHEMA_VERSION {
        return Err(AuthError::StaleSchema);
    }
    if claims.subject_id.is_none() {
        return Err(AuthError::MissingSubject);
    }
    Ok(())
}

/// Validate a token's `pwd_version` against the principal's current one.
///
/// `pwd_version` was minted into every web token and then checked in exactly
/// one place - the refresh endpoint - which meant changing a password did not
/// invalidate any live token on any guarded route. Combined with the old
/// "0 means non-expiring" TTL, a leaked token was a permanent credential.
///
/// A `pwd_version` of `0` is rejected rather than waved through. The refresh
/// endpoint used to treat `0` as "skip the check", which made the check
/// bypassable by anything that could mint or replay a zero-versioned token.
///
/// Call this only for principals that actually have a password on file - the
/// web users in `web_ui.auth`. Proxy API users authenticate against
/// `api_proxy.yml` credentials and carry no password version.
pub fn validate_password_version(claims: &Claims, current_pwd_version: u32) -> Result<(), AuthError> {
    if claims.pwd_version == 0 || claims.pwd_version != current_pwd_version {
        return Err(AuthError::PasswordChanged);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{permission::Permission, Claims, Role, RoleSet, UserId, CURRENT_PERMISSION_SCHEMA_VERSION};
    use tuliprox_core::model::WebAuthConfig;

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
        let data = verify_token(&jwt, secret, &cfg.issuer).expect("verify");
        assert_eq!(data.claims.username, "any");
        assert_eq!(data.claims.subject_id, Some(UserId::builtin_admin()));
        assert!(data.claims.is_admin());
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
            UserId::from("web:alice-uuid"),
        )
        .expect("web jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        assert_eq!(data.claims.username, "alice");
        assert_eq!(data.claims.subject_id, Some(UserId::from("web:alice-uuid")));
        assert!(!data.claims.is_admin());
        assert!(data.claims.permissions.contains(Permission::RecordingRead));
        assert!(!data.claims.permissions.contains(Permission::RecordingWrite));
    }

    #[test]
    fn api_user_jwt_carries_api_namespaced_subject_id() {
        let cfg = test_web_auth_config();
        let jwt = create_jwt_api_user(&cfg, "bob", UserId::from("api:bob-uuid")).expect("api jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        assert_eq!(data.claims.subject_id, Some(UserId::from("api:bob-uuid")));
        assert!(data.claims.is_api_user());
        assert!(data.claims.permissions.is_empty());
    }

    #[test]
    fn stale_schema_token_is_rejected() {
        // Manually craft a Claims payload that simulates a token issued
        // before the schema bump. The validator must mark it
        // refresh-required.
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 0).expect("admin jwt");
        let mut data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        data.claims.permission_schema_version = 0; // stale
        let err = validate_token_claims(&data.claims).unwrap_err();
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
        let mut data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        data.claims.subject_id = None;
        let err = validate_token_claims(&data.claims).unwrap_err();
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
        assert!(verify_token(&tampered, cfg.secret.as_bytes(), &cfg.issuer).is_none());
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
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        let json = serde_json::to_string(&data.claims).expect("serialize");
        assert!(!json.contains("owner_id"), "JWT must not carry an owner_id field; got: {json}");
        assert!(json.contains("subject_id"), "JWT must carry subject_id: {json}");
    }

    #[test]
    fn token_from_another_issuer_does_not_verify() {
        // `iss` was minted and never checked. It is checked now.
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 1).expect("admin jwt");
        assert!(verify_token(&jwt, cfg.secret.as_bytes(), "tuliprox-test").is_some());
        assert!(verify_token(&jwt, cfg.secret.as_bytes(), "someone-else").is_none());
    }

    #[test]
    fn zero_ttl_config_no_longer_mints_a_century_long_token() {
        let mut cfg = test_web_auth_config();
        cfg.token_ttl_mins = 0;
        let jwt = create_jwt_admin(&cfg, "any", 1).expect("admin jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        let lifetime_mins = (data.claims.exp - data.claims.iat) / 60;
        assert_eq!(lifetime_mins, i64::from(DEFAULT_TOKEN_TTL_MINS));
    }

    #[test]
    fn oversized_ttl_is_clamped_to_the_ceiling() {
        let mut cfg = test_web_auth_config();
        cfg.token_ttl_mins = MAX_TOKEN_TTL_MINS * 10;
        let jwt = create_jwt_admin(&cfg, "any", 1).expect("admin jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        let lifetime_mins = (data.claims.exp - data.claims.iat) / 60;
        assert_eq!(lifetime_mins, i64::from(MAX_TOKEN_TTL_MINS));
    }

    #[test]
    fn password_version_mismatch_is_rejected() {
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 7).expect("admin jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        assert!(validate_password_version(&data.claims, 7).is_ok());
        assert_eq!(validate_password_version(&data.claims, 8), Err(AuthError::PasswordChanged));
    }

    #[test]
    fn zero_password_version_is_rejected_rather_than_waved_through() {
        // The refresh endpoint used to read `pwd_version == 0` as "skip the
        // check", which made the check bypassable.
        let cfg = test_web_auth_config();
        let jwt = create_jwt_admin(&cfg, "any", 0).expect("admin jwt");
        let data = verify_token(&jwt, cfg.secret.as_bytes(), &cfg.issuer).expect("verify");
        assert_eq!(validate_password_version(&data.claims, 0), Err(AuthError::PasswordChanged));
        assert_eq!(validate_password_version(&data.claims, 5), Err(AuthError::PasswordChanged));
    }

    #[test]
    fn is_token_refresh_required_classifier() {
        assert!(AuthError::StaleSchema.is_token_refresh_required());
        assert!(AuthError::MissingSubject.is_token_refresh_required());
        assert!(!AuthError::InvalidToken.is_token_refresh_required());
        assert!(!AuthError::Forbidden.is_token_refresh_required());
        // A changed password cannot be fixed by a refresh - the refresh
        // endpoint applies the same check. The client must sign in again.
        assert!(!AuthError::PasswordChanged.is_token_refresh_required());
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
            roles: RoleSet::from(Role::Admin),
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
