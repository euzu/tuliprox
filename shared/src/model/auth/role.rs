//! Roles as a static bitset.
//!
//! Roles used to be a `Vec<String>` on [`Claims`](super::Claims), compared with
//! `==` in five places and `eq_ignore_ascii_case` in a sixth. That is one
//! allocation per mint, a string comparison per check, and two different
//! answers to the same question depending on which call site you landed on.
//!
//! The set is a `u8` bitfield built by the same `create_bitset!` macro that
//! backs `PermissionSet`, so a role check is a `test` instruction. The JWT wire
//! format is unchanged - [`role_names`] serialises the set back to the legacy
//! `["ADMIN"]` string array so tokens minted before this change still
//! deserialise, and clients that read the payload see what they always saw.

use crate::create_bitset;

create_bitset!(u8, Role, Admin, ApiUser);

pub const ROLE_ADMIN: &str = "ADMIN";
pub const ROLE_API_USER: &str = "API_USER";

impl Role {
    /// The wire name. This is what lands in the JWT payload.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => ROLE_ADMIN,
            Self::ApiUser => ROLE_API_USER,
        }
    }

    /// Parse a wire name. Case-insensitive: the mint side has always written
    /// the uppercase constants, but one historical call site compared
    /// case-insensitively, so accepting both is the superset that cannot
    /// regress an existing token.
    #[inline]
    pub fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case(ROLE_ADMIN) {
            Some(Self::Admin)
        } else if name.eq_ignore_ascii_case(ROLE_API_USER) {
            Some(Self::ApiUser)
        } else {
            None
        }
    }
}

impl RoleSet {
    pub const ADMIN: Self = Self(1 << (Role::Admin as u8));
    pub const API_USER: Self = Self(1 << (Role::ApiUser as u8));

    /// Every role in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Role> {
        [Role::Admin, Role::ApiUser].into_iter().filter(move |role| self.contains(*role))
    }

    /// The wire names, in declaration order.
    pub fn names(self) -> Vec<&'static str> { self.iter().map(Role::as_str).collect() }

    /// Build from wire names. Unknown names are dropped rather than rejected:
    /// a token minted by a newer version carrying a role this build does not
    /// know must fail closed (no role) rather than fail to parse.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = Self::new();
        for name in names {
            if let Some(role) = Role::from_name(name.as_ref()) {
                set.set(role);
            }
        }
        set
    }
}

/// Serialises a [`RoleSet`] as the legacy `["ADMIN"]` string array.
///
/// Applied to the `Claims::roles` field so the JWT payload is byte-compatible
/// with tokens minted before roles became a bitset.
pub mod role_names {
    use super::RoleSet;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(roles: &RoleSet, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(roles.names())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RoleSet, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        Ok(RoleSet::from_names(names))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_name_round_trip() {
        assert_eq!(Role::from_name("ADMIN"), Some(Role::Admin));
        assert_eq!(Role::from_name("admin"), Some(Role::Admin));
        assert_eq!(Role::from_name("API_USER"), Some(Role::ApiUser));
        assert_eq!(Role::from_name("api_user"), Some(Role::ApiUser));
        assert_eq!(Role::from_name("nope"), None);
        assert_eq!(Role::Admin.as_str(), ROLE_ADMIN);
        assert_eq!(Role::ApiUser.as_str(), ROLE_API_USER);
    }

    #[test]
    fn set_from_names_drops_unknown_roles() {
        let set = RoleSet::from_names(["ADMIN", "SOMETHING_NEW"]);
        assert!(set.contains(Role::Admin));
        assert!(!set.contains(Role::ApiUser));
        assert_eq!(set.names(), vec![ROLE_ADMIN]);
    }

    #[test]
    fn empty_set_has_no_roles() {
        let set = RoleSet::new();
        assert!(set.is_empty());
        assert!(!set.contains(Role::Admin));
        assert!(set.names().is_empty());
    }

    #[test]
    fn wire_format_is_the_legacy_string_array() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wrapper {
            #[serde(with = "role_names")]
            roles: RoleSet,
        }
        let wrapper = Wrapper { roles: RoleSet::ADMIN };
        let json = serde_json::to_string(&wrapper).expect("serialize");
        assert_eq!(json, r#"{"roles":["ADMIN"]}"#);
        let back: Wrapper = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, wrapper);
    }
}
