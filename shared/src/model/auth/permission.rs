use crate::create_bitset;

create_bitset!(
    u16,
    Permission,
    ConfigRead,
    ConfigWrite,
    SourceRead,
    SourceWrite,
    UserRead,
    UserWrite,
    PlaylistRead,
    PlaylistWrite,
    LibraryRead,
    LibraryWrite,
    SystemRead,
    SystemWrite,
    EpgRead,
    EpgWrite
);

pub const PERM_ALL: PermissionSet = PermissionSet(0xFFFF);

pub const PERMISSION_NAMES: &[(&str, Permission)] = &[
    ("config.read", Permission::ConfigRead),
    ("config.write", Permission::ConfigWrite),
    ("source.read", Permission::SourceRead),
    ("source.write", Permission::SourceWrite),
    ("user.read", Permission::UserRead),
    ("user.write", Permission::UserWrite),
    ("playlist.read", Permission::PlaylistRead),
    ("playlist.write", Permission::PlaylistWrite),
    ("library.read", Permission::LibraryRead),
    ("library.write", Permission::LibraryWrite),
    ("system.read", Permission::SystemRead),
    ("system.write", Permission::SystemWrite),
    ("epg.read", Permission::EpgRead),
    ("epg.write", Permission::EpgWrite),
];

pub fn permission_from_name(name: &str) -> Option<Permission> {
    PERMISSION_NAMES.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

pub fn permission_to_name(perm: Permission) -> Option<&'static str> {
    PERMISSION_NAMES.iter().find(|(_, p)| *p == perm).map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_set_new_is_empty() {
        let set = PermissionSet::new();
        assert!(set.is_empty());
        assert!(!set.contains(Permission::ConfigRead));
    }

    #[test]
    fn test_permission_set_set_and_contains() {
        let mut set = PermissionSet::new();
        set.set(Permission::ConfigRead);
        assert!(set.contains(Permission::ConfigRead));
        assert!(!set.contains(Permission::ConfigWrite));
        assert!(!set.is_empty());
    }

    #[test]
    fn test_permission_set_from_variants() {
        let set = Permission::ConfigRead | Permission::SourceRead;
        assert!(set.contains(Permission::ConfigRead));
        assert!(set.contains(Permission::SourceRead));
        assert!(!set.contains(Permission::ConfigWrite));
    }

    #[test]
    fn test_permission_set_union() {
        let mut a: PermissionSet = Permission::ConfigRead.into();
        let b: PermissionSet = Permission::SourceRead.into();
        a.union(b);
        assert!(a.contains(Permission::ConfigRead));
        assert!(a.contains(Permission::SourceRead));
    }

    #[test]
    fn test_permission_set_bitor() {
        let a: PermissionSet = Permission::ConfigRead.into();
        let b: PermissionSet = Permission::SourceRead.into();
        let c = a | b;
        assert!(c.contains(Permission::ConfigRead));
        assert!(c.contains(Permission::SourceRead));
    }

    #[test]
    fn test_permission_set_unset() {
        let mut set = Permission::ConfigRead | Permission::ConfigWrite;
        set.unset(Permission::ConfigRead);
        assert!(!set.contains(Permission::ConfigRead));
        assert!(set.contains(Permission::ConfigWrite));
    }

    #[test]
    fn test_perm_all_contains_every_permission() {
        assert!(PERM_ALL.contains(Permission::ConfigRead));
        assert!(PERM_ALL.contains(Permission::ConfigWrite));
        assert!(PERM_ALL.contains(Permission::EpgRead));
        assert!(PERM_ALL.contains(Permission::EpgWrite));
    }

    #[test]
    fn test_permission_from_name() {
        assert_eq!(permission_from_name("config.read"), Some(Permission::ConfigRead));
        assert_eq!(permission_from_name("source.write"), Some(Permission::SourceWrite));
        assert_eq!(permission_from_name("nonexistent"), None);
        assert_eq!(permission_from_name(""), None);
    }

    #[test]
    fn test_permission_to_name() {
        assert_eq!(permission_to_name(Permission::ConfigRead), Some("config.read"));
        assert_eq!(permission_to_name(Permission::EpgWrite), Some("epg.write"));
    }

    #[test]
    fn test_permission_set_is_subset_of() {
        let small: PermissionSet = Permission::ConfigRead.into();
        let large = Permission::ConfigRead | Permission::SourceRead;
        assert!(small.is_subset_of(&large));
        assert!(!large.is_subset_of(&small));
    }

    #[test]
    fn test_permission_set_serde_roundtrip() {
        let set = Permission::ConfigRead | Permission::SourceWrite;
        let json = serde_json::to_string(&set).expect("serialize failed");
        let deserialized: PermissionSet = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(set, deserialized);
    }

    #[test]
    fn test_permission_set_default_is_zero() {
        let set: PermissionSet = Default::default();
        assert!(set.is_empty());
        assert_eq!(set.0, 0);
    }
}
