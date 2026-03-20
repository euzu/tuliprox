use super::permission::PermissionSet;
use zeroize::Zeroize;

pub const TOKEN_NO_AUTH: &str = "authorized";

pub const ROLE_ADMIN: &str = "ADMIN";
pub const ROLE_API_USER: &str = "API_USER";

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
        };
        let json = serde_json::to_string(&claims).expect("serialize failed");
        let deserialized: Claims = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(deserialized.username, "bob");
        assert_eq!(deserialized.permissions, claims.permissions);
        assert_eq!(deserialized.pwd_version, 99);
    }
}
