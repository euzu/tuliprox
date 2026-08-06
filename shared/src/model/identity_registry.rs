//! Identity registry types for stable subject identities.
//!
//! Phase 0 + Phase 1 introduce the [`UserId`] newtype used by recording
//! metadata, scope strings, and the per-user quota config map. The full
//! registry (with on-disk mapping, bootstrap, and rename migration) is added
//! in Phase 2 (Task 11).

use std::fmt;

/// Stable subject identifier (UUID v4 hex). Used for web users, API users,
/// and the reserved built-in admin subject. Namespaces are string prefixes on
/// the wrapped `String` (`web:<uuid>`, `api:<uuid>`, `builtin:admin`) to keep
/// the representation copy-free in the public DTO layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl UserId {
    pub const BUILTIN_ADMIN_NAMESPACE: &'static str = "builtin:admin";
    pub const WEB_NAMESPACE: &'static str = "web:";
    pub const API_NAMESPACE: &'static str = "api:";

    /// Reserved subject ID for the built-in administrator. Stable across
    /// restarts; never generated dynamically.
    pub fn builtin_admin() -> Self { Self(Self::BUILTIN_ADMIN_NAMESPACE.to_string()) }

    pub fn is_builtin_admin(&self) -> bool { self.0 == Self::BUILTIN_ADMIN_NAMESPACE }

    pub fn is_web(&self) -> bool { self.0.starts_with(Self::WEB_NAMESPACE) }

    pub fn is_api(&self) -> bool { self.0.starts_with(Self::API_NAMESPACE) }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl From<String> for UserId {
    fn from(s: String) -> Self { Self(s) }
}

impl From<&str> for UserId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl AsRef<str> for UserId {
    fn as_ref(&self) -> &str { &self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_admin_subject_id_is_stable() {
        assert_eq!(UserId::builtin_admin().0, "builtin:admin");
        assert!(UserId::builtin_admin().is_builtin_admin());
    }

    #[test]
    fn web_and_api_namespaces_are_detected() {
        assert!(UserId::from("web:abc").is_web());
        assert!(!UserId::from("web:abc").is_api());
        assert!(UserId::from("api:xyz").is_api());
        assert!(!UserId::from("api:xyz").is_web());
        assert!(!UserId::from("builtin:admin").is_web());
        assert!(!UserId::from("builtin:admin").is_api());
    }

    #[test]
    fn display_and_as_ref_return_inner_string() {
        let id = UserId::from("web:abc");
        assert_eq!(format!("{id}"), "web:abc");
        assert_eq!(id.as_ref(), "web:abc");
    }
}
