//! Stable subject identities.
//!
//! The [`UserId`] newtype is used by token claims, token revocations,
//! recording metadata and the per-user quota config map. It is derived from
//! the configured username, so it needs no stored mapping.

use std::fmt;

/// Error returned when a string cannot be promoted to a `UserId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUserId(String);

impl fmt::Display for InvalidUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "invalid user id: {}", self.0) }
}

impl std::error::Error for InvalidUserId {}

/// Subject identifier for web users, API users and the reserved built-in
/// admin. Namespaces are string prefixes on the wrapped `String`
/// (`web:<username>`, `api:<username>`, `builtin:admin`): a web user and an API
/// user may share a username and are still different principals.
///
/// Deserialization validates the input via `UserId::validate` so arbitrary,
/// path-like, or reserved strings cannot enter authorization or directory
/// construction through persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
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

    /// The subject of a configured web user. Pass the username as configured,
    /// not as typed at login: sign-in matches web users case-insensitively.
    pub fn web(username: &str) -> Self { Self::namespaced(Self::WEB_NAMESPACE, username) }

    /// The subject of a proxy API user.
    pub fn api(username: &str) -> Self { Self::namespaced(Self::API_NAMESPACE, username) }

    /// `namespace` followed by the trimmed username, with the few characters
    /// [`UserId::validate`] refuses percent-encoded. `%` is encoded too, so two
    /// different usernames can never produce the same id.
    fn namespaced(namespace: &str, username: &str) -> Self {
        let username = username.trim();
        let mut id = String::with_capacity(namespace.len() + username.len());
        id.push_str(namespace);
        for ch in username.chars() {
            match ch {
                '%' => id.push_str("%25"),
                '/' => id.push_str("%2F"),
                '\\' => id.push_str("%5C"),
                '\0' => id.push_str("%00"),
                other => id.push(other),
            }
        }
        Self(id)
    }

    pub fn is_web(&self) -> bool { self.0.starts_with(Self::WEB_NAMESPACE) }

    pub fn is_api(&self) -> bool { self.0.starts_with(Self::API_NAMESPACE) }

    /// Validate a string before promoting it to a `UserId`. Rejects empty
    /// strings, NUL bytes, path separators, traversal components, and
    /// any string that does not match one of the known namespaces.
    pub fn validate(s: &str) -> Result<(), InvalidUserId> {
        if s.is_empty() || s.contains('\0') {
            return Err(InvalidUserId(s.to_string()));
        }
        // A `UserId` is a single opaque token — never a path component.
        if s.contains('/') || s.contains('\\') {
            return Err(InvalidUserId(s.to_string()));
        }
        if s == "." || s == ".." {
            return Err(InvalidUserId(s.to_string()));
        }
        if s == Self::BUILTIN_ADMIN_NAMESPACE
            || s.starts_with(Self::WEB_NAMESPACE)
            || s.starts_with(Self::API_NAMESPACE)
        {
            Ok(())
        } else {
            Err(InvalidUserId(s.to_string()))
        }
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

// `UserId` exposes both infallible `From` conversions (for internal
// callers and string literals that the type system can guarantee are
// valid) and `UserId::validate` (used by the serde `Deserialize` impl
// below to gate external / persisted input). The blanket `TryFrom`
// derived from `From` therefore does not run `validate`; callers
// taking untrusted input must round-trip through the `Deserialize`
// path or call `UserId::validate` directly.

impl From<String> for UserId {
    fn from(s: String) -> Self { Self(s) }
}

impl From<&str> for UserId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl AsRef<str> for UserId {
    fn as_ref(&self) -> &str { &self.0 }
}

impl<'de> serde::Deserialize<'de> for UserId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        // Validate directly rather than going through `try_from`: the
        // blanket `TryFrom` derived from `From<String>` would skip the
        // namespace/path checks. The deserialize boundary is the one
        // place that must reject arbitrary persisted strings.
        Self::validate(&raw).map_err(serde::de::Error::custom)?;
        Ok(Self(raw))
    }
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
    fn a_subject_is_the_namespaced_username() {
        assert_eq!(UserId::web("alice"), UserId::from("web:alice"));
        assert_eq!(UserId::api(" alice "), UserId::from("api:alice"));
        assert_ne!(UserId::web("alice"), UserId::api("alice"), "same name, different principals");
    }

    #[test]
    fn a_derived_subject_always_validates_and_stays_unambiguous() {
        for name in ["a/b", "a\\b", "a\0b", "50%", "..", "."] {
            let id = UserId::web(name);
            assert!(UserId::validate(&id.0).is_ok(), "{name:?} -> {id}");
        }
        assert_eq!(UserId::web("a/b").0, "web:a%2Fb");
        assert_ne!(UserId::web("a/b"), UserId::web("a%2Fb"), "an encoded name is not the same user");
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

    #[test]
    fn validate_rejects_path_like_and_unknown_namespaces() {
        // Valid namespaces pass.
        assert!(UserId::validate("web:abc").is_ok());
        assert!(UserId::validate("api:xyz").is_ok());
        assert!(UserId::validate("builtin:admin").is_ok());
        // Empty, NUL, separators, traversal, and unknown namespaces fail.
        assert!(UserId::validate("").is_err());
        assert!(UserId::validate("web:\0").is_err());
        assert!(UserId::validate("web:a/b").is_err());
        assert!(UserId::validate("web:a\\b").is_err());
        assert!(UserId::validate(".").is_err());
        assert!(UserId::validate("..").is_err());
        assert!(UserId::validate("admin").is_err());
        assert!(UserId::validate("plain-uuid-without-namespace").is_err());
    }

    #[test]
    fn deserialize_rejects_invalid_user_ids() {
        // serde rejects path-like and unknown-namespace strings at the
        // boundary so they cannot reach authorization or directory
        // construction.
        let bad = serde_json::from_str::<UserId>("\"../escape\"");
        assert!(bad.is_err());
        let bad = serde_json::from_str::<UserId>("\"plain\"");
        assert!(bad.is_err());
        // Valid names still deserialize.
        let ok: UserId = serde_json::from_str("\"web:abc\"").expect("valid");
        assert!(ok.is_web());
    }
}
