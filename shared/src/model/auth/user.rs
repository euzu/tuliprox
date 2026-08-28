use super::{super::identity_registry::UserId, permission::PermissionSet};
use zeroize::Zeroize;

pub const TOKEN_NO_AUTH: &str = "authorized";

pub const ROLE_ADMIN: &str = "ADMIN";
pub const ROLE_API_USER: &str = "API_USER";

/// Current permission schema version. Bump this constant when the
/// `Permission` enum or the permission bit layout changes. Tokens
/// issued before a bump fail closed at the validator with a stable
/// "token refresh required" response so clients re-authenticate
/// before the new permission bits can leak through.
///
/// Bumped to `2` for the `download.read/write -> recording.read/write`
/// migration. Old tokens still decode the legacy bits (rollback compat),
/// but a fresh token issued by the authenticator carries the new bits
/// and the `download.*` bits have been cleared by
/// [`crate::model::permission::migrate_permissions`].
pub const CURRENT_PERMISSION_SCHEMA_VERSION: u16 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Claims {
    pub username: String,
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
    pub roles: Vec<String>,
    #[serde(default)]
    pub permissions: PermissionSet,
    #[serde(default)]
    pub pwd_version: u32,
    /// Stable subject identifier for the principal. `None` for
    /// tokens issued before subject ids were introduced; tokens
    /// missing this field are rejected by the validators. Built-in
    /// admin tokens carry the reserved `builtin:admin` subject;
    /// web/API tokens carry the registry-allocated `UserId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<UserId>,
    /// Version of the permission schema at token issuance. Tokens
    /// carrying a value below
    /// [`CURRENT_PERMISSION_SCHEMA_VERSION`] are rejected with a
    /// token-refresh-required response.
    #[serde(default)]
    pub permission_schema_version: u16,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WebUiUserDto {
    pub username: String,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RbacGroupDto {
    pub name: String,
    pub permissions: Vec<String>,
    pub builtin: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserCredential {
    pub username: String,
    pub password: String,
}

impl UserCredential {
    pub fn zeroize(&mut self) { self.password.zeroize(); }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq, Default)]
pub struct TokenResponse {
    pub token: String,
    pub username: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::auth::permission::Permission;

    #[test]
    fn test_claims_deserialize_without_permissions_and_pwd_version() {
        // Simulate an old JWT payload that doesn't have permissions or pwd_version
        let json = r#"{
            "username": "admin",
            "iss": "tuliprox",
            "iat": 1000000,
            "exp": 2000000,
            "roles": ["admin"]
        }"#;
        let claims: Claims = serde_json::from_str(json).expect("deserialize failed");
        assert_eq!(claims.username, "admin");
        assert_eq!(claims.roles, vec!["admin"]);
        assert!(claims.permissions.is_empty());
        assert_eq!(claims.pwd_version, 0);
    }

    #[test]
    fn test_claims_deserialize_with_permissions_and_pwd_version() {
        let json = r#"{
            "username": "alice",
            "iss": "tuliprox",
            "iat": 1000000,
            "exp": 2000000,
            "roles": ["user"],
            "permissions": 5,
            "pwd_version": 42
        }"#;
        let claims: Claims = serde_json::from_str(json).expect("deserialize failed");
        assert_eq!(claims.username, "alice");
        assert_eq!(claims.permissions.0, 5);
        assert_eq!(claims.pwd_version, 42);
        // Old-format tokens carry no subject_id or schema version; the
        // validators must reject them at the boundary.
        assert!(claims.subject_id.is_none());
        assert_eq!(claims.permission_schema_version, 0);
    }

    #[test]
    fn test_claims_serde_roundtrip() {
        let claims = Claims {
            username: "bob".to_string(),
            iss: "test".to_string(),
            iat: 100,
            exp: 200,
            roles: vec!["user".to_string()],
            permissions: Permission::ConfigRead | Permission::SourceRead,
            pwd_version: 99,
            subject_id: Some(UserId::from("web:bob-uuid")),
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let json = serde_json::to_string(&claims).expect("serialize failed");
        let deserialized: Claims = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(deserialized.username, "bob");
        assert_eq!(deserialized.permissions, claims.permissions);
        assert_eq!(deserialized.pwd_version, 99);
        assert_eq!(deserialized.subject_id, claims.subject_id);
        assert_eq!(deserialized.permission_schema_version, CURRENT_PERMISSION_SCHEMA_VERSION);
    }

    #[test]
    fn test_claims_subject_id_is_optional_in_json() {
        // Backward compat: an old payload without `subject_id` or
        // `permission_schema_version` deserializes without error and
        // carries the default values. The validator is the gate.
        let json = r#"{
            "username": "carol",
            "iss": "tuliprox",
            "iat": 1,
            "exp": 2,
            "roles": []
        }"#;
        let claims: Claims = serde_json::from_str(json).expect("deserialize");
        assert!(claims.subject_id.is_none());
        assert_eq!(claims.permission_schema_version, 0);
    }

    #[test]
    fn test_claims_serialize_skips_none_subject_id() {
        // A web token without a subject_id is unusual but allowed for
        // backward compat. The serialization must not emit a key
        // for `null` — it would mislead clients about the principal.
        let claims = Claims {
            username: "anon".to_string(),
            iss: "tuliprox".to_string(),
            iat: 1,
            exp: 2,
            roles: vec![],
            permissions: PermissionSet::new(),
            pwd_version: 0,
            subject_id: None,
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let json = serde_json::to_string(&claims).expect("serialize");
        assert!(!json.contains("subject_id"), "no subject_id key in: {json}");
    }

    #[test]
    fn current_permission_schema_version_is_at_least_two() {
        // The schema version was bumped by +1 to invalidate tokens
        // carrying only `download.*` permission bits. Old tokens must
        // fail closed at the validator.
        assert!(CURRENT_PERMISSION_SCHEMA_VERSION >= 2, "schema must be bumped for download->recording migration");
    }
}
